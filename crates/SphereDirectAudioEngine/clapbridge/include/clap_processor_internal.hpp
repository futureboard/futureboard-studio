#pragma once
// Internal (in-crate) header for the DAUx CLAP bridge.
//
// Holds `SphereDauxClapProcessor` so the platform editor translation units can
// reach processor state without re-declaring it. clap_processor.cpp owns the
// cross-platform CLAP core; platform TUs own their windowing code — the same
// split the VST3 and VST2 bridges use.
//
// NOTE: private header — never installed, never included outside clapbridge.

#include "sphere_daux_clap_processor.h"

#include "clap/clap.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
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

inline thread_local std::string g_clap_last_error;

inline void clap_set_last_error(std::string value) {
  g_clap_last_error = std::move(value);
}

inline bool daux_clap_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_PLUGIN_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_PLUGIN_BRIDGE_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_CLAP_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_FORENSIC_TRACE") != nullptr;
  return enabled;
}

inline bool daux_clap_midi_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_FORENSIC_TRACE") != nullptr ||
      std::getenv("FUTUREBOARD_CLAP_MIDI_DEBUG") != nullptr;
  return enabled;
}

struct SphereDauxClapProcessor;

/// Monotonic opaque editor handle, shared by every platform editor TU.
/// Defined in clap_processor.cpp.
extern "C" unsigned long long clap_next_editor_handle(void);

#if defined(__APPLE__)
unsigned long long clap_open_editor_mac(SphereDauxClapProcessor *, const char *,
                                        const char *, int, int);
unsigned long long clap_embed_editor_mac(SphereDauxClapProcessor *,
                                         unsigned long long, int, int, int,
                                         int);
void clap_embed_set_bounds_mac(SphereDauxClapProcessor *, int, int, int, int);
void clap_close_editor_mac(SphereDauxClapProcessor *);
int clap_focus_editor_mac(SphereDauxClapProcessor *);
#elif defined(__linux__)
unsigned long long clap_open_editor_linux(SphereDauxClapProcessor *,
                                          const char *, const char *, int,
                                          int);
void clap_close_editor_linux(SphereDauxClapProcessor *);
int clap_focus_editor_linux(SphereDauxClapProcessor *);

// Host side of clap.posix-fd-support / clap.timer-support — CLAP's Linux
// equivalent of VST3's Linux::IRunLoop, for the minority of GUI-bearing
// plug-ins that register fds/timers with the host instead of running their
// own loop. Implemented in clap_editor_linux.cpp (bridged onto the GTK
// thread's GLib main context); advertised from clap_processor.cpp's
// host_get_extension only on this platform, since Win32/Cocoa editors don't
// need it.
bool clap_host_register_fd_linux(const clap_host_t *, int,
                                 clap_posix_fd_flags_t);
bool clap_host_modify_fd_linux(const clap_host_t *, int, clap_posix_fd_flags_t);
bool clap_host_unregister_fd_linux(const clap_host_t *, int);
bool clap_host_register_timer_linux(const clap_host_t *, uint32_t, clap_id *);
bool clap_host_unregister_timer_linux(const clap_host_t *, clap_id);
#endif

/// One CLAP event as it sits in our preallocated input list. CLAP events are a
/// tagged union addressed through `clap_event_header_t`, so a fixed-size cell
/// large enough for every event kind we emit keeps the list allocation-free.
union ClapEventCell {
  clap_event_header_t header;
  clap_event_note_t note;
  clap_event_midi_t midi;
  clap_event_param_value_t param;
};

/// Cached parameter metadata. CLAP parameters carry absolute values, but the
/// bridge API (shared with VST3/VST2) is normalized `0..1`, so every conversion
/// needs the range. Cached at setup so no conversion touches the plug-in from
/// the audio thread.
struct ClapParamRange {
  clap_id id{0};
  double min_value{0.0};
  double max_value{1.0};

  double denormalize(double normalized) const {
    const double clamped = normalized < 0.0 ? 0.0
                           : normalized > 1.0
                               ? 1.0
                               : normalized;
    return min_value + clamped * (max_value - min_value);
  }

  double normalize(double absolute) const {
    const double span = max_value - min_value;
    if (span <= 0.0) {
      return 0.0;
    }
    const double n = (absolute - min_value) / span;
    return n < 0.0 ? 0.0 : (n > 1.0 ? 1.0 : n);
  }
};

struct SphereDauxClapProcessor {
  static constexpr int kMaxPending = 64;
  static constexpr int kMaxBridgeChannels = 32;
  static constexpr int kMaxBridgeBuses = 16;
  static constexpr int kMaxProcessFrames = 8192;
  static constexpr int kMaxEvents = 512;

  // ── Module + instance ─────────────────────────────────────────────────────
#if defined(_WIN32)
  HMODULE module{nullptr};
#else
  void *module{nullptr}; // CFBundleRef (macOS) / dlopen handle (Linux)
#endif
  const clap_plugin_entry_t *entry{nullptr};
  const clap_plugin_t *plugin{nullptr};
  clap_host_t host{};
  std::string plugin_path;
  std::string plugin_id;
  double sample_rate{44100.0};
  bool entry_initialized{false};
  bool plugin_initialized{false};
  bool activated{false};
  bool processing{false};

  // ── Extensions (queried once after init) ─────────────────────────────────
  const clap_plugin_audio_ports_t *ext_audio_ports{nullptr};
  const clap_plugin_note_ports_t *ext_note_ports{nullptr};
  const clap_plugin_params_t *ext_params{nullptr};
  const clap_plugin_state_t *ext_state{nullptr};
  const clap_plugin_gui_t *ext_gui{nullptr};
  const clap_plugin_latency_t *ext_latency{nullptr};

  // ── Port topology ────────────────────────────────────────────────────────
  int audio_input_bus_count{0};
  int audio_output_bus_count{0};
  int main_audio_input_channel_count{0};
  int main_audio_output_channel_count{0};
  int bridge_audio_output_channel_count{0};
  std::array<int, kMaxBridgeBuses> audio_output_bus_channel_counts{};
  std::array<int, kMaxBridgeBuses> audio_input_bus_channel_counts{};
  int event_input_bus_count{0};
  /// True when the plug-in's note input port accepts `CLAP_NOTE_DIALECT_CLAP`.
  /// False means notes have to travel as MIDI events instead.
  bool note_port_accepts_clap_dialect{false};
  bool note_port_accepts_midi{false};

  // ── Preallocated audio scratch ───────────────────────────────────────────
  // CLAP wants one `clap_audio_buffer_t` per port, each pointing at a channel
  // pointer array. All of it is sized once in `setup`.
  std::vector<float> input_storage;
  std::vector<float> output_storage;
  std::vector<float *> input_channel_ptrs;
  std::vector<float *> output_channel_ptrs;
  std::vector<clap_audio_buffer_t> input_buffers;
  std::vector<clap_audio_buffer_t> output_buffers;
  int allocated_frames{0};

  // ── Preallocated events ──────────────────────────────────────────────────
  std::vector<ClapEventCell> event_storage;
  int event_count{0};
  clap_input_events_t in_events{};
  clap_output_events_t out_events{};

  // ── Transport ────────────────────────────────────────────────────────────
  clap_event_transport_t transport{};
  uint64_t steady_time{0};

  // ── Pending parameter changes (absolute CLAP values) ─────────────────────
  struct PendingParam {
    clap_id id{0};
    double value{0.0};
  };
  std::array<PendingParam, kMaxPending> pending_buf{};
  int pending_count{0};
  std::mutex pending_mutex;
  std::vector<ClapParamRange> param_ranges;

  // ── Host-callback flags ──────────────────────────────────────────────────
  std::atomic<bool> restart_requested{false};
  std::atomic<bool> process_requested{false};
  std::atomic<bool> callback_requested{false};
  std::atomic<int> reported_latency{0};

  // ── Diagnostics ──────────────────────────────────────────────────────────
  unsigned long long process_count{0};
  double last_input_peak{0.0};
  double last_output_peak{0.0};
  double last_difference_peak{0.0};

  std::atomic<bool> processor_valid{true};

  // ── Thread identity (for the clap.thread-check extension) ────────────────
  /// The thread that created the instance. CLAP's "main thread" for this
  /// plug-in: init, activate, GUI, and state all run here.
  std::thread::id main_thread_id{};
  /// Whichever thread last entered `process()`. Latched on the first block so
  /// `is_audio_thread` can answer truthfully instead of guessing.
  std::thread::id audio_thread_id{};
  std::atomic<bool> audio_thread_known{false};

  // ── Editor state ─────────────────────────────────────────────────────────
  std::string editor_title;
  std::string editor_window_id;
  std::string embed_instance_label;
  bool gui_created{false};
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
  std::atomic<bool> pending_main_shell_resize{false};
  std::atomic<int> pending_main_shell_w{0};
  std::atomic<int> pending_main_shell_h{0};

  // ── Host-owned view ──────────────────────────────────────────────────────
  // The `view_host_*` state backs the path where the *caller* owns the window
  // and this side only drives `clap.gui`. It is deliberately separate from the
  // `embed_*`/`editor_window` shell state above: the two are never active at
  // once, and the resize request has its own pending slot so the shell's
  // `take_pending_shell_resize` consumer cannot swallow it.
  bool view_host_attached{false};
  std::atomic<int> view_host_resize_w{0};
  std::atomic<int> view_host_resize_h{0};
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
  void *editor_native_embed{nullptr};    // NSView* handed to gui->set_parent
  void *editor_native_delegate{nullptr}; // DauxClapEditorWindowDelegate*
#elif defined(__linux__)
  void *editor_native_window{nullptr}; // GtkWidget* (GtkWindow), g_object_ref'd
#endif

  // ── Lifecycle ────────────────────────────────────────────────────────────

  bool setup(double sr);
  void shutdown();
  void cache_port_topology();
  void cache_param_ranges();
  void allocate_audio_scratch(int frames);
  void build_events(const SphereDauxClapMidiEvent *events, int count);
  void enqueue_param(clap_id id, double absolute_value);

  const ClapParamRange *range_for(clap_id id) const {
    for (const auto &range : param_ranges) {
      if (range.id == id) {
        return &range;
      }
    }
    return nullptr;
  }

  /// Core block processing into the preallocated output buffers.
  bool process_planar(const float *in_l, const float *in_r, int frames,
                      const SphereDauxClapMidiEvent *events, int event_count);

  /// Preferred editor size from `clap_plugin_gui->get_size`, or 0 when the
  /// plug-in has no GUI or does not report one.
  bool preferred_gui_size(int *width, int *height);

#if defined(_WIN32)
  void close_embed_editor(const char *reason);
  void close_editor_window();
#endif
};
