//! Rodhareist's native editor window.
//!
//! The editor of one Rodhareist insert — Futureboard's flagship guitar
//! multi-effect — drawn natively with GPUI inside the shared
//! [`native_plugin_shell`], replacing the old CEF/React `editorui/` bundle.
//!
//! It talks to the plug-in the way the CEF editor did, through the same host
//! ops `plugin_ops.rs` injects:
//!
//! * every edit is a wire param, sent with `forward_param` — folded into
//!   Studio's state mirror (what the project saves and a restarted host
//!   replays) and marking the project edited. [`wire_diff`] computes the
//!   edits generically from `rodharerist::ui_values`, so every one of the
//!   120-ish wire parameters (including the reorderable `path_slot_*` rack)
//!   is covered without a field-by-field diff.
//! * an Impulse Response (`.wav`) is read from disk and sent with `load_ir`;
//!   a NAM capture (`.nam`, which is JSON) is read and sent with
//!   `load_nam_capture`. The host replies asynchronously; `plugin_ops.rs`
//!   routes the result back here through [`Self::notify_ir_load_result`] /
//!   [`Self::notify_nam_capture_result`].
//!
//! Every card (`rodhareist_panel_dynamics`, `rodhareist_panel_drive_amp`,
//! `rodhareist_panel_cab_capture`, `rodhareist_panel_modulation`,
//! `rodhareist_panel_timefx`) is a pure presentational function taking plain
//! values and closures — this window is the only thing that knows about
//! `rodharerist::Params` and the host ops. [`rodhareist_panel`] in this
//! module composes them.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, px, size, App, AppContext, Context, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind,
};
use rodharerist::{EqModel, Params};

use crate::components::builtin_plugin_editor_window::{
    BuiltinEditorHostOps, BuiltinIrLoadRequest, BuiltinNamLoadRequest, PluginInstanceKey,
};
use crate::components::native_plugin_shell::{
    native_plugin_shell, NativeBuiltinEditor, ShellIdentity, ShellMeter, SHELL_METER_INTERVAL,
};
use crate::components::rodhareist_panel::rodhareist_panel;

pub const RODHAREIST_WINDOW_WIDTH: f32 = 1_180.0;
pub const RODHAREIST_WINDOW_HEIGHT: f32 = 860.0;
pub const RODHAREIST_WINDOW_MIN_WIDTH: f32 = 860.0;
pub const RODHAREIST_WINDOW_MIN_HEIGHT: f32 = 560.0;

/// `next` as wire edits against `current`: every wire id whose value
/// differs, mapped to its wire index. Generic over the *entire* parameter
/// surface via `rodharerist::ui_values`/`ui_param_index` — a new knob added
/// to the wire table needs no change here.
pub(crate) fn wire_diff(current: &Params, next: &Params) -> Vec<(u32, f32)> {
    let before: std::collections::HashMap<&str, f32> =
        rodharerist::ui_values(current).into_iter().collect();
    rodharerist::ui_values(next)
        .into_iter()
        .filter(|(id, value)| before.get(id) != Some(value))
        .filter_map(|(id, value)| rodharerist::ui_param_index(id).map(|index| (index, value)))
        .collect()
}

/// A file load in flight for the Cabinet IR or the NAM Capture slot.
#[derive(Clone, Debug, Default)]
pub(crate) struct LoadStatus {
    pub loading: bool,
    pub error: Option<String>,
    /// The loaded file's display name (IR) — paired with its length in
    /// `ir_seconds` once the host answers.
    pub loaded_name: Option<String>,
    pub ir_seconds: f32,
}

pub struct RodhareistEditorWindow {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    params: Params,
    ir_status: LoadStatus,
    nam_status: LoadStatus,
    meter: ShellMeter,
    /// Cleared when the window closes, ending the meter timer.
    alive: Rc<Cell<bool>>,
}

impl RodhareistEditorWindow {
    pub fn new(
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut window = Self {
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            params: rodharerist::default_params(),
            ir_status: LoadStatus::default(),
            nam_status: LoadStatus::default(),
            meter: ShellMeter::default(),
            alive: Rc::new(Cell::new(true)),
        };
        window.sync_from_mirror(cx);
        window.start_meter(cx);
        window
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    /// Re-reads the insert's params from Studio's state mirror — after an
    /// undo, a preset, or a project reload.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        self.params = crate::components::builtin_plugin_editor::builtin_rodhareist_params(
            &self.key.insert_id,
        )
        .unwrap_or_else(rodharerist::default_params);
        cx.notify();
    }

    /// Rebinds the window to another insert, keeping the OS window.
    pub fn rebind(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        if key != self.key {
            self.key = key;
            self.ir_status = LoadStatus::default();
            self.nam_status = LoadStatus::default();
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
    }

    /// Sends what changed between the window's params and `next` as wire
    /// edits. Each one reaches the host DSP, the state mirror, and the
    /// project's dirty flag through `forward_param`.
    pub(crate) fn set_params(&mut self, next: Params, cx: &mut Context<Self>) {
        let edits = wire_diff(&self.params, &next);
        self.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        cx.notify();
    }

    /// Builds a closure for a knob/trim change: applies `apply` to a clone of
    /// the current params and forwards the diff. Handed to the pure
    /// `rodhareist_panel_*` card functions as their `on_*` callbacks.
    pub(crate) fn knob_cb(
        &self,
        cx: &Context<Self>,
        apply: impl Fn(&mut Params, f32) + 'static,
    ) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |value: &f32, _window, app: &mut App| {
            let value = *value;
            let _ = entity.update(app, |this, cx| {
                let mut next = this.params.clone();
                apply(&mut next, value);
                this.set_params(next, cx);
            });
        }
    }

    /// Builds a closure for a toggle/stepper click: applies `apply` to a
    /// clone of the current params and forwards the diff.
    pub(crate) fn click_cb(
        &self,
        cx: &Context<Self>,
        apply: impl Fn(&mut Params) + 'static,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                let mut next = this.params.clone();
                apply(&mut next);
                this.set_params(next, cx);
            });
        }
    }

    /// A clone of the current params, for the pure panel functions to read.
    pub(crate) fn params_snapshot(&self) -> Params {
        self.params.clone()
    }

    pub(crate) fn host_ops_forward_param(
        &self,
    ) -> Option<crate::components::builtin_plugin_editor_window::BuiltinParamForwarder> {
        self.host_ops.forward_param.clone()
    }

    pub(crate) fn ir_loaded_info(&self) -> Option<(String, f32)> {
        self.ir_status
            .loaded_name
            .clone()
            .map(|name| (name, self.ir_status.ir_seconds))
    }
    pub(crate) fn ir_loading(&self) -> bool {
        self.ir_status.loading
    }
    pub(crate) fn ir_error(&self) -> Option<String> {
        self.ir_status.error.clone()
    }
    pub(crate) fn nam_loaded_info(&self) -> Option<String> {
        self.nam_status.loaded_name.clone()
    }
    pub(crate) fn nam_loading(&self) -> bool {
        self.nam_status.loading
    }
    pub(crate) fn nam_error(&self) -> Option<String> {
        self.nam_status.error.clone()
    }

    /// Click handler that opens the IR file picker, for the Cabinet card's
    /// "Load IR…" button.
    pub(crate) fn load_ir_cb(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| this.start_ir_load(cx));
        }
    }

    /// Click handler that opens the NAM capture file picker, for the NAM
    /// card's "Load Capture…" button.
    pub(crate) fn load_nam_cb(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| this.start_nam_load(cx));
        }
    }

    fn start_ir_load(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            self.ir_status = LoadStatus::default();
            self.ir_status.loading = true;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Load Impulse Response")
                    .add_filter("Cabinet IR", &["wav", "wave"])
                    .pick_file()
                    .await
                else {
                    let _ = this.update(cx, |this, cx| {
                        this.ir_status.loading = false;
                        cx.notify();
                    });
                    return;
                };
                let path = handle.path().to_path_buf();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "impulse.wav".to_string());
                let read = cx
                    .background_spawn({
                        let path = path.clone();
                        async move { std::fs::read(&path) }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| match read {
                    Ok(bytes) => {
                        if let Some(load) = this.host_ops.load_ir.clone() {
                            load(&this.key.clone(), BuiltinIrLoadRequest { name, bytes });
                        } else {
                            this.ir_status.loading = false;
                            this.ir_status.error =
                                Some("The plug-in host is not running yet.".into());
                        }
                        cx.notify();
                    }
                    Err(error) => {
                        this.ir_status.loading = false;
                        this.ir_status.error = Some(format!("{}", error));
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        #[cfg(not(feature = "native-dialogs"))]
        {
            self.ir_status.error = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    fn start_nam_load(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            self.nam_status = LoadStatus::default();
            self.nam_status.loading = true;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Load NAM Capture")
                    .add_filter("Neural Amp Modeler", &["nam"])
                    .pick_file()
                    .await
                else {
                    let _ = this.update(cx, |this, cx| {
                        this.nam_status.loading = false;
                        cx.notify();
                    });
                    return;
                };
                let path: PathBuf = handle.path().to_path_buf();
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "capture".to_string());
                let read = cx
                    .background_spawn({
                        let path = path.clone();
                        async move { std::fs::read_to_string(&path) }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| match read {
                    Ok(json) => {
                        if let Some(load) = this.host_ops.load_nam_capture.clone() {
                            load(
                                &this.key.clone(),
                                BuiltinNamLoadRequest {
                                    name,
                                    json,
                                    // Auto-upgraded by the DSP when the model
                                    // includes a cab; a stereo-width toggle
                                    // can be added to the card later.
                                    stereo: false,
                                    full_rig: false,
                                },
                            );
                        } else {
                            this.nam_status.loading = false;
                            this.nam_status.error =
                                Some("The plug-in host is not running yet.".into());
                        }
                        cx.notify();
                    }
                    Err(error) => {
                        this.nam_status.loading = false;
                        this.nam_status.error = Some(format!("{}", error));
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        #[cfg(not(feature = "native-dialogs"))]
        {
            self.nam_status.error =
                Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    /// Routed from `plugin_ops.rs` when the host answers a `load_ir` call.
    pub fn notify_ir_load_result(
        &mut self,
        ok: bool,
        name: &str,
        error: Option<&str>,
        frames: u64,
        cx: &mut Context<Self>,
    ) {
        self.ir_status.loading = false;
        if ok {
            self.ir_status.loaded_name = Some(name.to_string());
            self.ir_status.ir_seconds = frames as f32 / self.sample_rate_hint();
            self.ir_status.error = None;
        } else {
            self.ir_status.error = Some(error.unwrap_or("load failed").to_string());
        }
        cx.notify();
    }

    /// Routed from `plugin_ops.rs` when the host answers a `load_nam_capture`
    /// call.
    pub fn notify_nam_capture_result(
        &mut self,
        ok: bool,
        name: &str,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.nam_status.loading = false;
        if ok {
            self.nam_status.loaded_name = Some(name.to_string());
            self.nam_status.error = None;
        } else {
            self.nam_status.error = Some(error.unwrap_or("load failed").to_string());
        }
        cx.notify();
    }

    /// Best-effort sample rate for turning an IR's frame count into seconds
    /// for display only — never used by the audio path.
    fn sample_rate_hint(&self) -> f32 {
        self.host_ops
            .host_status_source
            .as_ref()
            .and_then(|source| source(&self.key))
            .map(|(sample_rate, _block_frames, _latency_samples, _tempo_bpm)| sample_rate as f32)
            .filter(|rate| *rate > 0.0)
            .unwrap_or(48_000.0)
    }

    /// Polls the insert's output meter at the CEF editors' telemetry rate,
    /// redrawing only when the reading moves.
    fn start_meter(&mut self, cx: &mut Context<Self>) {
        let alive = self.alive.clone();
        cx.spawn(async move |this, cx| {
            while alive.get() {
                cx.background_executor().timer(SHELL_METER_INTERVAL).await;
                let keep = this.update(cx, |this, cx| {
                    let key = this.key.clone();
                    if this.meter.poll(this.host_ops.meter_source.as_ref(), &key) {
                        cx.notify();
                    }
                });
                if keep.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

impl NativeBuiltinEditor for RodhareistEditorWindow {
    fn rebind_insert(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        self.rebind(key, identity, host_ops, cx);
    }
}

impl Render for RodhareistEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let content = rodhareist_panel(self, cx);

        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(native_plugin_shell(
                "rodhareist-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    let _ = entity.update(cx, |_this, _cx| {});
                    on_close(window, cx);
                },
                content,
            ))
    }
}

/// Opens with a track's Rodhareist insert, the whole signal chain visible as
/// a stack of cards. Narrower, cards wrap; the title/identity bars stay put.
pub fn open_rodhareist_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<RodhareistEditorWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(RODHAREIST_WINDOW_WIDTH), px(RODHAREIST_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(RODHAREIST_WINDOW_MIN_WIDTH),
        px(RODHAREIST_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| RodhareistEditorWindow::new(key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

/// Display label for an [`EqModel`] — shared by the dynamics card and
/// anything else that needs to name the EQ slot's current voicing.
pub(crate) fn eq_model_label(model: EqModel) -> &'static str {
    match model {
        EqModel::Studio => "Studio EQ",
        EqModel::Vintage => "Vintage EQ",
        EqModel::Modern => "Modern EQ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_diff_is_empty_for_identical_params() {
        let p = rodharerist::default_params();
        assert!(wire_diff(&p, &p).is_empty());
    }

    #[test]
    fn wire_diff_reports_only_the_field_that_changed() {
        let before = rodharerist::default_params();
        let mut after = before.clone();
        after.amp_gain = after.amp_gain + 1.0;
        let diff = wire_diff(&before, &after);
        assert_eq!(diff.len(), 1);
        let (index, value) = diff[0];
        assert_eq!(index, rodharerist::ui_param_index("amp_gain").unwrap());
        assert_eq!(value, after.amp_gain);
    }

    #[test]
    fn wire_diff_covers_a_stage_order_change() {
        let before = rodharerist::default_params();
        let mut after = before.clone();
        after.stage_order = [None; rodharerist::PATH_SLOTS];
        after.stage_order[0] = Some(rodharerist::StageKind::Wah);
        let diff = wire_diff(&before, &after);
        assert!(!diff.is_empty());
    }
}
