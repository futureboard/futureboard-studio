#pragma once
// Internal (in-crate) header for the DAUx VST3 bridge.
//
// Holds the shared value types + the main SphereDauxVst3Processor struct so the
// platform editor translation units (editorplatform/windows/editor_embed.cpp,
// editorplatform/macos/editor_mac.mm, editor_linux.cpp) can access processor
// state without re-declaring it. vst3_processor.cpp owns the cross-platform VST3
// core; platform TUs own their windowing code.
//
// NOTE: this is a private header — never installed, never included outside the
// vst3bridge sources.

#include "sphere_daux_vst3_processor.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <vector>

#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/gui/iplugviewcontentscalesupport.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmidicontrollers.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"
#include "public.sdk/source/vst/hosting/module.h"

#include "editor_windows.hpp"

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#endif

// ── Shared diagnostics helpers ───────────────────────────────────────────────
// Defined inline so both vst3_processor.cpp and the platform editor TUs share a
// single implementation. Small, non-realtime-sensitive (used off the audio
// hot path or behind cached debug flags).

inline thread_local std::string g_last_error;

inline void set_last_error(std::string value) { g_last_error = std::move(value); }

inline bool daux_vst3_bus_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_PLUGIN_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_PLUGIN_BRIDGE_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_VST3_BUS_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_FORENSIC_TRACE") != nullptr;
  return enabled;
}

inline std::string vst3_tchar_to_utf8(const Steinberg::Vst::TChar *value) {
  if (!value)
    return {};
  std::string out;
  for (int i = 0; i < 128 && value[i] != 0; ++i) {
    const auto ch = static_cast<unsigned int>(value[i]);
    out.push_back(ch < 0x80 ? static_cast<char>(ch) : '?');
  }
  return out;
}

inline const char *vst3_bus_type_name(Steinberg::Vst::BusType type) {
  switch (type) {
  case Steinberg::Vst::kMain:
    return "main";
  case Steinberg::Vst::kAux:
    return "aux";
  default:
    return "unknown";
  }
}

inline bool daux_vst3_midi_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_FORENSIC_TRACE") != nullptr ||
      std::getenv("FUTUREBOARD_VST3_MIDI_DEBUG") != nullptr;
  return enabled;
}

// Forward declaration so ComponentHandlerImpl can hold a back-pointer.
struct SphereDauxVst3Processor;

#if defined(_WIN32)
// IPlugFrame implementation — defined in editorplatform/windows/editor_embed.cpp.
// Forward-declared so the processor can hold a raw pointer to its editor frame.
class PluginEditorFrame;
// IPlugFrame for the Rust-owned view host. Separate from `PluginEditorFrame`
// because it owns no window: a resize request is recorded for the host to read
// and act on, never applied here.
class HostViewFrame;
#endif
#if defined(__APPLE__)
class MacPluginEditorFrame;
#endif

struct Vst3BusAudioStats {
  double peak_l{0.0};
  double peak_r{0.0};
  double rms_l{0.0};
  double rms_r{0.0};
  int first_non_zero{-1};
  bool ptr_valid{false};
};

// ── Parameter helper types (stack/member allocated — zero heap use) ──────────

/// One pending parameter change (id + normalized value 0..1).
struct PendingParam {
  Steinberg::Vst::ParamID id{0};
  Steinberg::Vst::ParamValue value{0.0};
};

/// Minimal IParamValueQueue: single value at sample-offset 0.
/// No heap allocation — lives inside SimpleParamChanges::queues[].
struct SimpleParamValueQueue final : Steinberg::Vst::IParamValueQueue {
  Steinberg::Vst::ParamID param_id{0};
  Steinberg::Vst::ParamValue param_value{0.0};

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (std::memcmp(iid, Steinberg::Vst::IParamValueQueue::iid,
                    sizeof(Steinberg::TUID)) == 0) {
      *obj = this;
      return Steinberg::kResultOk;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }
  Steinberg::uint32 PLUGIN_API addRef() override { return 1; }
  Steinberg::uint32 PLUGIN_API release() override { return 1; }

  Steinberg::Vst::ParamID PLUGIN_API getParameterId() override {
    return param_id;
  }
  Steinberg::int32 PLUGIN_API getPointCount() override { return 1; }

  Steinberg::tresult PLUGIN_API
  getPoint(Steinberg::int32 index, Steinberg::int32 &sample_offset,
           Steinberg::Vst::ParamValue &value) override {
    if (index != 0)
      return Steinberg::kResultFalse;
    sample_offset = 0;
    value = param_value;
    return Steinberg::kResultOk;
  }
  Steinberg::tresult PLUGIN_API addPoint(Steinberg::int32,
                                         Steinberg::Vst::ParamValue v,
                                         Steinberg::int32 &idx) override {
    param_value = v;
    idx = 0;
    return Steinberg::kResultOk;
  }
};

/// Minimal IParameterChanges: fixed capacity, no dynamic allocation.
/// Reused each process call — reset() clears the count without freeing memory.
struct SimpleParamChanges final : Steinberg::Vst::IParameterChanges {
  static constexpr int kMaxQueues = 64;

  std::array<SimpleParamValueQueue, kMaxQueues> queues{};
  int count{0};

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (std::memcmp(iid, Steinberg::Vst::IParameterChanges::iid,
                    sizeof(Steinberg::TUID)) == 0) {
      *obj = this;
      return Steinberg::kResultOk;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }
  Steinberg::uint32 PLUGIN_API addRef() override { return 1; }
  Steinberg::uint32 PLUGIN_API release() override { return 1; }

  Steinberg::int32 PLUGIN_API getParameterCount() override { return count; }

  Steinberg::Vst::IParamValueQueue *PLUGIN_API
  getParameterData(Steinberg::int32 index) override {
    if (index < 0 || index >= count)
      return nullptr;
    return &queues[index];
  }

  Steinberg::Vst::IParamValueQueue *PLUGIN_API addParameterData(
      const Steinberg::Vst::ParamID &id, Steinberg::int32 &index) override {
    if (count >= kMaxQueues)
      return nullptr;
    index = count;
    queues[count].param_id = id;
    queues[count].param_value = 0.0;
    return &queues[count++];
  }

  void reset() { count = 0; }
};

/// Minimal IEventList: fixed capacity, no dynamic allocation.
struct SimpleEventList final : Steinberg::Vst::IEventList {
  static constexpr int kMaxEvents = 256;

  std::array<Steinberg::Vst::Event, kMaxEvents> events{};
  int count{0};

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (std::memcmp(iid, Steinberg::Vst::IEventList::iid,
                    sizeof(Steinberg::TUID)) == 0) {
      *obj = this;
      return Steinberg::kResultOk;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }
  Steinberg::uint32 PLUGIN_API addRef() override { return 1; }
  Steinberg::uint32 PLUGIN_API release() override { return 1; }

  Steinberg::int32 PLUGIN_API getEventCount() override { return count; }

  Steinberg::tresult PLUGIN_API getEvent(Steinberg::int32 index,
                                         Steinberg::Vst::Event &e) override {
    if (index < 0 || index >= count)
      return Steinberg::kResultFalse;
    e = events[index];
    return Steinberg::kResultOk;
  }

  Steinberg::tresult PLUGIN_API addEvent(Steinberg::Vst::Event &e) override {
    if (count >= kMaxEvents)
      return Steinberg::kResultFalse;
    events[count++] = e;
    return Steinberg::kResultOk;
  }

  void reset() { count = 0; }

  bool push_note_on(Steinberg::int32 sample_offset, Steinberg::int16 channel,
                    Steinberg::int16 pitch, float velocity) {
    if (count >= kMaxEvents)
      return false;
    auto &e = events[count++];
    e = {};
    e.busIndex = 0;
    e.sampleOffset = sample_offset;
    e.type = Steinberg::Vst::Event::kNoteOnEvent;
    e.noteOn.channel = channel;
    e.noteOn.pitch = pitch;
    e.noteOn.velocity = velocity;
    e.noteOn.tuning = 0.f;
    e.noteOn.length = 0;
    e.noteOn.noteId = -1;
    return true;
  }

  bool push_note_off(Steinberg::int32 sample_offset, Steinberg::int16 channel,
                     Steinberg::int16 pitch, float velocity) {
    if (count >= kMaxEvents)
      return false;
    auto &e = events[count++];
    e = {};
    e.busIndex = 0;
    e.sampleOffset = sample_offset;
    e.type = Steinberg::Vst::Event::kNoteOffEvent;
    e.noteOff.channel = channel;
    e.noteOff.pitch = pitch;
    e.noteOff.velocity = velocity;
    e.noteOff.tuning = 0.f;
    e.noteOff.noteId = -1;
    return true;
  }

  void sort_by_sample_offset() {
    if (count <= 1)
      return;
    std::sort(
        events.begin(), events.begin() + count,
        [](const Steinberg::Vst::Event &a, const Steinberg::Vst::Event &b) {
          return a.sampleOffset < b.sampleOffset;
        });
  }
};

/// IComponentHandler that captures performEdit() callbacks from the plugin GUI
/// and enqueues them for delivery to IAudioProcessor on the next process call.
struct ComponentHandlerImpl final : Steinberg::Vst::IComponentHandler {
  SphereDauxVst3Processor *owner{nullptr};

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (std::memcmp(iid, Steinberg::Vst::IComponentHandler::iid,
                    sizeof(Steinberg::TUID)) == 0) {
      *obj = this;
      return Steinberg::kResultOk;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }
  Steinberg::uint32 PLUGIN_API addRef() override { return 1; }
  Steinberg::uint32 PLUGIN_API release() override { return 1; }

  Steinberg::tresult PLUGIN_API beginEdit(Steinberg::Vst::ParamID) override {
    return Steinberg::kResultOk;
  }
  Steinberg::tresult PLUGIN_API endEdit(Steinberg::Vst::ParamID) override {
    return Steinberg::kResultOk;
  }
  Steinberg::tresult PLUGIN_API restartComponent(Steinberg::int32) override {
    return Steinberg::kResultOk;
  }

  // Defined below, after SphereDauxVst3Processor is complete.
  Steinberg::tresult PLUGIN_API performEdit(
      Steinberg::Vst::ParamID id, Steinberg::Vst::ParamValue value) override;
};

// ── Platform editor function forward declarations
// ───────────────────────────── Implementations live in editor_mac.mm (macOS)
// and editor_linux.cpp (Linux).

#if defined(__APPLE__)
unsigned long long open_editor_mac(SphereDauxVst3Processor *, const char *,
                                   const char *, int, int);
void close_editor_mac(SphereDauxVst3Processor *);
int focus_editor_mac(SphereDauxVst3Processor *);
void shutdown_editor_mac(SphereDauxVst3Processor *);
/// Resize the host-owned editor window's content area and tell the view.
///
/// The Cocoa half of `sphere_daux_vst3_view_set_size`: on macOS the editor's
/// window belongs to this process, so applying a size and reporting it are the
/// same act. Declared here rather than reached through the AppKit header so
/// `vst3_processor.cpp` stays free of Cocoa.
void resize_editor_mac(SphereDauxVst3Processor *, int width, int height,
                       const char *reason);
#elif defined(__linux__)
unsigned long long open_editor_linux(SphereDauxVst3Processor *, const char *,
                                     const char *, int, int);
void close_editor_linux(SphereDauxVst3Processor *);
int focus_editor_linux(SphereDauxVst3Processor *);
void shutdown_editor_linux(SphereDauxVst3Processor *);
#endif

// ── Main processor struct
// ─────────────────────────────────────────────────────

struct SphereDauxVst3Processor {
  static constexpr int kMaxPending = 64;
  static constexpr int kMaxBridgeChannels = 32;
  static constexpr int kMaxBridgeBuses = 16;
  static constexpr int kMaxProcessFrames = 8192;

  VST3::Hosting::Module::Ptr module;
  Steinberg::Vst::HostApplication host_context;
  Steinberg::IPtr<Steinberg::Vst::IComponent> component;
  Steinberg::IPtr<Steinberg::Vst::IAudioProcessor> processor;
  Steinberg::IPtr<Steinberg::Vst::IEditController> controller;
  /// MIDI controller → parameter mapping (queried from `controller`). Null when
  /// the plugin exposes no IMidiMapping; CC events are then ignored.
  Steinberg::IPtr<Steinberg::Vst::IMidiMapping> midi_mapping;
  Steinberg::IPtr<Steinberg::Vst::IConnectionPoint> component_connection;
  Steinberg::IPtr<Steinberg::Vst::IConnectionPoint> controller_connection;
  bool controller_is_component{false};

  // Stereo single-sample I/O buffers
  Steinberg::Vst::SpeakerArrangement input_arrangement =
      Steinberg::Vst::SpeakerArr::kStereo;
  Steinberg::Vst::SpeakerArrangement output_arrangement =
      Steinberg::Vst::SpeakerArr::kStereo;
  float input_l{0.f}, input_r{0.f};
  float output_l{0.f}, output_r{0.f};
  float *input_channels[2] = {&input_l, &input_r};
  float *output_channels[2] = {&output_l, &output_r};
  Steinberg::Vst::AudioBusBuffers input_bus{};
  Steinberg::Vst::AudioBusBuffers output_bus{};
  Steinberg::Vst::ProcessContext process_context{};
  Steinberg::Vst::ProcessData process_data{};
  int audio_input_bus_count{0};
  int audio_output_bus_count{0};
  int main_audio_input_channel_count{2};
  int main_audio_output_channel_count{2};
  int bridge_audio_output_channel_count{2};
  std::array<int, kMaxBridgeBuses> audio_output_bus_channel_counts{};
  std::array<std::string, kMaxBridgeBuses> audio_output_bus_names{};
  std::array<Steinberg::Vst::BusType, kMaxBridgeBuses> audio_output_bus_types{};
  std::array<Steinberg::Vst::SpeakerArrangement, kMaxBridgeBuses>
      audio_output_bus_arrangements{};
  std::array<bool, kMaxBridgeBuses> audio_output_bus_active_before{};
  std::array<bool, kMaxBridgeBuses> audio_output_bus_active_after{};
  std::array<Steinberg::tresult, kMaxBridgeBuses>
      audio_output_bus_activate_results{};
  int active_audio_output_bus_count{0};
  bool processing{false};

  // Diagnostics
  unsigned long long process_count{0};
  double last_input_peak{0.0};
  double last_output_peak{0.0};
  double last_difference_peak{0.0};
  bool first_process_done{false};
  bool process_audio_out_logged{false};

  // Thread-safe parameter change queue (no dynamic allocation)
  std::array<PendingParam, kMaxPending> pending_buf{};
  int pending_count{0};
  std::mutex pending_mutex; // protects pending_buf/count

  SimpleParamChanges param_changes_obj; // reused per process call
  SimpleEventList input_events_obj;     // reused per process call
  int event_input_bus_count{0};
  ComponentHandlerImpl component_handler; // installed on IEditController
  /// Owned copy of the loaded module path (survives after create() returns).
  std::string plugin_path;
  /// `PClassInfo::name` of the instantiated audio-module class. ARA pairs a main
  /// factory with its audio module by exact class name, so this is what the ARA
  /// entry points match on.
  std::string class_name;
  /// Rate `setup()` should run at, kept so activation can be deferred.
  ///
  /// `ARAVST3.h` requires `bindToDocumentControllerWithRoles` to happen "before
  /// the first call to setActive () or setState () or
  /// getProcessContextRequirements () or the creation of the GUI", so an ARA
  /// instance is created unactivated, bound, and only then activated.
  double deferred_sample_rate{0.0};
  /// Whether `setup()` has run.
  bool activated{false};

#if defined(_WIN32)
  Steinberg::IPtr<Steinberg::IPlugView> editor_view;
  // IPlugFrame handed to the view via setFrame() before attached(). Owned by
  // this processor; created on attach, destroyed on detach. Required for
  // WebView/CEF-backed editors (UAD Native) to bootstrap and to honour
  // plug-in-driven resizeView() requests.
  PluginEditorFrame *editor_frame{nullptr};
  HWND editor_hwnd{nullptr};
  HWND editor_attach_hwnd{nullptr};
  HWND editor_fallback_label_hwnd{nullptr};
  HWND editor_fallback_reload_hwnd{nullptr};
  HWND editor_fallback_generic_hwnd{nullptr};
  HWND editor_fallback_close_hwnd{nullptr};
  unsigned long long editor_handle{0};
  std::string editor_window_id;
  std::string editor_title;
  int editor_requested_width{0};
  int editor_requested_height{0};
  bool editor_attached{false};
  // ── GPUI-embedded editor state ───────────────────────────────────────────
  // When the editor is hosted inside a GPUI PluginView window (rather than the
  // daux-owned top-level shell above), `editor_parent_hwnd` is the GPUI window,
  // `editor_embed_top_hwnd` is the native editor shell, and
  // `editor_attach_hwnd` is the dedicated child content HWND passed to
  // IPlugView::attached(). The IPlugView is created from THIS processor's
  // existing `controller` — never a new component/controller.
  HWND editor_parent_hwnd{nullptr};
  HWND editor_embed_top_hwnd{nullptr};
  DauxEditorWindow editor_window{};
  int embed_host_kind{
      1}; // 0 = WS_CHILD, 1 = owned tool window, 2 = detached top-level
  bool embed_mode{false};
  bool embed_geometry_valid{false};
  RECT embed_last_applied{}; // last applied window rect (screen for tool)
  int embed_host_x{0}, embed_host_y{0}, embed_host_w{0}, embed_host_h{0};
  int embed_content_w{0}, embed_content_h{0};
  bool embed_resize_in_progress{false};
  // IPlugView::canResize, cached per created view (keyed on the view pointer
  // so view re-creation re-queries automatically). Drives the generic resize
  // contract: fixed-size views keep their getSize; resizable views go through
  // checkSizeConstraint.
  bool editor_resizable{false};
  const void *editor_resizable_view{nullptr};
  // Main-owned shell (bridge): resizeView updates these; Rust polls and resizes
  // the NativeEditorShell outer window — never SetWindowPos on
  // editor_parent_hwnd.
  std::atomic<bool> pending_main_shell_resize{false};
  int pending_main_shell_w{0};
  int pending_main_shell_h{0};
  std::string embed_instance_label;
  // Detached mode (kind==2): set by the detached window's WM_CLOSE so the Rust
  // shell can tear the editor down. Consumed (and reset) via the take accessor.
  std::atomic<bool> embed_user_closed{false};
  // Bundled browser/WebView runtime — active only while an editor is open.
  // One DLL-directory cookie per native runtime dir we added to the search
  // path.
  std::vector<DLL_DIRECTORY_COOKIE> plugin_browser_dll_cookies;
  HMODULE plugin_browser_loader =
      nullptr;                         // optional verify-load (WebView2 only)
  int plugin_browser_runtime_kind = 0; // DauxEditorRuntimeKind
  // ── Rust-owned view host ─────────────────────────────────────────────────
  //
  // The window belongs to the caller — a GPUI-owned child HWND — and this side
  // only drives `IPlugView`. Deliberately kept apart from the `embed_*` state
  // above: nothing on this path may create, move, resize, or destroy a window,
  // so there is no second owner of the editor's geometry and no C++ window
  // procedure in the loop.
  HWND view_host_parent{nullptr};
  bool view_host_attached{false};
  HostViewFrame *view_host_frame{nullptr};
  // `IPlugFrame::resizeView` lands here instead of moving anything. The host
  // polls it, resizes its own surface, and calls back with the size it granted.
  std::atomic<bool> view_host_resize_pending{false};
  int view_host_resize_w{0};
  int view_host_resize_h{0};
  // Guards window proc access; set to false before destroy so pending messages
  // received after GWLP_USERDATA is zeroed still find a valid flag.
  std::atomic<bool> processor_valid{true};
#elif defined(__APPLE__) || defined(__linux__)
  // Platform editor state (macOS / Linux).
  // ObjC and GTK4 types are hidden behind void* to keep this C++ TU clean.
  // editor_mac.mm / editor_linux.cpp access these exclusively via the
  // sphere_daux_editor_bridge.h C API.
  Steinberg::IPtr<Steinberg::IPlugView> editor_view;
  void *editor_native_window{nullptr}; // NSWindow* (mac) / GtkWidget* (linux)
  void *editor_native_embed{
      nullptr}; // NSView* (mac only — the IPlugView parent)
  void *editor_native_delegate{nullptr}; // DauxEditorWindowDelegate* (mac only)
  unsigned long long editor_handle{0};
  std::string editor_window_id;
  std::string editor_title;
  int editor_requested_width{0};
  int editor_requested_height{0};
  int editor_content_width{0};
  int editor_content_height{0};
  bool editor_attached{false};
#if defined(__APPLE__)
  // Installed before attached(), matching the VST3 editorhost contract. This
  // lets editors such as Kontakt finalize or later change their native size.
  MacPluginEditorFrame *editor_frame{nullptr};
#endif
  // Host-owned top-level editor (Linux/macOS): set when the user closes the
  // editor window via its own titlebar so the external plugin-host process can
  // poll it (sphere_daux_vst3_embed_take_user_close), report EditorClosed, and
  // detach — while keeping the audio instance alive. Read-and-reset on poll.
  std::atomic<bool> editor_user_closed{false};
  std::atomic<bool> processor_valid{true};
#endif

  // ── Setup / shutdown ───────────────────────────────────────────────────────

  static Steinberg::Vst::SpeakerArrangement
  arrangement_for_channels(int channels) {
    using namespace Steinberg::Vst;
    switch (channels) {
    case 0:
      return SpeakerArr::kEmpty;
    case 1:
      return SpeakerArr::kMono;
    case 2:
      return SpeakerArr::kStereo;
    case 3:
      return SpeakerArr::k30Cine;
    case 4:
      return SpeakerArr::k40Music;
    case 5:
      return SpeakerArr::k50;
    case 6:
      return SpeakerArr::k51;
    case 7:
      return SpeakerArr::k70Music;
    case 8:
      return SpeakerArr::k71Music;
    default:
      return SpeakerArr::kStereo;
    }
  }

  int output_bus_count_for_process() const {
    const int reported = std::min(audio_output_bus_count, kMaxBridgeBuses);
    if (reported <= 0)
      return 0;
    const int active = std::min(active_audio_output_bus_count, reported);
    return active > 0 ? active : reported;
  }

  int output_bus_channels_for_process(int bus) const {
    if (bus < 0 || bus >= kMaxBridgeBuses)
      return 0;
    int channels = audio_output_bus_channel_counts[bus];
    if (channels <= 0 && bus == 0)
      channels = main_audio_output_channel_count;
    return std::max(0, std::min(channels, kMaxBridgeChannels));
  }

  static Vst3BusAudioStats
  compute_bus_stats(const Steinberg::Vst::AudioBusBuffers &bus, int frames) {
    Vst3BusAudioStats stats{};
    stats.ptr_valid = bus.numChannels > 0 && bus.channelBuffers32 != nullptr;
    if (!stats.ptr_valid || frames <= 0)
      return stats;
    const float *left = bus.channelBuffers32[0];
    const float *right = bus.numChannels > 1 ? bus.channelBuffers32[1] : left;
    stats.ptr_valid = left != nullptr && right != nullptr;
    if (!stats.ptr_valid)
      return stats;
    double sum_l = 0.0;
    double sum_r = 0.0;
    for (int i = 0; i < frames; ++i) {
      const double l = static_cast<double>(left[i]);
      const double r = static_cast<double>(right[i]);
      const double abs_l = std::abs(l);
      const double abs_r = std::abs(r);
      stats.peak_l = std::max(stats.peak_l, abs_l);
      stats.peak_r = std::max(stats.peak_r, abs_r);
      sum_l += l * l;
      sum_r += r * r;
      if (stats.first_non_zero < 0 &&
          (abs_l > 0.0000001 || abs_r > 0.0000001)) {
        stats.first_non_zero = i;
      }
    }
    const double n = static_cast<double>(std::max(frames, 1));
    stats.rms_l = std::sqrt(sum_l / n);
    stats.rms_r = std::sqrt(sum_r / n);
    return stats;
  }

  void log_process_audio_out_once(
      int frames, int num_output_buses,
      const Steinberg::Vst::AudioBusBuffers *output_buses,
      const Vst3BusAudioStats *stats) {
    if (process_audio_out_logged && !daux_vst3_bus_debug())
      return;
    process_audio_out_logged = true;
    std::fprintf(stderr,
                 "[PROCESS AUDIO OUT]\n"
                 "block_size=%d\n"
                 "sample_rate=%.0f\n"
                 "num_output_buses_passed_to_process=%d\n",
                 frames, process_context.sampleRate, num_output_buses);
    for (int bus = 0; bus < num_output_buses; ++bus) {
      const auto &s = stats[bus];
      const auto silence_flags =
          output_buses ? output_buses[bus].silenceFlags : 0;
      std::fprintf(stderr,
                   "output_bus index=%d channel_count=%d ptr_valid=%d "
                   "silence_flags=0x%llx "
                   "peak_l=%.8f peak_r=%.8f rms_l=%.8f rms_r=%.8f "
                   "first_non_zero_sample_index=%d "
                   "routed_destination=downmix:fallback\n",
                   bus, output_buses ? output_buses[bus].numChannels : 0,
                   s.ptr_valid ? 1 : 0,
                   static_cast<unsigned long long>(silence_flags), s.peak_l,
                   s.peak_r, s.rms_l, s.rms_r, s.first_non_zero);
    }
  }

  bool setup(double sample_rate) {
    const double sr = sample_rate > 0.0 ? sample_rate : 44100.0;

    // Wire up I/O buffer descriptors
    input_bus.numChannels = 2;
    input_bus.channelBuffers32 = input_channels;
    output_bus.numChannels = 2;
    output_bus.channelBuffers32 = output_channels;

    // ProcessData is reused every call — initialise once here
    process_data.processMode = Steinberg::Vst::kRealtime;
    process_data.symbolicSampleSize = Steinberg::Vst::kSample32;
    process_data.numSamples = 1;
    process_data.numInputs = 0;
    process_data.numOutputs = 0;
    process_data.inputs = nullptr;
    process_data.outputs = nullptr;
    process_data.inputParameterChanges = nullptr;
    process_data.outputParameterChanges = nullptr;

    // Initial transport defaults. The host (engine in-process, or the bridge
    // producer) overwrites tempo/time-sig/position/playing per block via
    // sphere_daux_vst3_set_process_context before every process() call — these
    // are only the values seen if a plugin processes before the first update.
    process_context.sampleRate = sr;
    process_context.tempo = 120.0;
    process_context.timeSigNumerator = 4;
    process_context.timeSigDenominator = 4;
    process_context.state = Steinberg::Vst::ProcessContext::kTempoValid |
                            Steinberg::Vst::ProcessContext::kTimeSigValid;
    process_data.processContext = &process_context;

    const auto input_bus_count =
        component->getBusCount(Steinberg::Vst::kAudio, Steinberg::Vst::kInput);
    const auto output_bus_count =
        component->getBusCount(Steinberg::Vst::kAudio, Steinberg::Vst::kOutput);
    audio_input_bus_count = static_cast<int>(input_bus_count);
    audio_output_bus_count = static_cast<int>(output_bus_count);
    main_audio_input_channel_count = audio_input_bus_count > 0 ? 2 : 0;
    main_audio_output_channel_count = audio_output_bus_count > 0 ? 2 : 0;
    bridge_audio_output_channel_count = 0;
    audio_output_bus_channel_counts.fill(0);
    audio_output_bus_names.fill(std::string{});
    audio_output_bus_types.fill(Steinberg::Vst::kAux);
    audio_output_bus_arrangements.fill(Steinberg::Vst::SpeakerArr::kEmpty);
    audio_output_bus_active_before.fill(false);
    audio_output_bus_active_after.fill(false);
    audio_output_bus_activate_results.fill(Steinberg::kResultFalse);
    active_audio_output_bus_count = 0;
    if (audio_input_bus_count > 0) {
      Steinberg::Vst::BusInfo input_info{};
      if (component->getBusInfo(Steinberg::Vst::kAudio, Steinberg::Vst::kInput,
                                0, input_info) == Steinberg::kResultOk &&
          input_info.channelCount > 0) {
        main_audio_input_channel_count =
            static_cast<int>(input_info.channelCount);
      }
    }
    if (audio_output_bus_count > 0) {
      Steinberg::Vst::BusInfo output_info{};
      if (component->getBusInfo(Steinberg::Vst::kAudio, Steinberg::Vst::kOutput,
                                0, output_info) == Steinberg::kResultOk &&
          output_info.channelCount > 0) {
        main_audio_output_channel_count =
            static_cast<int>(output_info.channelCount);
      }
      const int buses = std::min(audio_output_bus_count, kMaxBridgeBuses);
      for (int bus = 0; bus < buses; ++bus) {
        Steinberg::Vst::BusInfo bus_info{};
        int channels = 0;
        if (component->getBusInfo(Steinberg::Vst::kAudio,
                                  Steinberg::Vst::kOutput, bus,
                                  bus_info) == Steinberg::kResultOk &&
            bus_info.channelCount > 0) {
          channels = static_cast<int>(bus_info.channelCount);
          audio_output_bus_names[bus] = vst3_tchar_to_utf8(bus_info.name);
          audio_output_bus_types[bus] = bus_info.busType;
          audio_output_bus_active_before[bus] =
              (bus_info.flags & Steinberg::Vst::BusInfo::kDefaultActive) != 0;
        }
        audio_output_bus_channel_counts[bus] = channels;
        Steinberg::Vst::SpeakerArrangement arrangement =
            Steinberg::Vst::SpeakerArr::kEmpty;
        if (processor->getBusArrangement(Steinberg::Vst::kOutput, bus,
                                         arrangement) == Steinberg::kResultOk &&
            arrangement != Steinberg::Vst::SpeakerArr::kEmpty) {
          audio_output_bus_arrangements[bus] = arrangement;
        } else {
          audio_output_bus_arrangements[bus] =
              arrangement_for_channels(channels);
        }
        bridge_audio_output_channel_count = std::min(
            kMaxBridgeChannels, bridge_audio_output_channel_count + channels);
      }
      if (bridge_audio_output_channel_count <= 0) {
        bridge_audio_output_channel_count = main_audio_output_channel_count;
      }
    }
    std::fprintf(
        stderr,
        "[SphereVST3] busCount input=%d output=%d mainInputChannels=%d "
        "mainOutputChannels=%d bridgeOutputChannels=%d\n",
        (int)input_bus_count, (int)output_bus_count,
        main_audio_input_channel_count, main_audio_output_channel_count,
        bridge_audio_output_channel_count);

    event_input_bus_count =
        component->getBusCount(Steinberg::Vst::kEvent, Steinberg::Vst::kInput);
    std::fprintf(stderr, "[SphereVST3] eventInputBusCount=%d\n",
                 event_input_bus_count);
    if (event_input_bus_count > 0) {
      const auto ev_res = component->activateBus(
          Steinberg::Vst::kEvent, Steinberg::Vst::kInput, 0, true);
      if (ev_res != Steinberg::kResultOk) {
        std::fprintf(
            stderr,
            "[SphereVST3] activate event input bus FAILED (result=%d)\n",
            (int)ev_res);
      }
    }

    // Set bus arrangements before bus activation. VST3 requires one arrangement
    // per bus for both directions. Passing a single entry for a multi-bus
    // plugin can leave the instance unconfigured and silent. Build the full
    // per-bus list from each bus's actual channel count; if the plugin still
    // rejects it we fall back to its default arrangement. Processing below
    // provides buffers for every active bus regardless, so a rejection no
    // longer means silence.
    const int arr_in_buses = std::min(audio_input_bus_count, kMaxBridgeBuses);
    const int arr_out_buses = std::min(audio_output_bus_count, kMaxBridgeBuses);
    std::array<Steinberg::Vst::SpeakerArrangement, kMaxBridgeBuses>
        in_arrangements{};
    std::array<Steinberg::Vst::SpeakerArrangement, kMaxBridgeBuses>
        out_arrangements{};
    for (int bus = 0; bus < arr_in_buses; ++bus) {
      int ch = (bus == 0) ? main_audio_input_channel_count : 2;
      Steinberg::Vst::BusInfo bi{};
      if (component->getBusInfo(Steinberg::Vst::kAudio, Steinberg::Vst::kInput,
                                bus, bi) == Steinberg::kResultOk &&
          bi.channelCount > 0) {
        ch = static_cast<int>(bi.channelCount);
      }
      in_arrangements[bus] = arrangement_for_channels(ch);
    }
    for (int bus = 0; bus < arr_out_buses; ++bus) {
      int ch = audio_output_bus_channel_counts[bus];
      if (ch <= 0)
        ch = (bus == 0) ? main_audio_output_channel_count : 2;
      out_arrangements[bus] = arrangement_for_channels(ch);
      audio_output_bus_arrangements[bus] = out_arrangements[bus];
    }
    // Keep the legacy single-arrangement members in sync for the stereo
    // single-sample I/O path (bus 0).
    input_arrangement = arr_in_buses > 0 ? in_arrangements[0]
                                         : Steinberg::Vst::SpeakerArr::kEmpty;
    output_arrangement = arr_out_buses > 0 ? out_arrangements[0]
                                           : Steinberg::Vst::SpeakerArr::kEmpty;
    const auto arrangement_res = processor->setBusArrangements(
        arr_in_buses > 0 ? in_arrangements.data() : nullptr, arr_in_buses,
        arr_out_buses > 0 ? out_arrangements.data() : nullptr, arr_out_buses);
    if (arrangement_res != Steinberg::kResultOk) {
      std::ostringstream err;
      err << g_last_error << "; setBusArrangements returned "
          << (int)arrangement_res << " for " << arr_in_buses << " in / "
          << arr_out_buses << " out buses"
          << "; continuing with plugin default arrangement";
      set_last_error(err.str());
      std::fprintf(stderr,
                   "[SphereVST3] setBusArrangements result=%d failed "
                   "inBuses=%d outBuses=%d\n",
                   (int)arrangement_res, arr_in_buses, arr_out_buses);
    } else {
      std::fprintf(stderr,
                   "[SphereVST3] setBusArrangements result=%d ok inBuses=%d "
                   "outBuses=%d mainIn=%d mainOut=%d\n",
                   (int)arrangement_res, arr_in_buses, arr_out_buses,
                   main_audio_input_channel_count,
                   main_audio_output_channel_count);
    }

    // Activate audio buses. Multi-output instruments can expose each mixer
    // route as a separate output bus, not as channels inside bus 0, so every
    // reported output bus must be active and present in ProcessData.outputs for
    // multi-out audio to exist.
    Steinberg::tresult in_res = Steinberg::kResultOk;
    if (audio_input_bus_count > 0) {
      in_res = component->activateBus(Steinberg::Vst::kAudio,
                                      Steinberg::Vst::kInput, 0, true);
      if (in_res != Steinberg::kResultOk)
        std::fprintf(stderr,
                     "[DAUx VST3] activate input bus FAILED (result=%d)\n",
                     (int)in_res);
    } else {
      std::fprintf(
          stderr,
          "[SphereVST3] activate input bus skipped reason=no_audio_inputs\n");
    }
    Steinberg::tresult out_res = audio_output_bus_count > 0
                                     ? Steinberg::kResultOk
                                     : Steinberg::kResultFalse;
    const int output_buses_to_activate =
        std::min(audio_output_bus_count, kMaxBridgeBuses);
    for (int bus = 0; bus < output_buses_to_activate; ++bus) {
      const auto bus_res = component->activateBus(
          Steinberg::Vst::kAudio, Steinberg::Vst::kOutput, bus, true);
      audio_output_bus_activate_results[bus] = bus_res;
      audio_output_bus_active_after[bus] =
          audio_output_bus_channel_counts[bus] > 0 &&
          bus_res == Steinberg::kResultOk;
      if (audio_output_bus_active_after[bus]) {
        active_audio_output_bus_count = bus + 1;
      }
      if (bus_res != Steinberg::kResultOk) {
        out_res = bus_res;
        std::fprintf(stderr,
                     "[DAUx VST3] activate output bus %d FAILED (result=%d)\n",
                     bus, (int)bus_res);
      }
    }

    std::fprintf(stderr,
                 "[SphereVST3] activateBus inputResult=%d outputResult=%d "
                 "outputBuses=%d\n",
                 (int)in_res, (int)out_res, output_buses_to_activate);

    if (active_audio_output_bus_count == 0 && audio_output_bus_count > 0) {
      active_audio_output_bus_count =
          std::min(audio_output_bus_count, kMaxBridgeBuses);
    }

    std::fprintf(stderr,
                 "[PLUGIN BUS MAP]\n"
                 "plugin_name=%s\n"
                 "num_audio_inputs=%d\n"
                 "num_audio_outputs=%d\n",
                 plugin_path.c_str(), audio_input_bus_count,
                 audio_output_bus_count);
    for (int bus = 0; bus < output_buses_to_activate; ++bus) {
      std::fprintf(
          stderr,
          "output_bus index=%d name=\"%s\" media_type=audio bus_type=%s "
          "channel_count=%d speaker_arrangement=0x%llx active_before=%d "
          "activate_result=%d active_after=%d "
          "routed_to_track_or_downmix=downmix:fallback\n",
          bus,
          audio_output_bus_names[bus].empty()
              ? "(unnamed)"
              : audio_output_bus_names[bus].c_str(),
          vst3_bus_type_name(audio_output_bus_types[bus]),
          audio_output_bus_channel_counts[bus],
          static_cast<unsigned long long>(audio_output_bus_arrangements[bus]),
          audio_output_bus_active_before[bus] ? 1 : 0,
          static_cast<int>(audio_output_bus_activate_results[bus]),
          audio_output_bus_active_after[bus] ? 1 : 0);
    }

    process_data.numInputs = audio_input_bus_count > 0 ? 1 : 0;
    process_data.inputs = audio_input_bus_count > 0 ? &input_bus : nullptr;
    process_data.numOutputs = active_audio_output_bus_count;
    process_data.outputs = nullptr;

    // setupProcessing
    Steinberg::Vst::ProcessSetup ps{};
    ps.processMode = Steinberg::Vst::kRealtime;
    ps.symbolicSampleSize = Steinberg::Vst::kSample32;
    ps.maxSamplesPerBlock = 8192;
    ps.sampleRate = sr;
    const auto setup_res = processor->setupProcessing(ps);
    if (setup_res != Steinberg::kResultOk) {
      std::ostringstream err;
      err << g_last_error << "; setupProcessing returned " << (int)setup_res
          << " sr=" << sr << " maxBlock=8192 realtime sample32";
      set_last_error(err.str());
      std::fprintf(stderr, "[DAUx VST3] setupProcessing FAILED (result=%d)\n",
                   (int)setup_res);
      return false;
    }
    std::fprintf(stderr,
                 "[SphereVST3] setupProcessing sr=%.0f block=8192 result=%d ok "
                 "realtime sample32\n",
                 sr, (int)setup_res);

    // setActive(true) — accept kResultOk and kNotImplemented (some plugins
    // simply don't need explicit activation; treating not-implemented as fatal
    // would block legitimate plugins).
    const auto active_res = component->setActive(true);
    if (active_res != Steinberg::kResultOk &&
        active_res != Steinberg::kNotImplemented) {
      std::ostringstream err;
      err << g_last_error << "; setActive(true) returned " << (int)active_res;
      set_last_error(err.str());
      std::fprintf(stderr, "[DAUx VST3] setActive(true) FAILED (result=%d)\n",
                   (int)active_res);
      return false;
    }
    std::fprintf(stderr, "[SphereVST3] setActive result=%d ok\n",
                 (int)active_res);

    // setProcessing(true) — accept kResultOk and kNotImplemented.
    // Per VST3 spec, setProcessing is an optional notification; plugins like
    // iZotope Ozone return kNotImplemented (0x80004001) and that's legitimate.
    const auto proc_res = processor->setProcessing(true);
    if (proc_res != Steinberg::kResultOk &&
        proc_res != Steinberg::kNotImplemented) {
      std::ostringstream err;
      err << g_last_error << "; setProcessing(true) returned " << (int)proc_res;
      set_last_error(err.str());
      std::fprintf(stderr,
                   "[DAUx VST3] setProcessing(true) FAILED (result=%d)\n",
                   (int)proc_res);
      return false;
    }
    processing = true;
    std::fprintf(
        stderr, "[SphereVST3] setProcessing result=%d ok (notImplemented=%d)\n",
        (int)proc_res, proc_res == Steinberg::kNotImplemented ? 1 : 0);

    // Register IComponentHandler so plugin GUI edits are captured
    if (controller) {
      component_handler.owner = this;
      const auto ch_res = controller->setComponentHandler(&component_handler);
      if (ch_res == Steinberg::kResultOk)
        std::fprintf(stderr, "[DAUx VST3] IComponentHandler registered\n");
      else
        std::fprintf(
            stderr,
            "[DAUx VST3] setComponentHandler not accepted (result=%d) — "
            "GUI edits may not reach processor\n",
            (int)ch_res);

      // MIDI controller → parameter mapping (optional). Used to route CC /
      // pitch-bend / aftertouch input to parameter changes during process().
      midi_mapping =
          Steinberg::FUnknownPtr<Steinberg::Vst::IMidiMapping>(controller);
      std::fprintf(stderr, "[DAUx VST3] IMidiMapping %s\n",
                   midi_mapping ? "available" : "not exposed");
    }

    return true;
  }

  void shutdown() {
#if defined(_WIN32)
    close_editor_window();
#elif defined(__APPLE__)
    shutdown_editor_mac(this);
#elif defined(__linux__)
    shutdown_editor_linux(this);
#endif
    if (processor && processing)
      processor->setProcessing(false);
    processing = false;
    if (component_connection && controller_connection) {
      component_connection->disconnect(controller_connection);
      controller_connection->disconnect(component_connection);
    }
    component_connection = nullptr;
    controller_connection = nullptr;
    if (controller && !controller_is_component) {
      if (auto pb = Steinberg::FUnknownPtr<Steinberg::IPluginBase>(controller))
        pb->terminate();
    }
    if (component) {
      component->setActive(false);
      if (auto pb = Steinberg::FUnknownPtr<Steinberg::IPluginBase>(component))
        pb->terminate();
    }
  }

  // ── Thread-safe parameter enqueue (called from audio thread OR GUI thread)
  // ──

  /// Drain pending parameters, route MIDI CC to parameter changes, and fill
  /// inputEvents with note on/off for this block.
  void prepare_process_io(const SphereDauxVst3MidiEvent *midi_events,
                          int midi_event_count) {
    // Parameter changes come from two sources this block: GUI/host edits queued
    // in `pending_buf`, and CC events mapped via IMidiMapping below.
    param_changes_obj.reset();
    {
      std::lock_guard<std::mutex> lock(pending_mutex);
      for (int i = 0; i < pending_count; ++i) {
        Steinberg::int32 idx = 0;
        auto *q = param_changes_obj.addParameterData(pending_buf[i].id, idx);
        if (q) {
          Steinberg::int32 dummy = 0;
          q->addPoint(0, pending_buf[i].value, dummy);
        }
      }
      pending_count = 0;
    }

    input_events_obj.reset();
    int cc_mapped = 0;
    if (midi_events && midi_event_count > 0) {
      const int n = std::min(midi_event_count, SimpleEventList::kMaxEvents);
      for (int i = 0; i < n; ++i) {
        const auto &m = midi_events[i];
        const auto ch = static_cast<Steinberg::int16>(m.channel & 0x0F);
        const auto offset = static_cast<Steinberg::int32>(m.sample_offset);
        if (m.kind == 2) {
          // ControlChange → parameter change via IMidiMapping. `pitch` carries
          // the VST3 controller number (not masked to 7 bits: 128/129 are
          // aftertouch / pitch bend). The block-level value wins (our
          // SimpleParamValueQueue holds a single point).
          if (!midi_mapping) {
            continue;
          }
          const auto ctrl = static_cast<Steinberg::Vst::CtrlNumber>(m.pitch);
          Steinberg::Vst::ParamID pid = 0;
          if (midi_mapping->getMidiControllerAssignment(0, ch, ctrl, pid) ==
              Steinberg::kResultOk) {
            Steinberg::int32 idx = 0;
            auto *q = param_changes_obj.addParameterData(pid, idx);
            if (q) {
              Steinberg::int32 dummy = 0;
              q->addPoint(offset,
                          static_cast<Steinberg::Vst::ParamValue>(m.velocity),
                          dummy);
              ++cc_mapped;
            }
          }
          continue;
        }
        if (event_input_bus_count <= 0) {
          continue;
        }
        const auto pitch = static_cast<Steinberg::int16>(m.pitch & 0x7F);
        if (m.kind == 1) {
          input_events_obj.push_note_on(offset, ch, pitch, m.velocity);
        } else {
          input_events_obj.push_note_off(offset, ch, pitch, m.velocity);
        }
      }
      input_events_obj.sort_by_sample_offset();

      if (daux_vst3_midi_debug()) {
        std::fprintf(stderr,
                     "[vst3-midi] input_events=%d event_bus=%d active=%s\n",
                     input_events_obj.count, event_input_bus_count,
                     event_input_bus_count > 0 ? "true" : "false");
        for (int i = 0; i < input_events_obj.count; ++i) {
          const auto &e = input_events_obj.events[i];
          if (e.type == Steinberg::Vst::Event::kNoteOnEvent) {
            std::fprintf(stderr,
                         "[vst3-midi] add note_on pitch=%d velocity=%.2f "
                         "sampleOffset=%d\n",
                         (int)e.noteOn.pitch, e.noteOn.velocity,
                         (int)e.sampleOffset);
          } else if (e.type == Steinberg::Vst::Event::kNoteOffEvent) {
            std::fprintf(stderr,
                         "[vst3-midi] add note_off pitch=%d sampleOffset=%d\n",
                         (int)e.noteOff.pitch, (int)e.sampleOffset);
          }
        }
      }
    }

    process_data.inputParameterChanges =
        (param_changes_obj.count > 0) ? &param_changes_obj : nullptr;
    process_data.inputEvents =
        (input_events_obj.count > 0) ? &input_events_obj : nullptr;
  }

  /// Add or update a parameter change in the pending queue.
  /// Deduplicates by paramId — later value wins within one block.
  void enqueue_param(Steinberg::Vst::ParamID id,
                     Steinberg::Vst::ParamValue value) {
    std::lock_guard<std::mutex> lock(pending_mutex);
    for (int i = 0; i < pending_count; ++i) {
      if (pending_buf[i].id == id) {
        pending_buf[i].value = value;
        return;
      }
    }
    if (pending_count < kMaxPending)
      pending_buf[pending_count++] = {id, value};
  }

#ifdef _WIN32
  void close_editor_window();
  void close_editor_view_only(const char *reason);
  // Detach the embedded IPlugView and destroy the host window, keeping the
  // component/controller (and thus the realtime processor) alive.
  void close_embed_editor(const char *reason);
#endif
};
