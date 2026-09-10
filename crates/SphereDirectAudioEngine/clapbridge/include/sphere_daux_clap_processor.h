#pragma once

// CLAP runtime bridge.
//
// Mirrors `sphere_daux_vst3_processor.h` / `sphere_daux_vst2_processor.h`
// function-for-function so the Rust `Vst3RuntimeProcessor` dispatches to any of
// the three backends behind one method surface. Read the VST3 header for the
// contract of each call; the notes here only cover where CLAP differs.

#ifdef _WIN32
#define SPHERE_DAUX_CLAP_API __declspec(dllexport)
#else
#define SPHERE_DAUX_CLAP_API __attribute__((visibility("default")))
#endif

extern "C" {

struct SphereDauxClapProcessor;

SPHERE_DAUX_CLAP_API int sphere_daux_clap_bridge_probe(void);

SPHERE_DAUX_CLAP_API const char *sphere_daux_clap_last_error(void);

/// `class_id` is the CLAP plug-in id from the factory descriptor (e.g.
/// `"com.example.synth"`). Empty selects the module's first plug-in.
SPHERE_DAUX_CLAP_API SphereDauxClapProcessor *
sphere_daux_clap_create(const char *plugin_path, const char *class_id,
                        double sample_rate);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_destroy(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_process_stereo_sample(SphereDauxClapProcessor *processor,
                                       float in_l, float in_r, float *out_l,
                                       float *out_r);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_process_stereo_block(SphereDauxClapProcessor *processor,
                                      const float *in_l, const float *in_r,
                                      float *out_l, float *out_r, int frames);

/// Same layout and `kind` encoding as the VST3/VST2 event. Notes become CLAP
/// note events when the plug-in's note port accepts the CLAP dialect, and MIDI
/// events otherwise. `kind == 2` (controller `0..=127` CC, `128` channel
/// aftertouch, `129` pitch bend) always travels as a MIDI event, since CLAP has
/// no first-class controller event.
typedef struct SphereDauxClapMidiEvent {
  unsigned int sample_offset;
  unsigned char kind;
  unsigned char channel;
  unsigned char pitch;
  float velocity;
} SphereDauxClapMidiEvent;

SPHERE_DAUX_CLAP_API int sphere_daux_clap_process_stereo_block_with_midi(
    SphereDauxClapProcessor *processor, const float *in_l, const float *in_r,
    float *out_l, float *out_r, int frames,
    const SphereDauxClapMidiEvent *events, int event_count);

SPHERE_DAUX_CLAP_API int sphere_daux_clap_process_main_output_block_with_midi(
    SphereDauxClapProcessor *processor, const float *in_l, const float *in_r,
    float *out_interleaved, int frames, int output_channels,
    const SphereDauxClapMidiEvent *events, int event_count);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_event_input_bus_count(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_audio_input_bus_count(SphereDauxClapProcessor *processor);
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_audio_output_bus_count(SphereDauxClapProcessor *processor);
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_main_audio_input_channel_count(
    SphereDauxClapProcessor *processor);
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_main_audio_output_channel_count(
    SphereDauxClapProcessor *processor);
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_bridge_audio_output_channel_count(
    SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_output_bus_channel_counts(SphereDauxClapProcessor *processor,
                                           int *out_counts, int max_count);

SPHERE_DAUX_CLAP_API unsigned long long
sphere_daux_clap_process_count(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API double
sphere_daux_clap_last_input_peak(SphereDauxClapProcessor *processor);
SPHERE_DAUX_CLAP_API double
sphere_daux_clap_last_output_peak(SphereDauxClapProcessor *processor);
SPHERE_DAUX_CLAP_API double
sphere_daux_clap_last_difference_peak(SphereDauxClapProcessor *processor);

/// `param_id` is the CLAP `clap_id`. `value` is normalized `0..1`; the bridge
/// denormalizes it against the parameter's own `min_value`/`max_value` before
/// pushing a `CLAP_EVENT_PARAM_VALUE`, because CLAP parameters carry absolute
/// values rather than normalized ones.
SPHERE_DAUX_CLAP_API void
sphere_daux_clap_set_param(SphereDauxClapProcessor *processor,
                           unsigned int param_id, double value);

SPHERE_DAUX_CLAP_API unsigned long long
sphere_daux_clap_open_editor(SphereDauxClapProcessor *processor,
                             const char *window_id, const char *title,
                             int width, int height);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_close_editor(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_focus_editor(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_set_editor_title(SphereDauxClapProcessor *processor,
                                  const char *title);

// ── GPUI-embedded editor ────────────────────────────────────────────────────

SPHERE_DAUX_CLAP_API unsigned long long
sphere_daux_clap_embed_editor(SphereDauxClapProcessor *processor,
                              unsigned long long parent_handle, int x, int y,
                              int width, int height);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_embed_set_bounds(SphereDauxClapProcessor *processor, int x,
                                  int y, int width, int height);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_embed_refresh(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API unsigned long long
sphere_daux_clap_embed_attach_hwnd(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_embed_detach(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_embed_is_valid(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_embed_has_visible_ui(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_embed_host_kind(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_embed_take_user_close(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_embed_set_waiting_stage(SphereDauxClapProcessor *processor,
                                         const char *stage);

SPHERE_DAUX_CLAP_API void
sphere_daux_clap_embed_set_instance_label(SphereDauxClapProcessor *processor,
                                          const char *instance_id);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_prepare_editor_view(SphereDauxClapProcessor *processor,
                                     int *out_width, int *out_height);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_take_pending_shell_resize(SphereDauxClapProcessor *processor,
                                           int *out_width, int *out_height);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_embed_content_size(SphereDauxClapProcessor *processor,
                                    int *out_width, int *out_height);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_editor_resizable(SphereDauxClapProcessor *processor);

// ── Host-owned view host ────────────────────────────────────────────────────
//
// Mirrors `sphere_daux_vst3_view_*`. The host owns the window; these drive only
// the `clap.gui` extension. Nothing on this path creates, moves, resizes, or
// destroys a window, so the caller stays the single owner of the editor's
// geometry. Main/UI thread only.

/// Runs the `clap.gui` create → set_scale → get_size → set_parent → show
/// sequence against `parent_hwnd`, which the caller owns and must keep alive
/// until `sphere_daux_clap_view_detach`.
///
/// `width`/`height` are the region the host has available; the size reported
/// through `out_width`/`out_height` is the plug-in's own (`get_size`), which the
/// host is expected to lay out around. Re-attaching to the same window is a
/// no-op that re-reports the size. Returns 1 on success.
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_attach(SphereDauxClapProcessor *processor,
                             unsigned long long parent_hwnd, int width,
                             int height, int *out_width, int *out_height);

/// Hides and destroys the GUI (`hide` + `destroy`). The parent window is
/// untouched. Safe when nothing is attached.
SPHERE_DAUX_CLAP_API void
sphere_daux_clap_view_detach(SphereDauxClapProcessor *processor);

/// 1 while a GUI is attached through the host-owned path.
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_is_attached(SphereDauxClapProcessor *processor);

/// Tells the GUI the size the host has given it (`set_size`). Call after the
/// host surface has actually been resized, never before.
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_set_size(SphereDauxClapProcessor *processor, int width,
                               int height);

/// The GUI's current content size (`get_size`).
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_get_size(SphereDauxClapProcessor *processor,
                               int *out_width, int *out_height);

/// 1 when the GUI accepts host-driven resizing (`can_resize`).
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_can_resize(SphereDauxClapProcessor *processor);

/// Applies the CLAP size contract to a proposed content size in place: a fixed
/// GUI snaps to its own size, a resizable one goes through `adjust_size`.
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_constrain(SphereDauxClapProcessor *processor,
                                int *io_width, int *io_height);

/// Reads and clears the plug-in's pending `clap_host_gui->request_resize`, if
/// any.
///
/// The request is recorded rather than acted on, so the host decides what it
/// can grant, resizes its own surface, and reports the result back through
/// `sphere_daux_clap_view_set_size`.
SPHERE_DAUX_CLAP_API int
sphere_daux_clap_view_take_resize_request(SphereDauxClapProcessor *processor,
                                          int *out_width, int *out_height);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_is_valid(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API int
sphere_daux_clap_get_latency_samples(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API void sphere_daux_clap_set_process_context(
    SphereDauxClapProcessor *processor, double tempo, int time_sig_num,
    int time_sig_den, long long project_time_samples, double ppq,
    double bar_ppq, int playing, int recording);

/// `out_component` receives the `clap.state` blob. `out_controller` is always
/// empty — CLAP has a single state stream — so the caller's existing packing is
/// unchanged.
SPHERE_DAUX_CLAP_API int sphere_daux_clap_get_state(
    SphereDauxClapProcessor *processor, unsigned char **out_component,
    int *out_component_len, unsigned char **out_controller,
    int *out_controller_len);

SPHERE_DAUX_CLAP_API int sphere_daux_clap_set_state(
    SphereDauxClapProcessor *processor, const unsigned char *component_data,
    int component_len, const unsigned char *controller_data,
    int controller_len);

SPHERE_DAUX_CLAP_API void sphere_daux_clap_state_free(unsigned char *data);

SPHERE_DAUX_CLAP_API char *
sphere_daux_clap_list_parameters_json(SphereDauxClapProcessor *processor);

SPHERE_DAUX_CLAP_API void sphere_daux_clap_parameters_json_free(char *data);
/// The `NSWindow*` of this instance's host-owned editor, as an opaque handle.
///
/// 0 on Windows, and 0 whenever no editor is open. The caller passes it to the
/// shared editor-chrome ABI (`sphere_daux_editor_chrome.h`), which addresses the
/// strip by the window it lives in, so one strip implementation serves every
/// format.
SPHERE_DAUX_CLAP_API unsigned long long
sphere_daux_clap_editor_native_window(SphereDauxClapProcessor *processor);

}
