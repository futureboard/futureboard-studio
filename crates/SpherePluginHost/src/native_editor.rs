//! Plain-Rust facade over the C ABI embedded-editor helpers in
//! `sphere_plugin_editor_embed_*` (defined in `vst3backend`). GPUI owns a
//! borderless external window and this module attaches the VST3 IPlugView
//! into a WS_CHILD region under it. The N-API wrapper in `editor_window.rs`
//! is feature-gated; this module ships the native surface as
//! `Result<u64, String>`.
//!
//! Hard rules (per `CLAUDE.md`, hosted and bridged plugins):
//! - These calls must not run on the audio thread.
//! - Every `attach_editor_into_parent` must pair with `detach_editor`.
//! - Bad plugin → `Err(...)`, never panic.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_ulonglong};

#[repr(C)]
struct SpherePluginHostString {
    data: *const c_char,
    len: u64,
}

extern "C" {
    fn sphere_plugin_editor_drain_param_events_json() -> SpherePluginHostString;
    fn sphere_plugin_host_free_string(value: SpherePluginHostString);

    fn sphere_plugin_editor_embed_attach(
        parent_hwnd: c_ulonglong,
        plugin_path: *const c_char,
        class_id: *const c_char,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
    ) -> c_ulonglong;
    fn sphere_plugin_editor_embed_set_bounds(
        handle: c_ulonglong,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
    );
    fn sphere_plugin_editor_embed_detach(handle: c_ulonglong);
    fn sphere_plugin_editor_embed_detach_all();
    fn sphere_plugin_editor_embed_is_valid(handle: c_ulonglong) -> c_int;
    fn sphere_plugin_editor_embed_has_visible_ui(handle: c_ulonglong) -> c_int;
    fn sphere_plugin_editor_embed_host_kind(handle: c_ulonglong) -> c_int;
    fn sphere_plugin_editor_embed_refresh(handle: c_ulonglong);
    fn sphere_plugin_editor_embed_preferred_size(
        handle: c_ulonglong,
        out_width: *mut c_int,
        out_height: *mut c_int,
    ) -> c_int;
    fn sphere_plugin_editor_embed_prepare(
        plugin_path: *const c_char,
        class_id: *const c_char,
        out_width: *mut c_int,
        out_height: *mut c_int,
    ) -> c_ulonglong;
    fn sphere_plugin_editor_embed_cancel_prepare(prepare_id: c_ulonglong);
    fn sphere_plugin_editor_embed_attach_prepared(
        prepare_id: c_ulonglong,
        parent_hwnd: c_ulonglong,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
    ) -> c_ulonglong;
    fn sphere_plugin_editor_embed_host_hwnd(handle: c_ulonglong) -> c_ulonglong;
    fn sphere_plugin_editor_embed_delayed_gpu_refresh(handle: c_ulonglong);
}

/// Which native presentation backs an attached embed session. Exactly one mode
/// is ever active per session — the C++ host never creates both a WS_CHILD
/// embed and an owned tool window at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginEditorPresentationMode {
    /// `WS_CHILD` region under the GPUI window's HWND.
    ChildHwndEmbed,
    /// `WS_POPUP | WS_EX_TOOLWINDOW` owned by the GPUI window (default).
    OwnedToolWindowFallback,
    /// Standalone top-level OS window, modeled on the VST3 SDK editorhost
    /// sample — not owned by or composited under the GPUI window. Generic
    /// escape hatch for editors that won't render in the embedded/overlay modes
    /// (`FUTUREBOARD_PLUGIN_EDITOR_MODE=detached`).
    DetachedNativeWindow,
}

/// Query the presentation mode currently backing an attached editor session.
/// `None` if the handle is unknown / detached.
pub fn editor_presentation_mode(handle: u64) -> Option<PluginEditorPresentationMode> {
    if handle == 0 {
        return None;
    }
    match unsafe { sphere_plugin_editor_embed_host_kind(handle as c_ulonglong) } {
        0 => Some(PluginEditorPresentationMode::ChildHwndEmbed),
        1 => Some(PluginEditorPresentationMode::OwnedToolWindowFallback),
        2 => Some(PluginEditorPresentationMode::DetachedNativeWindow),
        _ => None,
    }
}

/// Region (in physical pixels, relative to the parent window's client area)
/// for the embedded plugin host child window.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmbedRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Attach a VST3 IPlugView into a WS_CHILD host region under `parent_hwnd`
/// (the GPUI editor window's native handle). Returns a non-zero session handle.
///
/// Must be called on the thread that owns `parent_hwnd` (the GPUI UI thread),
/// never the audio thread. `Err` on any failure — the caller renders a GPUI
/// fallback; no panic.
pub fn attach_editor_into_parent(
    parent_hwnd: u64,
    plugin_path: &str,
    class_id: &str,
    region: EmbedRegion,
) -> Result<u64, String> {
    if parent_hwnd == 0 {
        return Err("attach_editor_into_parent: null parent handle".to_string());
    }
    let path = to_cstring("plugin_path", plugin_path.to_string())?;
    let class = to_cstring("class_id", class_id.to_string())?;
    let handle = unsafe {
        sphere_plugin_editor_embed_attach(
            parent_hwnd as c_ulonglong,
            path.as_ptr(),
            class.as_ptr(),
            region.x as c_int,
            region.y as c_int,
            region.width as c_int,
            region.height as c_int,
        )
    };
    if handle == 0 {
        Err("embedded plugin editor failed to attach".to_string())
    } else {
        Ok(handle)
    }
}

/// Reposition / resize the embedded host child region (physical pixels).
pub fn set_editor_region_bounds(handle: u64, region: EmbedRegion) {
    if handle == 0 {
        return;
    }
    unsafe {
        sphere_plugin_editor_embed_set_bounds(
            handle as c_ulonglong,
            region.x as c_int,
            region.y as c_int,
            region.width as c_int,
            region.height as c_int,
        )
    };
}

/// Detach the IPlugView and destroy the host child window. Idempotent.
pub fn detach_editor(handle: u64) {
    if handle == 0 {
        return;
    }
    unsafe { sphere_plugin_editor_embed_detach(handle as c_ulonglong) };
}

/// Tear down every embedded editor session (call on application quit).
pub fn detach_all_embedded_editors() {
    unsafe { sphere_plugin_editor_embed_detach_all() };
}

pub fn editor_is_valid(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    unsafe { sphere_plugin_editor_embed_is_valid(handle as c_ulonglong) != 0 }
}

/// Returns true when the embedded session has a visible host child and either
/// plugin-owned sub-windows or a non-trivial `IPlugView::getSize` after attach.
pub fn editor_has_visible_ui(handle: u64) -> bool {
    if handle == 0 {
        return false;
    }
    unsafe { sphere_plugin_editor_embed_has_visible_ui(handle as c_ulonglong) != 0 }
}

/// Pump paint/size messages and re-sync host geometry (tool window screen position).
/// Call on the GPUI UI thread each frame while the editor is attached.
pub fn refresh_editor_host(handle: u64) {
    if handle == 0 {
        return;
    }
    unsafe { sphere_plugin_editor_embed_refresh(handle as c_ulonglong) };
}

/// Phase 1 of two-phase attach: load plugin, `createView`, `getSize` only.
/// Returns `(prepare_id, preferred_width, preferred_height)`.
pub fn prepare_editor_view(plugin_path: &str, class_id: &str) -> Result<(u64, u32, u32), String> {
    let path = to_cstring("plugin_path", plugin_path.to_string())?;
    let class = to_cstring("class_id", class_id.to_string())?;
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let prepare_id = unsafe {
        sphere_plugin_editor_embed_prepare(
            path.as_ptr(),
            class.as_ptr(),
            &mut width as *mut c_int,
            &mut height as *mut c_int,
        )
    };
    if prepare_id == 0 {
        return Err("prepare_editor_view failed".to_string());
    }
    Ok((prepare_id, width.max(0) as u32, height.max(0) as u32))
}

/// Cancel a pending prepare session (editor closed before attach).
pub fn cancel_prepared_editor(prepare_id: u64) {
    if prepare_id == 0 {
        return;
    }
    unsafe { sphere_plugin_editor_embed_cancel_prepare(prepare_id as c_ulonglong) };
}

/// Phase 2: attach a prepared view into `parent_hwnd` at `region` size.
pub fn attach_prepared_editor(
    prepare_id: u64,
    parent_hwnd: u64,
    region: EmbedRegion,
) -> Result<u64, String> {
    if prepare_id == 0 {
        return Err("attach_prepared_editor: null prepare_id".to_string());
    }
    if parent_hwnd == 0 {
        return Err("attach_prepared_editor: null parent handle".to_string());
    }
    let handle = unsafe {
        sphere_plugin_editor_embed_attach_prepared(
            prepare_id as c_ulonglong,
            parent_hwnd as c_ulonglong,
            region.x as c_int,
            region.y as c_int,
            region.width as c_int,
            region.height as c_int,
        )
    };
    if handle == 0 {
        Err("attach_prepared_editor failed".to_string())
    } else {
        Ok(handle)
    }
}

/// Host child HWND backing an attached embed session (`IPlugView::attached` target).
pub fn editor_host_hwnd(handle: u64) -> Option<u64> {
    if handle == 0 {
        return None;
    }
    let hwnd = unsafe { sphere_plugin_editor_embed_host_hwnd(handle as c_ulonglong) };
    if hwnd == 0 {
        None
    } else {
        Some(hwnd)
    }
}

/// Repeat show/resize/redraw once (~100ms after attach) for GPU-heavy editors.
pub fn delayed_gpu_refresh(handle: u64) {
    if handle == 0 {
        return;
    }
    unsafe { sphere_plugin_editor_embed_delayed_gpu_refresh(handle as c_ulonglong) };
}

pub fn editor_preferred_size(handle: u64) -> Option<(u32, u32)> {
    if handle == 0 {
        return None;
    }
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let ok = unsafe {
        sphere_plugin_editor_embed_preferred_size(
            handle as c_ulonglong,
            &mut width as *mut c_int,
            &mut height as *mut c_int,
        )
    };
    if ok == 0 || width <= 0 || height <= 0 {
        None
    } else {
        Some((width as u32, height as u32))
    }
}

#[derive(Debug, Clone)]
pub struct NativeEditorParamEvent {
    pub window_id: String,
    pub param_id: f64,
    pub value: f64,
}

#[derive(serde::Deserialize)]
struct ParamEventRaw {
    #[serde(rename = "windowId")]
    window_id: String,
    #[serde(rename = "paramId")]
    param_id: f64,
    value: f64,
}

fn to_cstring(label: &str, value: String) -> Result<CString, String> {
    CString::new(value).map_err(|e| format!("{label}: {e}"))
}

/// Drain any pending parameter-change events emitted by the native
/// editor view. Callers should poll this on the UI thread at ~30 Hz.
pub fn drain_plugin_editor_param_events() -> Result<Vec<NativeEditorParamEvent>, String> {
    let native = unsafe { sphere_plugin_editor_drain_param_events_json() };
    if native.data.is_null() {
        return Ok(Vec::new());
    }
    let json = unsafe { CStr::from_ptr(native.data) }
        .to_string_lossy()
        .into_owned();
    unsafe { sphere_plugin_host_free_string(native) };
    let parsed: Vec<ParamEventRaw> =
        serde_json::from_str(&json).map_err(|e| format!("param event json: {e}"))?;
    Ok(parsed
        .into_iter()
        .map(|p| NativeEditorParamEvent {
            window_id: p.window_id,
            param_id: p.param_id,
            value: p.value,
        })
        .collect())
}
