#pragma once

// VST2 runtime bridge.
//
// Mirrors `sphere_daux_vst3_processor.h` function-for-function so the Rust
// `Vst3RuntimeProcessor` can dispatch to either backend behind one method
// surface. Read that header for the contract of each call; the notes here only
// cover where VST2 differs.

#ifdef _WIN32
#define SPHERE_DAUX_VST2_API __declspec(dllexport)
#else
#define SPHERE_DAUX_VST2_API __attribute__((visibility("default")))
#endif

extern "C" {

struct SphereDauxVst2Processor;

SPHERE_DAUX_VST2_API int sphere_daux_vst2_bridge_probe(void);

SPHERE_DAUX_VST2_API const char *sphere_daux_vst2_last_error(void);

/// `class_id` selects a sub-plug-in inside a shell module. Accepted forms:
/// `"vst2:<decimal uniqueID>"`, a bare decimal, or a 4-character FourCC.
/// Empty selects the module's default plug-in.
SPHERE_DAUX_VST2_API SphereDauxVst2Processor *
sphere_daux_vst2_create(const char *plugin_path, const char *class_id,
                        double sample_rate);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_destroy(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_process_stereo_sample(SphereDauxVst2Processor *processor,
                                       float in_l, float in_r, float *out_l,
                                       float *out_r);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_process_stereo_block(SphereDauxVst2Processor *processor,
                                      const float *in_l, const float *in_r,
                                      float *out_l, float *out_r, int frames);

/// Same layout and `kind` encoding as `SphereDauxVst3MidiEvent`. VST2 has no
/// `IMidiMapping` indirection, so `kind == 2` becomes a real MIDI status byte:
/// controller `0..=127` → CC, `128` → channel aftertouch, `129` → pitch bend.
typedef struct SphereDauxVst2MidiEvent {
  unsigned int sample_offset;
  unsigned char kind;
  unsigned char channel;
  unsigned char pitch;
  float velocity;
} SphereDauxVst2MidiEvent;

SPHERE_DAUX_VST2_API int sphere_daux_vst2_process_stereo_block_with_midi(
    SphereDauxVst2Processor *processor, const float *in_l, const float *in_r,
    float *out_l, float *out_r, int frames,
    const SphereDauxVst2MidiEvent *events, int event_count);

SPHERE_DAUX_VST2_API int sphere_daux_vst2_process_main_output_block_with_midi(
    SphereDauxVst2Processor *processor, const float *in_l, const float *in_r,
    float *out_interleaved, int frames, int output_channels,
    const SphereDauxVst2MidiEvent *events, int event_count);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_event_input_bus_count(SphereDauxVst2Processor *processor);

/// VST2 has no bus concept: these report `numInputs`/`numOutputs` folded into
/// stereo-width buses so the multi-out mixer strips line up with VST3.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_audio_input_bus_count(SphereDauxVst2Processor *processor);
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_audio_output_bus_count(SphereDauxVst2Processor *processor);
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_main_audio_input_channel_count(
    SphereDauxVst2Processor *processor);
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_main_audio_output_channel_count(
    SphereDauxVst2Processor *processor);
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_bridge_audio_output_channel_count(
    SphereDauxVst2Processor *processor);

/// Fills `out_counts` with the per-bus output channel counts (bus-by-bus, in
/// flatten order) and returns how many were written.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_output_bus_channel_counts(SphereDauxVst2Processor *processor,
                                           int *out_counts, int max_count);

SPHERE_DAUX_VST2_API unsigned long long
sphere_daux_vst2_process_count(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API double
sphere_daux_vst2_last_input_peak(SphereDauxVst2Processor *processor);
SPHERE_DAUX_VST2_API double
sphere_daux_vst2_last_output_peak(SphereDauxVst2Processor *processor);
SPHERE_DAUX_VST2_API double
sphere_daux_vst2_last_difference_peak(SphereDauxVst2Processor *processor);

/// `param_id` is the VST2 parameter index (VST2 has no separate ParamID
/// space), matching the `id` field emitted by
/// `sphere_daux_vst2_list_parameters_json`.
SPHERE_DAUX_VST2_API void
sphere_daux_vst2_set_param(SphereDauxVst2Processor *processor,
                           unsigned int param_id, double value);

SPHERE_DAUX_VST2_API unsigned long long
sphere_daux_vst2_open_editor(SphereDauxVst2Processor *processor,
                             const char *window_id, const char *title,
                             int width, int height);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_close_editor(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_focus_editor(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_set_editor_title(SphereDauxVst2Processor *processor,
                                  const char *title);

// ── GPUI-embedded editor ────────────────────────────────────────────────────

SPHERE_DAUX_VST2_API unsigned long long
sphere_daux_vst2_embed_editor(SphereDauxVst2Processor *processor,
                              unsigned long long parent_handle, int x, int y,
                              int width, int height);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_embed_set_bounds(SphereDauxVst2Processor *processor, int x,
                                  int y, int width, int height);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_embed_refresh(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API unsigned long long
sphere_daux_vst2_embed_attach_hwnd(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_embed_detach(SphereDauxVst2Processor *processor);

/// 1 when the plug-in reports `effCanDo("sizeWindow")` / a resizable editor.
/// VST2 editors are fixed-size unless they drive `audioMasterSizeWindow`.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_editor_resizable(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_embed_is_valid(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_embed_has_visible_ui(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_embed_host_kind(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_embed_take_user_close(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_embed_set_waiting_stage(SphereDauxVst2Processor *processor,
                                         const char *stage);

SPHERE_DAUX_VST2_API void
sphere_daux_vst2_embed_set_instance_label(SphereDauxVst2Processor *processor,
                                          const char *instance_id);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_prepare_editor_view(SphereDauxVst2Processor *processor,
                                     int *out_width, int *out_height);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_take_pending_shell_resize(SphereDauxVst2Processor *processor,
                                           int *out_width, int *out_height);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_embed_content_size(SphereDauxVst2Processor *processor,
                                    int *out_width, int *out_height);

// ── Host-owned view host ────────────────────────────────────────────────────
//
// Mirrors `sphere_daux_vst3_view_*`. The host owns the window; these drive only
// the plug-in's editor. Nothing on this path creates, moves, resizes, or
// destroys a window, so the caller stays the single owner of the editor's
// geometry. Main/UI thread only.

/// Opens the plug-in's editor into `parent_hwnd` (`effEditOpen`), which the
/// caller owns and must keep alive until `sphere_daux_vst2_view_detach`.
///
/// `width`/`height` are the region the host has available; the size reported
/// through `out_width`/`out_height` is the plug-in's own (`effEditGetRect`),
/// which the host is expected to lay out around. Re-attaching to the same
/// window is a no-op that re-reports the size. Returns 1 on success.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_attach(SphereDauxVst2Processor *processor,
                             unsigned long long parent_hwnd, int width,
                             int height, int *out_width, int *out_height);

/// Closes the editor (`effEditClose`). The parent window is untouched. Safe
/// when nothing is attached.
SPHERE_DAUX_VST2_API void
sphere_daux_vst2_view_detach(SphereDauxVst2Processor *processor);

/// 1 while an editor is open through the host-owned path.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_is_attached(SphereDauxVst2Processor *processor);

/// Gives the plug-in's own child window the size the host applied. VST2 has no
/// `onSize` opcode, so laying the child out is the only notification there is.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_set_size(SphereDauxVst2Processor *processor, int width,
                               int height);

/// The editor's current content size (`effEditGetRect`).
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_get_size(SphereDauxVst2Processor *processor,
                               int *out_width, int *out_height);

/// 1 when the editor accepts host-driven resizing (`effCanDo("sizeWindow")`).
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_can_resize(SphereDauxVst2Processor *processor);

/// Applies the VST2 size contract to a proposed content size in place: a fixed
/// editor snaps to the size `effEditGetRect` reported, a resizable one takes
/// what it is offered.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_constrain(SphereDauxVst2Processor *processor,
                                int *io_width, int *io_height);

/// Reads and clears the plug-in's pending `audioMasterSizeWindow` request.
///
/// The request is recorded rather than acted on, so the host decides what it
/// can grant, resizes its own surface, and reports the result back through
/// `sphere_daux_vst2_view_set_size`.
SPHERE_DAUX_VST2_API int
sphere_daux_vst2_view_take_resize_request(SphereDauxVst2Processor *processor,
                                          int *out_width, int *out_height);

/// One `effEditIdle`. VST2 editors repaint and animate only while the host
/// calls this, so the host ticks it on the UI thread while a view is attached.
SPHERE_DAUX_VST2_API void
sphere_daux_vst2_view_idle(SphereDauxVst2Processor *processor);


SPHERE_DAUX_VST2_API int
sphere_daux_vst2_is_valid(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API int
sphere_daux_vst2_get_latency_samples(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API void sphere_daux_vst2_set_process_context(
    SphereDauxVst2Processor *processor, double tempo, int time_sig_num,
    int time_sig_den, long long project_time_samples, double ppq,
    double bar_ppq, int playing, int recording);

/// `out_component` receives the `effGetChunk` bank blob when the plug-in sets
/// `effFlagsProgramChunks`, otherwise a bridge-serialised parameter vector
/// (`"FBV2P"` magic). `out_controller` is always empty — VST2 has no split
/// component/controller — so the caller's existing packing is unchanged.
SPHERE_DAUX_VST2_API int sphere_daux_vst2_get_state(
    SphereDauxVst2Processor *processor, unsigned char **out_component,
    int *out_component_len, unsigned char **out_controller,
    int *out_controller_len);

SPHERE_DAUX_VST2_API int sphere_daux_vst2_set_state(
    SphereDauxVst2Processor *processor, const unsigned char *component_data,
    int component_len, const unsigned char *controller_data,
    int controller_len);

SPHERE_DAUX_VST2_API void sphere_daux_vst2_state_free(unsigned char *data);

SPHERE_DAUX_VST2_API char *
sphere_daux_vst2_list_parameters_json(SphereDauxVst2Processor *processor);

SPHERE_DAUX_VST2_API void sphere_daux_vst2_parameters_json_free(char *data);
/// The `NSWindow*` of this instance's host-owned editor, as an opaque handle.
///
/// 0 on Windows, and 0 whenever no editor is open. The caller passes it to the
/// shared editor-chrome ABI (`sphere_daux_editor_chrome.h`), which addresses the
/// strip by the window it lives in, so one strip implementation serves every
/// format.
SPHERE_DAUX_VST2_API unsigned long long
sphere_daux_vst2_editor_native_window(SphereDauxVst2Processor *processor);

}
