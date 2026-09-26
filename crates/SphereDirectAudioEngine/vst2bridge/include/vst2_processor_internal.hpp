#pragma once
// Internal (in-crate) header for the DAUx VST2 bridge.
//
// Holds `SphereDauxVst2Processor` so the platform editor translation units
// (vst2_editor_windows.cpp, vst2_editor_mac.mm) can reach processor state
// without re-declaring it. vst2_processor.cpp owns the cross-platform VST2
// core; platform TUs own their windowing code — same split as vst3bridge.
//
// NOTE: private header — never installed, never included outside vst2bridge.

#include "sphere_daux_vst2_processor.h"
#include "sphere_vst2_abi.h"

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

// ── Shared diagnostics helpers ──────────────────────────────────────────────

inline thread_local std::string g_vst2_last_error;

inline void vst2_set_last_error(std::string value) {
  g_vst2_last_error = std::move(value);
}

inline bool daux_vst2_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_PLUGIN_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_PLUGIN_BRIDGE_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_VST2_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_FORENSIC_TRACE") != nullptr;
  return enabled;
}

inline bool daux_vst2_midi_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_FORENSIC_TRACE") != nullptr ||
      std::getenv("FUTUREBOARD_VST2_MIDI_DEBUG") != nullptr;
  return enabled;
}

/// Bridge-serialised parameter-vector state, used when the plug-in does not set
/// `effFlagsProgramChunks`. Little-endian f32 values after a 5-byte magic and a
/// u32 count.
inline constexpr char kVst2ParamStateMagic[5] = {'F', 'B', 'V', '2', 'P'};

struct SphereDauxVst2Processor;

/// Called from `audioMaster` on the plug-in's thread. Defined in
/// vst2_processor.cpp.
intptr_t vst2_audio_master(AEffect *effect, int32_t opcode, int32_t index,
                           intptr_t value, void *ptr, float opt);

/// Monotonic opaque editor handle, shared by every platform editor TU.
/// Defined in vst2_processor.cpp.
extern "C" unsigned long long vst2_next_editor_handle(void);

#if defined(__APPLE__)
unsigned long long vst2_open_editor_mac(SphereDauxVst2Processor *, const char *,
                                        const char *, int, int);
unsigned long long vst2_embed_editor_mac(SphereDauxVst2Processor *,
                                         unsigned long long, int, int, int,
                                         int);
void vst2_embed_set_bounds_mac(SphereDauxVst2Processor *, int, int, int, int);
void vst2_close_editor_mac(SphereDauxVst2Processor *);
int vst2_focus_editor_mac(SphereDauxVst2Processor *);
void vst2_editor_idle_mac(SphereDauxVst2Processor *);
#endif

struct SphereDauxVst2Processor {
  static constexpr int kMaxPending = 64;
  static constexpr int kMaxBridgeChannels = 32;
  static constexpr int kMaxBridgeBuses = 16;
  static constexpr int kMaxProcessFrames = 8192;
  static constexpr int kMaxMidiEvents = 256;

  // ── Module + instance ─────────────────────────────────────────────────────
#if defined(_WIN32)
  HMODULE module{nullptr};
#else
  void *module{nullptr}; // CFBundleRef (macOS) / dlopen handle
#endif
  AEffect *effect{nullptr};
  std::string plugin_path;
  int32_t shell_unique_id{0};
  double sample_rate{44100.0};
  bool opened{false};
  bool processing{false};
  bool has_editor{false};
  bool is_synth{false};
  bool uses_chunks{false};

  int num_inputs{0};
  int num_outputs{0};
  int num_params{0};
  int audio_input_bus_count{0};
  int audio_output_bus_count{0};
  int main_audio_input_channel_count{0};
  int main_audio_output_channel_count{0};
  int bridge_audio_output_channel_count{0};
  std::array<int, kMaxBridgeBuses> audio_output_bus_channel_counts{};
  int event_input_bus_count{0};

  // ── Preallocated audio scratch (no allocation on the audio path) ──────────
  // Non-interleaved planar storage plus the channel pointer arrays VST2 wants.
  std::vector<float> input_storage;  // kMaxBridgeChannels * kMaxProcessFrames
  std::vector<float> output_storage; // kMaxBridgeChannels * kMaxProcessFrames
  std::array<float *, kMaxBridgeChannels> input_channels{};
  std::array<float *, kMaxBridgeChannels> output_channels{};
  int allocated_frames{0};
  int allocated_input_channels{0};
  int allocated_output_channels{0};

  // ── Preallocated MIDI event block ────────────────────────────────────────
  // `VstEvents` has a trailing flexible pointer array, so the header and the
  // event pointer table are allocated once as raw bytes at create() time.
  std::vector<unsigned char> events_block;
  std::array<VstMidiEvent, kMaxMidiEvents> midi_events{};
  int midi_event_count{0};

  // ── Transport ────────────────────────────────────────────────────────────
  // Written by set_process_context (control thread, once per block) and read by
  // audioMasterGetTime (plug-in thread, inside process). Both happen on the
  // audio thread in practice; the double-buffer keeps a torn read impossible
  // without a lock.
  VstTimeInfo time_info{};

  // ── Pending parameter changes ────────────────────────────────────────────
  struct PendingParam {
    unsigned int index{0};
    float value{0.f};
  };
  std::array<PendingParam, kMaxPending> pending_buf{};
  int pending_count{0};
  std::mutex pending_mutex;

  // ── Diagnostics ──────────────────────────────────────────────────────────
  unsigned long long process_count{0};
  double last_input_peak{0.0};
  double last_output_peak{0.0};
  double last_difference_peak{0.0};

  std::atomic<bool> processor_valid{true};

  // ── Editor state ─────────────────────────────────────────────────────────
  std::string editor_title;
  std::string editor_window_id;
  std::string embed_instance_label;
  int editor_requested_width{0};
  int editor_requested_height{0};
  bool editor_attached{false};
  bool embed_mode{false};
  int embed_host_kind{1};
  int embed_content_w{0};
  int embed_content_h{0};
  int embed_host_x{0}, embed_host_y{0}, embed_host_w{0}, embed_host_h{0};
  bool embed_geometry_valid{false};
  bool embed_resize_in_progress{false};
  bool editor_resizable{false};
  unsigned long long editor_handle{0};
  std::atomic<bool> embed_user_closed{false};
  /// Raised by `audioMasterAutomate` / `audioMasterEndEdit`: the plug-in's own
  /// GUI changed a parameter, so its saved state may be stale. Cleared by
  /// `sphere_daux_vst2_take_state_touched`. Plug-ins may automate from the
  /// audio thread, hence a plain atomic store.
  std::atomic<bool> state_touched{false};
  std::atomic<bool> pending_main_shell_resize{false};
  int pending_main_shell_w{0};
  int pending_main_shell_h{0};

  // ── Host-owned view ──────────────────────────────────────────────────────
  // The `view_host_*` state backs the path where the *caller* owns the window
  // and this side only drives `effEditOpen`/`effEditClose`. It is deliberately
  // separate from the `embed_*`/`editor_window` shell state above: the two are
  // never active at once, and the resize request has its own pending slot so
  // the shell's `take_pending_shell_resize` consumer cannot swallow it.
  bool view_host_attached{false};
  int view_host_resize_w{0};
  int view_host_resize_h{0};
  std::atomic<bool> view_host_resize_pending{false};

#if defined(_WIN32)
  DauxEditorWindow editor_window{};
  HWND editor_parent_hwnd{nullptr};
  HWND editor_embed_top_hwnd{nullptr};
  HWND editor_attach_hwnd{nullptr};
  HWND view_host_parent{nullptr};
  RECT embed_last_applied{};
#elif defined(__APPLE__)
  void *editor_native_window{nullptr};   // NSWindow*
  void *editor_native_embed{nullptr};    // NSView* handed to effEditOpen
  void *editor_native_delegate{nullptr}; // DauxVst2EditorWindowDelegate*
#endif

  // ── Dispatcher helpers ───────────────────────────────────────────────────

  intptr_t dispatch(int32_t opcode, int32_t index = 0, intptr_t value = 0,
                    void *ptr = nullptr, float opt = 0.f) {
    if (!effect || !effect->dispatcher)
      return 0;
    return effect->dispatcher(effect, opcode, index, value, ptr, opt);
  }

  bool can_do(const char *feature) {
    return dispatch(effCanDo, 0, 0, const_cast<char *>(feature)) > 0;
  }

  /// Read a plug-in string into a fixed buffer. VST2 guarantees at most 64
  /// bytes for most string opcodes; callers pass a 256-byte buffer for slack
  /// because some plug-ins overrun the documented size.
  std::string dispatch_string(int32_t opcode, int32_t index) {
    char buffer[256] = {};
    dispatch(opcode, index, 0, buffer);
    buffer[sizeof(buffer) - 1] = '\0';
    return std::string(buffer);
  }

  // ── Setup / shutdown ─────────────────────────────────────────────────────

  bool setup(double sr);
  void shutdown();
  void allocate_audio_scratch(int input_channels_needed,
                              int output_channels_needed, int frames);
  void prepare_midi_events(const SphereDauxVst2MidiEvent *events, int count);
  void apply_pending_params();
  void enqueue_param(unsigned int index, float value);

  /// Core block processing into the preallocated planar output buffers.
  /// Returns false when the instance is not usable.
  bool process_planar(const float *in_l, const float *in_r, int frames,
                      const SphereDauxVst2MidiEvent *events, int event_count);

#if defined(_WIN32)
  void close_embed_editor(const char *reason);
  void close_editor_window();
#endif
};
