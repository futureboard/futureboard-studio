#pragma once

#ifdef _WIN32
#  define SPHERE_PLUGIN_HOST_API __declspec(dllexport)
#else
#  define SPHERE_PLUGIN_HOST_API __attribute__((visibility("default")))
#endif

extern "C" {

struct SpherePluginHostString {
  const char* data;
  unsigned long long len;
};

SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_vst3_scan_path_json(const char* path);
/// True once a VST3 scan left a bus probe running inside a plug-in past its
/// deadline. The process must then end without normal teardown (its module is
/// still in use), right after its result is written.
SPHERE_PLUGIN_HOST_API bool sphere_vst3_scan_left_work_running(void);
SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_clap_scan_path_json(const char* path);
/// Scan a folder (or a single module) for 64-bit VST2 plug-ins. Windows
/// candidates are `.dll` files probed for a VST2 entry point; macOS candidates
/// are `.vst` bundles. Shell modules expand to one entry per sub-plug-in.
SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_vst2_scan_path_json(const char* path);
SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_au_scan_json();
SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_au_validate_component_json(
    const char* component_id);
SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_plugin_editor_drain_param_events_json();
SPHERE_PLUGIN_HOST_API void sphere_plugin_host_free_string(SpherePluginHostString value);

// ── Embedded editor (GPUI-hosted) ───────────────────────────────────────────
// Attach a VST3 IPlugView into a WS_CHILD region under a caller-provided parent
// window (the GPUI borderless editor window's native HWND). No NanoVG shell.
// Must be called on the thread that owns `parent_hwnd`. Returns a non-zero
// session handle on success, 0 on failure (never throws).
SPHERE_PLUGIN_HOST_API unsigned long long sphere_plugin_editor_embed_attach(
    unsigned long long parent_hwnd,
    const char* plugin_path,
    const char* class_id,
    int x,
    int y,
    int width,
    int height);
SPHERE_PLUGIN_HOST_API void sphere_plugin_editor_embed_set_bounds(
    unsigned long long handle,
    int x,
    int y,
    int width,
    int height);
SPHERE_PLUGIN_HOST_API void sphere_plugin_editor_embed_detach(unsigned long long handle);
SPHERE_PLUGIN_HOST_API void sphere_plugin_editor_embed_detach_all();
SPHERE_PLUGIN_HOST_API int sphere_plugin_editor_embed_is_valid(unsigned long long handle);
SPHERE_PLUGIN_HOST_API int sphere_plugin_editor_embed_has_visible_ui(unsigned long long handle);
// Presentation mode currently backing this embed session:
//   0 = ChildHwndEmbed (WS_CHILD), 1 = OwnedToolWindowFallback (WS_POPUP tool),
//  -1 = no such session. Exactly one mode is ever active per session.
SPHERE_PLUGIN_HOST_API int sphere_plugin_editor_embed_host_kind(unsigned long long handle);
// Reposition (if needed), onSize, WM_SIZE/WM_SHOWWINDOW to plugin children, and pump
// pending paint messages. Call from the GPUI UI thread while Attached.
SPHERE_PLUGIN_HOST_API void sphere_plugin_editor_embed_refresh(unsigned long long handle);
SPHERE_PLUGIN_HOST_API int sphere_plugin_editor_embed_preferred_size(
    unsigned long long handle,
    int* out_width,
    int* out_height);
// Two-phase attach: createView + getSize without attached(); returns prepare_id.
SPHERE_PLUGIN_HOST_API unsigned long long sphere_plugin_editor_embed_prepare(
    const char* plugin_path,
    const char* class_id,
    int* out_width,
    int* out_height);
SPHERE_PLUGIN_HOST_API void sphere_plugin_editor_embed_cancel_prepare(unsigned long long prepare_id);
SPHERE_PLUGIN_HOST_API unsigned long long sphere_plugin_editor_embed_attach_prepared(
    unsigned long long prepare_id,
    unsigned long long parent_hwnd,
    int x,
    int y,
    int width,
    int height);
SPHERE_PLUGIN_HOST_API unsigned long long sphere_plugin_editor_embed_host_hwnd(unsigned long long handle);
SPHERE_PLUGIN_HOST_API void sphere_plugin_editor_embed_delayed_gpu_refresh(unsigned long long handle);

}
