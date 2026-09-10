#pragma once

#ifdef _WIN32
#define SPHERE_DAUX_VST3_API __declspec(dllexport)
#else
#define SPHERE_DAUX_VST3_API __attribute__((visibility("default")))
#endif

extern "C" {

struct SphereDauxVst3Processor;

SPHERE_DAUX_VST3_API int sphere_daux_vst3_bridge_probe(void);

SPHERE_DAUX_VST3_API const char *sphere_daux_vst3_last_error(void);

SPHERE_DAUX_VST3_API SphereDauxVst3Processor *
sphere_daux_vst3_create(const char *plugin_path, const char *class_id,
                        double sample_rate);

SPHERE_DAUX_VST3_API void
sphere_daux_vst3_destroy(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_process_stereo_sample(SphereDauxVst3Processor *processor,
                                       float in_l, float in_r, float *out_l,
                                       float *out_r);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_process_stereo_block(SphereDauxVst3Processor *processor,
                                      const float *in_l, const float *in_r,
                                      float *out_l, float *out_r, int frames);

/// MIDI event for batched delivery via processData.
/// kind: 0 = NoteOff, 1 = NoteOn, 2 = ControlChange.
/// For notes: `pitch` is the MIDI key, `velocity` is normalized [0.0, 1.0].
/// For ControlChange: `pitch` is the VST3 controller number (0..127 = MIDI CC,
/// 128 = aftertouch, 129 = pitch bend) and `velocity` is the normalized value
/// [0.0, 1.0]. CC events are routed to parameter changes via IMidiMapping and
/// are ignored when the plugin exposes no mapping.
typedef struct SphereDauxVst3MidiEvent {
  unsigned int sample_offset;
  unsigned char kind;
  unsigned char channel;
  unsigned char pitch;
  float velocity;
} SphereDauxVst3MidiEvent;

/// Process a stereo block with optional VST3 input note events (sorted by
/// sample_offset). When event_count is 0 or the plugin has no event input bus,
/// behaves like sphere_daux_vst3_process_stereo_block.
SPHERE_DAUX_VST3_API int sphere_daux_vst3_process_stereo_block_with_midi(
    SphereDauxVst3Processor *processor, const float *in_l, const float *in_r,
    float *out_l, float *out_r, int frames,
    const SphereDauxVst3MidiEvent *events, int event_count);

/// Process the main audio output bus into an interleaved buffer with
/// `output_channels` channels. `output_channels` must be at least the plugin's
/// main output channel count (or the desired capped bridge channel count).
/// Missing / unsupported channels are zero-filled by the host bridge.
SPHERE_DAUX_VST3_API int sphere_daux_vst3_process_main_output_block_with_midi(
    SphereDauxVst3Processor *processor, const float *in_l, const float *in_r,
    float *out_interleaved, int frames, int output_channels,
    const SphereDauxVst3MidiEvent *events, int event_count);

/// Number of event input buses reported at plugin setup (0 if none).
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_event_input_bus_count(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API unsigned long long
sphere_daux_vst3_process_count(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API double
sphere_daux_vst3_last_input_peak(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API double
sphere_daux_vst3_last_output_peak(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API double
sphere_daux_vst3_last_difference_peak(SphereDauxVst3Processor *processor);

/// Enqueue a normalized parameter change (0..1) for the given VST3 ParamID.
/// The change is delivered to IAudioProcessor via inputParameterChanges on the
/// next sphere_daux_vst3_process_stereo_sample call.
/// Thread-safe: may be called from the audio thread or the UI thread.
SPHERE_DAUX_VST3_API void
sphere_daux_vst3_set_param(SphereDauxVst3Processor *processor,
                           unsigned int param_id, double value);

/// Open/focus/close a native editor for the already-created processor instance.
/// UI thread only. The editor is bound to the same component/controller that
/// feeds the realtime processor, so GUI parameter edits enqueue into the same
/// parameter queue consumed by process().
SPHERE_DAUX_VST3_API unsigned long long
sphere_daux_vst3_open_editor(SphereDauxVst3Processor *processor,
                             const char *window_id, const char *title,
                             int width, int height);

SPHERE_DAUX_VST3_API void
sphere_daux_vst3_close_editor(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_focus_editor(SphereDauxVst3Processor *processor);

// ── GPUI-embedded editor (Windows) ───────────────────────────────────────────
// Attach the already-created instance's editor view (built from the same
// IEditController that feeds the realtime processor) into a GPUI-provided
// parent window, instead of a daux-owned top-level shell. No new
// component/controller is created — GUI edits affect the live audio processor.
// UI thread only. `parent_hwnd` is the GPUI PluginView HWND; x/y/width/height
// are the host region in physical pixels relative to the parent client area.
// Returns a non-zero editor handle on success, 0 on failure.
SPHERE_DAUX_VST3_API unsigned long long
sphere_daux_vst3_embed_editor(SphereDauxVst3Processor *processor,
                              unsigned long long parent_hwnd, int x, int y,
                              int width, int height);

// Reposition/resize the embedded host window (physical px, parent-client
// coords).
SPHERE_DAUX_VST3_API void
sphere_daux_vst3_embed_set_bounds(SphereDauxVst3Processor *processor, int x,
                                  int y, int width, int height);

// Cheap per-frame poll: tracks parent-window moves; no-ops when unchanged.
SPHERE_DAUX_VST3_API void
sphere_daux_vst3_embed_refresh(SphereDauxVst3Processor *processor);

// Detach the embedded IPlugView and destroy the host window. The
// component/controller (and thus the realtime processor) stay alive.
SPHERE_DAUX_VST3_API void
sphere_daux_vst3_embed_detach(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_embed_is_valid(SphereDauxVst3Processor *processor);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_embed_has_visible_ui(SphereDauxVst3Processor *processor);

// 0 = WS_CHILD, 1 = owned tool window, 2 = detached top-level, -1 = none.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_embed_host_kind(SphereDauxVst3Processor *processor);

// Detached mode only: 1 (and resets) if the user closed the standalone editor
// window so the host can tear the editor down; 0 otherwise.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_embed_take_user_close(SphereDauxVst3Processor *processor);

/// Claim one bare-Space transport toggle from any editor UI thread. Process-wide.
SPHERE_DAUX_VST3_API void sphere_daux_vst3_claim_transport_toggle(void);

/// Number of bare Space presses claimed from plug-in editor windows since the
/// last call. Process-wide; the host turns each into HostEvent::TransportToggleRequested.
SPHERE_DAUX_VST3_API unsigned int
sphere_daux_vst3_take_transport_toggle_requests(void);

SPHERE_DAUX_VST3_API void
sphere_daux_vst3_embed_set_waiting_stage(SphereDauxVst3Processor *processor,
                                         const char *stage);

SPHERE_DAUX_VST3_API void
sphere_daux_vst3_embed_set_instance_label(SphereDauxVst3Processor *processor,
                                          const char *instance_id);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_prepare_editor_view(SphereDauxVst3Processor *processor,
                                     int *out_width, int *out_height);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_take_pending_shell_resize(SphereDauxVst3Processor *processor,
                                           int *out_width, int *out_height);

SPHERE_DAUX_VST3_API int
sphere_daux_vst3_embed_content_size(SphereDauxVst3Processor *processor,
                                    int *out_width, int *out_height);

// ── ARA 2 entry points ───────────────────────────────────────────────────────
// ARA hosting lives in `crates/SphereAraHost`, which owns the document graph and
// the host services. This bridge only hands it the two COM pointers ARA needs
// from a VST3 module, so the ARA COM work stays in the audited shim inside
// `ara2-bridge-companion` and never gets hand-rolled here.

/// Creates an instance and stops before activation.
///
/// `ARAVST3.h` requires `bindToDocumentControllerWithRoles` to be called "before
/// the first call to setActive () or setState () or
/// getProcessContextRequirements () or the creation of the GUI", so an ARA
/// instance cannot be activated at creation the way an insert is. Bind it, then
/// call `sphere_daux_vst3_activate`.
SPHERE_DAUX_VST3_API SphereDauxVst3Processor *
sphere_daux_vst3_create_for_ara(const char *plugin_path, const char *class_id,
                                double sample_rate);

/// Runs the deferred activation. Returns 1 on success, or if already active.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_activate(SphereDauxVst3Processor *processor);

// ── Rust-owned view host ─────────────────────────────────────────────────────
//
// The host owns the window; these drive only `IPlugView`. Nothing on this path
// creates, moves, resizes, or destroys a window, so the caller stays the single
// owner of the editor's geometry. Main/UI thread only.

/// Creates the plug-in's editor view and attaches it to `parent_hwnd`, which
/// the caller owns and must keep alive until `sphere_daux_vst3_view_detach`.
///
/// `width`/`height` are the region the host has available; the size actually
/// reported through `out_width`/`out_height` is the plug-in's own, which the
/// host is expected to lay out around. Re-attaching to the same window is a
/// no-op that re-reports the size. Returns 1 on success.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_attach(SphereDauxVst3Processor *processor,
                             unsigned long long parent_hwnd, int width,
                             int height, int *out_width, int *out_height);

/// Releases the view (`IPlugView::removed`) and its frame. The parent window is
/// untouched. Safe when nothing is attached.
SPHERE_DAUX_VST3_API void
sphere_daux_vst3_view_detach(SphereDauxVst3Processor *processor);

/// 1 while a view is attached through the host-owned path.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_is_attached(SphereDauxVst3Processor *processor);

/// Tells the view the size the host has given it (`IPlugView::onSize`). Call
/// after the host surface has actually been resized, never before.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_set_size(SphereDauxVst3Processor *processor, int width,
                               int height);

/// The view's current content size (`IPlugView::getSize`).
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_get_size(SphereDauxVst3Processor *processor,
                               int *out_width, int *out_height);

/// 1 when the view accepts host-driven resizing (`IPlugView::canResize`).
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_can_resize(SphereDauxVst3Processor *processor);

/// Applies the VST3 size contract to a proposed content size in place: a fixed
/// view snaps to its own size, a resizable one goes through
/// `checkSizeConstraint`.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_constrain(SphereDauxVst3Processor *processor,
                                int *io_width, int *io_height);

/// Reads and clears the plug-in's pending `resizeView` request, if any.
///
/// The request is recorded rather than acted on, so the host decides what it
/// can grant, resizes its own surface, and reports the result back through
/// `sphere_daux_vst3_view_set_size`.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_view_take_resize_request(SphereDauxVst3Processor *processor,
                                          int *out_width, int *out_height);

/// Takes the instance out of the processing state without destroying it:
/// `setProcessing(false)` then `setActive(false)`.
///
/// This is the step an ARA teardown needs between "the host stopped calling
/// `process()`" and "the ARA binding is destroyed". A plug-in whose renderer is
/// still active is entitled to hold its render lock, and tearing the document
/// down from the main thread then waits on it forever. `sphere_daux_vst3_destroy`
/// does the same work, but only once the last handle is gone — far too late.
///
/// The caller must have stopped calling `process()` first; this makes no attempt
/// to interrupt a block already in flight. Idempotent.
SPHERE_DAUX_VST3_API void
sphere_daux_vst3_stop_processing(SphereDauxVst3Processor *processor);

/// 1 when the loaded module registers an ARA main-factory class matching this
/// instance's audio-module class, 0 otherwise. Cheap: reads factory class info
/// that is already loaded, and instantiates nothing.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_ara_is_supported(SphereDauxVst3Processor *processor);

/// Instantiates this module's ARA main-factory class and returns its
/// `Steinberg::FUnknown*` with one owning reference, or null when the module has
/// no matching ARA class. Release it with
/// `sphere_daux_vst3_ara_release_main_factory`, and always before destroying the
/// processor that owns the module.
SPHERE_DAUX_VST3_API void *
sphere_daux_vst3_ara_create_main_factory(SphereDauxVst3Processor *processor);

/// Releases a reference returned by `sphere_daux_vst3_ara_create_main_factory`.
SPHERE_DAUX_VST3_API void sphere_daux_vst3_ara_release_main_factory(void *unknown);

/// Borrows the initialized component's `Steinberg::FUnknown*`, which is what
/// carries `ARA::IPlugInEntryPoint(2)`. The pointer is owned by the processor:
/// it must not be released, and it dies with the processor.
SPHERE_DAUX_VST3_API void *
sphere_daux_vst3_ara_component_unknown(SphereDauxVst3Processor *processor);

/// Returns 1 if the processor has not been destroyed, 0 if it has.
/// The audio callback should call this before processing and bypass the insert
/// if it returns 0, to avoid use-after-free crashes.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_is_valid(SphereDauxVst3Processor *processor);

/// Returns the plugin's reported latency in samples (0 if not available).
/// Divide by sample_rate to get latency in seconds.
SPHERE_DAUX_VST3_API int
sphere_daux_vst3_get_latency_samples(SphereDauxVst3Processor *processor);

/// Update the transport ProcessContext (tempo / time signature / project
/// position / playing+recording) delivered to the plugin on the next
/// process() call. Call once per block from the thread that drives process().
SPHERE_DAUX_VST3_API void sphere_daux_vst3_set_process_context(
    SphereDauxVst3Processor *processor, double tempo, int time_sig_num,
    int time_sig_den, long long project_time_samples, double ppq,
    double bar_ppq, int playing, int recording);

/// Capture the plugin's current state (IComponent::getState +
/// IEditController::getState for split plugins). Returns 1 on success —
/// zero-length blobs are valid. Buffers are malloc-owned by the caller; free
/// them with sphere_daux_vst3_state_free.
SPHERE_DAUX_VST3_API int sphere_daux_vst3_get_state(
    SphereDauxVst3Processor *processor, unsigned char **out_component,
    int *out_component_len, unsigned char **out_controller,
    int *out_controller_len);

/// Restore a previously captured state (component setState →
/// controller setComponentState → controller setState). Returns 1 when the
/// component state was applied.
SPHERE_DAUX_VST3_API int sphere_daux_vst3_set_state(
    SphereDauxVst3Processor *processor, const unsigned char *component_data,
    int component_len, const unsigned char *controller_data,
    int controller_len);

/// Release a buffer returned by sphere_daux_vst3_get_state.
SPHERE_DAUX_VST3_API void sphere_daux_vst3_state_free(unsigned char *data);

/// Enumerate automatable VST3 parameters via IEditController::getParameterInfo.
/// Returns a malloc-owned UTF-8 JSON array:
/// `[{"id":1,"title":"Cutoff","short_title":"","unit":"","automatable":true,"hidden":false,"read_only":false,"value_normalized":0.5},...]`
/// Returns nullptr when the processor or controller is unavailable.
SPHERE_DAUX_VST3_API char *
sphere_daux_vst3_list_parameters_json(SphereDauxVst3Processor *processor);

/// Release a buffer returned by sphere_daux_vst3_list_parameters_json.
SPHERE_DAUX_VST3_API void sphere_daux_vst3_parameters_json_free(char *data);
/// The `NSWindow*` of this instance's host-owned editor, as an opaque handle.
///
/// 0 on Windows, and 0 whenever no editor is open. The caller passes it to the
/// shared editor-chrome ABI (`sphere_daux_editor_chrome.h`), which addresses the
/// strip by the window it lives in, so one strip implementation serves every
/// format.
SPHERE_DAUX_VST3_API unsigned long long
sphere_daux_vst3_editor_native_window(SphereDauxVst3Processor *processor);

}
