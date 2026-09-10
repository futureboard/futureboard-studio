#pragma once
// The plug-in editor's chrome strip — the tab strip and control row that sit
// above a plug-in's own view — as a C surface shared by every format bridge.
//
// Implemented once, in editor_mac_shell.mm, and linked into the VST3 bridge;
// the VST2 and CLAP bridges reach it from their own static libraries because
// every one of them ends up in the same binary. One implementation is the whole
// point: a VST2 editor and a VST3 editor are the same window to the user, and a
// second copy of this drawing is a second thing to keep in step.
//
// # Why the window, not the processor
//
// Each bridge has a processor type of its own and none of them can name the
// others'. What they do have in common is the thing the strip actually lives
// in: an `NSWindow` the bridge created for the plug-in's editor. So the strip
// is addressed by that window, passed as an opaque handle, and the bridges stay
// unaware of each other.
//
// A handle of 0, or one whose window has no shell in it, is not an error —
// it means there is no editor open right now, and every call below is a no-op.
//
// # Contract
//
// The host draws and decides nothing. Labels arrive already formatted and
// colours already resolved, because the studio is the only place that knows
// what a preset is called, what a latency reads as, or which grey a hovered
// control is. Presses go back out through `..._take_action` for the studio to
// apply.
//
// An update is staged across several calls and becomes visible only at
// `..._commit`, so a half-sent one never reaches the screen.
//
// Main thread only.

#ifdef __cplusplus
extern "C" {
#endif

/// Combined height of the chrome bands, in points. 0 where there is no strip on
/// this platform, which is what makes a window sized for one safe everywhere.
double sphere_daux_editor_chrome_height(void);

/// Begin an update, discarding any half-staged one.
void sphere_daux_editor_chrome_begin(unsigned long long native_window);

/// The parts that are not lists. `active` is the insert's on/off; the labels are
/// finished, and `active_tab` is the insert id this window is showing.
///
/// `shows_controls` is 0 for an editor with no insert behind it — an ARA plug-in
/// is bound to a clip, so it has no bypass, no per-slot CPU or latency and no
/// insert-keyed presets. The control row is then dropped and the window gives
/// its height back to the plug-in, rather than drawing controls that would do
/// nothing.
void sphere_daux_editor_chrome_set_header(
    unsigned long long native_window,
    int                active,
    const char*        preset_label,
    const char*        cpu_label,
    const char*        latency_label,
    const char*        active_tab,
    int                shows_controls);

/// Append one preset menu row, in menu order.
void sphere_daux_editor_chrome_add_preset(
    unsigned long long native_window,
    const char*        name,
    int                selected);

/// Append one tab, in slot order.
void sphere_daux_editor_chrome_add_tab(
    unsigned long long native_window,
    const char*        insert_id,
    const char*        display_name,
    int                insert_number);

/// Resolved theme colours, packed 0xRRGGBBAA, in `EditorChromePalette` order.
void sphere_daux_editor_chrome_set_palette(
    unsigned long long  native_window,
    const unsigned int* colors,
    int                 count);

/// Apply the staged update and repaint. `window_title` also retitles the window
/// when non-empty.
void sphere_daux_editor_chrome_commit(
    unsigned long long native_window,
    const char*        window_title);

/// Drain one queued press. Returns 0 when the queue is empty; otherwise fills
/// the outputs and returns 1. `out_id` is NUL-terminated within
/// `out_id_capacity` and is empty for controls that are not per-tab.
int sphere_daux_editor_chrome_take_action(
    unsigned long long native_window,
    int*               out_kind,
    int*               out_value,
    char*              out_id,
    int                out_id_capacity);

#ifdef __cplusplus
} // extern "C"
#endif
