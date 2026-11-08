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
//! The view (`rodhareist_panel`, over the block model in
//! `rodhareist_blocks`) is pure: it reads this window's params and hands
//! edits back through the `*_cb` builders here — this window is the only
//! thing that knows about the host ops. Factory presets come from
//! `rodharerist::factory_presets` and load as ordinary wire edits.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, div, px, size,
};
use rodharerist::Params;

use crate::components::builtin_plugin_editor::{
    FileLoad, RODHAREIST_FILES_FOLDER, RodhareistFile, file_load,
};
use crate::components::builtin_plugin_editor_window::{
    BuiltinEditorHostOps, BuiltinIrLoadRequest, BuiltinNamLoadRequest, PluginInstanceKey,
};
use crate::components::native_plugin_shell::{
    NativeBuiltinEditor, SHELL_METER_INTERVAL, ShellIdentity, ShellMeter,
    claim_editor_transport_key, native_plugin_shell, swallow_editor_transport_key_up,
};
use crate::components::plate;
use crate::components::rodhareist_panel::{
    ChainFocus, Page, RodFields, RodUi, chain_frame, rodhareist_panel, tapped_interval,
};
use crate::components::text_input::{
    TextInputAction, TextInputState, bind_mouse_selection, text_field_with_callbacks_and_ime,
};

/// The editor at its design size, and the smallest it may get.
pub const RODHAREIST_WINDOW_WIDTH: f32 = plate::WINDOW_SIZE.0;
pub const RODHAREIST_WINDOW_HEIGHT: f32 = plate::WINDOW_SIZE.1;
pub const RODHAREIST_WINDOW_MIN_WIDTH: f32 = plate::MIN_WINDOW_SIZE.0;
pub const RODHAREIST_WINDOW_MIN_HEIGHT: f32 = plate::MIN_WINDOW_SIZE.1;

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

/// The insert's input and output levels (linear) for the signal-path
/// meters, read from the host's meter frame on the shell's timer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct IoMeter {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

impl IoMeter {
    /// Whether the reading moved enough to be worth a redraw.
    fn poll(
        &mut self,
        source: Option<&crate::components::builtin_plugin_editor_window::BuiltinMeterSource>,
        key: &PluginInstanceKey,
    ) -> bool {
        let next = source
            .and_then(|source| source(key))
            .map(|f| IoMeter {
                in_peak: f.in_peak.clamp(0.0, 2.0),
                in_rms: f.in_rms.clamp(0.0, 2.0),
                out_peak: f.out_peak.clamp(0.0, 2.0),
                out_rms: f.out_rms.clamp(0.0, 2.0),
                in_clip: f.in_clip,
                out_clip: f.out_clip,
            })
            .unwrap_or_default();
        let moved = (next.in_peak - self.in_peak).abs() > 0.004
            || (next.in_rms - self.in_rms).abs() > 0.004
            || (next.out_peak - self.out_peak).abs() > 0.004
            || (next.out_rms - self.out_rms).abs() > 0.004
            || next.in_clip != self.in_clip
            || next.out_clip != self.out_clip;
        if moved {
            *self = next;
        }
        moved
    }
}

/// The factory preset `params` were loaded from: the bank entry equal to
/// them, ignoring the global power and input trim a preset load keeps.
fn matching_preset(params: &Params) -> Option<usize> {
    rodharerist::factory_presets()
        .iter()
        .position(|preset| preset_applied(params, &preset.params) == *params)
}

/// `preset` as loaded over `current`: everything from the preset except the
/// insert's power, input trim and loaded capture and IR, which belong to the
/// player's rig, not the tone.
fn preset_applied(current: &Params, preset: &Params) -> Params {
    let mut next = preset.clone();
    next.power = current.power;
    next.input_trim_db = current.input_trim_db;
    next.nam_file = current.nam_file.clone();
    next.ir_file = current.ir_file.clone();
    next
}

/// A file load this window started for the Cabinet IR or the NAM Capture
/// slot: in flight, or failed before it reached the host. What is loaded
/// is the project's (the mirrored file names) and the host's answer
/// (`builtin_plugin_editor::file_load`), not this window's.
#[derive(Clone, Debug, Default)]
pub(crate) struct LoadStatus {
    pub loading: bool,
    pub error: Option<String>,
}

/// The name Rodhareist's presets are filed under, as the registry lists it.
const PRESET_PLUGIN_NAME: &str = "Rodhareist";

/// `Documents/Futureboard Studio/Presets/Plug-ins/Rodhareist`: every
/// plug-in's user presets sit in a folder named for it there, Studio's
/// built-in and the external builds alike.
fn user_preset_dir() -> PathBuf {
    SpherePluginHost::preset::user_preset_dir(PRESET_PLUGIN_NAME)
        .unwrap_or_else(|| SpherePluginHost::registry::user_preset_root().join(PRESET_PLUGIN_NAME))
}

fn list_user_presets() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(user_preset_dir()) else {
        return Vec::new();
    };
    let mut presets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if !ext.eq_ignore_ascii_case("pst") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        presets.push((name.to_string(), path));
    }
    presets.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    presets
}

fn preset_file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let stem = stem.trim().trim_matches('.').to_string();
    if stem.is_empty() {
        "My Rig".to_string()
    } else {
        stem
    }
}

/// Whether saving under `stem` must stop and ask first: another user preset
/// already has that name. Saving the loaded preset under its own name
/// updates it, and a name the user was already warned about goes ahead.
/// Saving under a different name never touches the preset that was loaded.
fn save_needs_confirm(
    stem: &str,
    exists: bool,
    loaded: Option<&str>,
    confirmed: Option<&str>,
) -> bool {
    exists && loaded != Some(stem) && confirmed != Some(stem)
}

fn display_name(file_name: &str) -> String {
    std::path::Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_string())
}

/// Reads a picked capture or IR and keeps a copy in Rodhareist's folder,
/// reusing an identical file already there. Returns the stored name and the
/// bytes. Background-thread only (filesystem I/O).
#[cfg(feature = "native-dialogs")]
fn store_user_file(
    file: RodhareistFile,
    source: &std::path::Path,
) -> Result<(String, Vec<u8>), String> {
    use crate::components::builtin_plugin_files as files;
    let source_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the selected file has no name".to_string())?;
    let bytes =
        std::fs::read(source).map_err(|e| format!("could not read {}: {e}", source.display()))?;
    let dir = files::plugin_files_root(RODHAREIST_FILES_FOLDER).join(file.kind().dir_name());
    let name = crate::components::builtin_plugin_editor_window::import_user_file_bytes(
        &dir,
        file.kind(),
        source_name,
        &bytes,
    )?;
    Ok((name, bytes))
}

/// A user preset the editor loaded, and the params that load applied.
struct LoadedUserPreset {
    name: String,
    params: Params,
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
    io: IoMeter,
    /// The factory preset last loaded, or matched on open. Shown as
    /// "edited" once the params move away from it.
    preset: Option<usize>,
    /// `.pst` files in this plug-in's user preset folder.
    user_presets: Vec<(String, PathBuf)>,
    /// The user preset last loaded, and the params it applied, so the
    /// header can say when the rig has moved off that file.
    user_preset: Option<LoadedUserPreset>,
    /// The last save or load failure, shown on the Home preset list.
    preset_note: Option<String>,
    /// The name Save last stopped on because another user preset already
    /// has it. Saving that name again replaces the file.
    replace_confirm: Option<String>,
    /// The chain slot the block editor shows. Editor-local view state —
    /// never saved, never sent to the plug-in.
    pub(crate) chain_focus: Option<ChainFocus>,
    /// The page, footswitch map and display options — view state too.
    pub(crate) ui: RodUi,
    /// Explore's block search. Studio's text field: the OS IME (Thai, CJK,
    /// dead keys), caret, selection and clipboard all work in it.
    pub(crate) search: TextInputState,
    /// Home's preset name, saved as the `.pst` stem; never a wire parameter.
    pub(crate) preset_name: TextInputState,
    /// The insert's telemetry block (`rodharerist::telemetry`): per-channel
    /// levels and the tuner.
    pads: Option<[f32; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS]>,
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
            io: IoMeter::default(),
            preset: None,
            user_presets: Vec::new(),
            user_preset: None,
            preset_note: None,
            replace_confirm: None,
            chain_focus: None,
            ui: RodUi::default(),
            search: TextInputState::new("rodhareist-search", cx.focus_handle())
                .with_placeholder("Search blocks")
                .with_accessible_label("Search blocks"),
            preset_name: TextInputState::new("rodhareist-preset-name", cx.focus_handle())
                .with_placeholder("Preset name")
                .with_accessible_label("Preset name"),
            pads: None,
            alive: Rc::new(Cell::new(true)),
        };
        plate::ensure_fonts(cx);
        window.sync_from_mirror(cx);
        window.preset = matching_preset(&window.params);
        window.refresh_user_presets();
        let name = window.user_preset_save_name();
        window.preset_name.set_value(name);
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
            self.chain_focus = None;
            self.preset = None;
            self.user_preset = None;
            self.preset_note = None;
            self.replace_confirm = None;
            self.pads = None;
            self.ui = RodUi::default();
            self.search.set_value("");
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
        if self.preset.is_none() {
            self.preset = matching_preset(&self.params);
        }
        self.refresh_user_presets();
        let name = self.user_preset_save_name();
        self.preset_name.set_value(name);
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

    /// Slider handler for one wire parameter: maps the slider's 0..1
    /// position onto `min..max` and forwards the edit.
    pub(crate) fn slider_cb(
        &self,
        cx: &Context<Self>,
        id: &'static str,
        min: f32,
        max: f32,
    ) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |norm: &f32, _window, app: &mut App| {
            let value = min + norm.clamp(0.0, 1.0) * (max - min);
            let _ = entity.update(app, |this, cx| this.set_wire(id, value, cx));
        }
    }

    /// Double-click reset for one wire parameter: back to the plug-in's
    /// default value.
    pub(crate) fn reset_cb(
        &self,
        cx: &Context<Self>,
        id: &'static str,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_window, app: &mut App| {
            let default = wire_value(&rodharerist::default_params(), id);
            let _ = entity.update(app, |this, cx| this.set_wire(id, default, cx));
        }
    }

    /// Click handler that sets one wire parameter (a model, a mic type).
    pub(crate) fn set_cb(
        &self,
        cx: &Context<Self>,
        id: &'static str,
        value: f32,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| this.set_wire(id, value, cx));
        }
    }

    /// Click handler that flips one on/off wire parameter.
    pub(crate) fn toggle_cb(
        &self,
        cx: &Context<Self>,
        id: &'static str,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                let on = wire_value(&this.params, id) >= 0.5;
                this.set_wire(id, if on { 0.0 } else { 1.0 }, cx);
            });
        }
    }

    /// Sets one wire parameter, the way the DSP takes it.
    pub(crate) fn set_wire_value(&mut self, id: &str, value: f32, cx: &mut Context<Self>) {
        self.set_wire(id, value, cx);
    }

    /// Flips one on/off wire parameter.
    pub(crate) fn toggle_wire(&mut self, id: &str, cx: &mut Context<Self>) {
        let on = wire_value(&self.params, id) >= 0.5;
        self.set_wire(id, if on { 0.0 } else { 1.0 }, cx);
    }

    /// A tap of the tempo button: two taps close enough together set Delay
    /// A's time to the interval between them, a quarter note.
    pub(crate) fn tap_tempo(&mut self, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        if let Some(interval) = tapped_interval(self.ui.last_tap, now) {
            self.set_wire("delay_time", interval.as_secs_f32() * 1_000.0, cx);
        }
        self.ui.last_tap = Some(now);
        cx.notify();
    }

    /// The newest telemetry block, if the host publishes one.
    pub(crate) fn pads(&self) -> Option<&[f32; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS]> {
        self.pads.as_ref()
    }

    /// A telemetry block for the preview tool, as if the host had sent it.
    pub fn show_pads(&mut self, block: [f32; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS]) {
        self.pads = Some(block);
    }

    /// Shows page (the preview tool's way in).
    pub fn show_page(&mut self, page: Page) {
        self.ui.page = page;
    }

    /// The host's sample rate and block size, when it reports them.
    pub(crate) fn host_status(&self) -> Option<(u32, u32)> {
        self.host_ops
            .host_status_source
            .as_ref()
            .and_then(|source| source(&self.key))
            .map(|(rate, block, _, _)| (rate, block))
            .filter(|(rate, _)| *rate > 0)
    }

    /// Whether one of the editor's text fields has the keyboard.
    pub(crate) fn typing(&self, window: &Window) -> bool {
        self.search.is_focused(window) || self.preset_name.is_focused(window)
    }

    /// Hands the keyboard back to the editor from a text field — on a page
    /// change, a preset load or Enter — so it stops taking keys it no longer
    /// shows.
    pub(crate) fn release_text_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.typing(window) {
            self.focus_handle.focus(window, cx);
        }
    }

    /// Keys for the focused text field. Printable text and IME composition
    /// arrive through the OS input handler (`EntityInputHandler` below), so
    /// this only steers: caret, selection, deletion, clipboard, Enter and
    /// Escape.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset_name.is_focused(window) {
            // Enter writes the `.pst`; Escape leaves the text as it is.
            match self.preset_name.handle_key_ime(event, Some(cx)) {
                TextInputAction::Submit => {
                    self.preset_note = self.save_user_preset().err();
                    self.release_text_focus(window, cx);
                }
                TextInputAction::Cancel => self.release_text_focus(window, cx),
                TextInputAction::Consumed | TextInputAction::Pass => {}
            }
            cx.notify();
            return;
        }
        if !self.search.is_focused(window) {
            return;
        }
        // Escape clears a search, then leaves the field.
        if event.keystroke.key == "escape" && !self.search.value.is_empty() {
            self.search.set_value("");
        } else {
            match self.search.handle_key_ime(event, Some(cx)) {
                TextInputAction::Submit | TextInputAction::Cancel => {
                    self.release_text_focus(window, cx)
                }
                TextInputAction::Consumed | TextInputAction::Pass => {}
            }
        }
        cx.notify();
    }

    /// The text field the OS input handler is talking to.
    fn ime_field(&mut self, window: &Window) -> &mut TextInputState {
        if self.preset_name.is_focused(window) {
            &mut self.preset_name
        } else {
            &mut self.search
        }
    }

    fn set_wire(&mut self, id: &str, value: f32, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        if rodharerist::apply_to_params(&mut next, id, value) {
            self.set_params(next, cx);
        }
    }

    /// Click handler that loads factory preset `index` — every parameter it
    /// changes goes out as a normal wire edit, so it saves and undoes like
    /// any other edit.
    pub(crate) fn load_preset_cb(
        &self,
        cx: &Context<Self>,
        index: usize,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                let Some(preset) = rodharerist::factory_presets().get(index) else {
                    return;
                };
                let next = preset_applied(&this.params, &preset.params);
                this.preset = Some(index);
                this.user_preset = None;
                this.preset_note = None;
                this.replace_confirm = None;
                this.preset_name.set_value(preset.name);
                this.release_text_focus(window, cx);
                this.chain_focus = None;
                this.set_params(next, cx);
            });
        }
    }

    /// Click handler for the sticky clip indicators.
    pub(crate) fn clear_clip_cb(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                if let (Some(forward), Some(index)) = (
                    this.host_ops.forward_param.clone(),
                    rodharerist::ui_param_index("clear_clip"),
                ) {
                    forward(&this.key, index, 1.0, cx);
                }
            });
        }
    }

    /// `.pst` files saved for Rodhareist, name then path.
    pub(crate) fn user_presets(&self) -> &[(String, PathBuf)] {
        &self.user_presets
    }

    pub(crate) fn preset_note(&self) -> Option<&str> {
        self.preset_note.as_deref()
    }

    /// The loaded user preset and whether the rig has moved off it.
    pub(crate) fn user_preset_status(&self) -> Option<(&str, bool)> {
        let loaded = self.user_preset.as_ref()?;
        Some((&loaded.name, loaded.params != self.params))
    }

    pub(crate) fn refresh_user_presets(&mut self) {
        self.user_presets = list_user_presets();
    }

    /// Click handler that stores the current rig as a `.pst`.
    pub(crate) fn save_user_preset_cb(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                this.release_text_focus(window, cx);
                this.preset_note = this.save_user_preset().err();
                cx.notify();
            });
        }
    }

    /// Click handler that loads user preset `index`.
    pub(crate) fn load_user_preset_cb(
        &self,
        cx: &Context<Self>,
        index: usize,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                this.release_text_focus(window, cx);
                let Some((name, path)) = this.user_presets.get(index).cloned() else {
                    return;
                };
                match this.read_user_preset(&path) {
                    Ok(saved) => {
                        let next = preset_applied(&this.params, &saved);
                        this.preset = None;
                        this.user_preset = Some(LoadedUserPreset {
                            name: name.clone(),
                            params: next.clone(),
                        });
                        this.preset_name.set_value(name);
                        this.preset_note = None;
                        this.replace_confirm = None;
                        this.chain_focus = None;
                        this.set_params(next, cx);
                    }
                    Err(error) => {
                        this.preset_note = Some(error);
                        cx.notify();
                    }
                }
            });
        }
    }

    fn save_user_preset(&mut self) -> Result<(), String> {
        let typed = self.preset_name.value.trim();
        let name = if typed.is_empty() {
            self.user_preset_save_name()
        } else {
            typed.to_string()
        };
        let stem = preset_file_stem(&name);
        let path = user_preset_dir().join(format!("{stem}.pst"));
        let loaded = self.user_preset.as_ref().map(|loaded| loaded.name.as_str());
        if save_needs_confirm(
            &stem,
            path.exists(),
            loaded,
            self.replace_confirm.as_deref(),
        ) {
            self.replace_confirm = Some(stem.clone());
            return Err(format!(
                "A preset named {stem} is already saved. Press Save again to replace it."
            ));
        }
        self.replace_confirm = None;
        let json = rodharerist::RodhareistState::new(self.params.clone())
            .to_json()
            .map_err(|e| e.to_string())?;
        SpherePluginHost::preset::write_state_preset(
            &path,
            &SpherePluginHost::preset::StatePreset {
                plugin_id: rodharerist::PLUGIN_ID.to_string(),
                plugin_name: PRESET_PLUGIN_NAME.to_string(),
                state: json.into_bytes(),
            },
        )?;
        self.preset = None;
        self.user_preset = Some(LoadedUserPreset {
            name: stem.clone(),
            params: self.params.clone(),
        });
        self.preset_name.set_value(stem);
        self.refresh_user_presets();
        Ok(())
    }

    fn read_user_preset(&self, path: &std::path::Path) -> Result<Params, String> {
        let (plugin_id, bytes) = SpherePluginHost::preset::read_state_preset(path)?;
        if plugin_id != rodharerist::PLUGIN_ID {
            return Err(format!("This preset belongs to {plugin_id}"));
        }
        let json = String::from_utf8(bytes).map_err(|_| "preset state is not text".to_string())?;
        let state = rodharerist::RodhareistState::from_json(&json).map_err(|e| e.to_string())?;
        Ok(state.params)
    }

    /// The name the field starts from: the loaded user preset, else the
    /// factory name, else "My Rig".
    fn user_preset_save_name(&self) -> String {
        if let Some(loaded) = &self.user_preset {
            return loaded.name.clone();
        }
        if let Some(index) = self.preset {
            if let Some(preset) = rodharerist::factory_presets().get(index) {
                return preset.name.to_string();
            }
        }
        "My Rig".to_string()
    }

    /// The loaded factory preset and whether the params have moved off it.
    pub(crate) fn preset_status(&self) -> Option<(usize, bool)> {
        let index = self.preset?;
        let preset = rodharerist::factory_presets().get(index)?;
        Some((
            index,
            preset_applied(&self.params, &preset.params) != self.params,
        ))
    }

    pub(crate) fn io_meter(&self) -> IoMeter {
        self.io
    }

    /// The host's tempo, read-only — the plug-in follows the transport.
    pub(crate) fn host_tempo(&self) -> Option<f64> {
        self.host_ops
            .host_status_source
            .as_ref()
            .and_then(|source| source(&self.key))
            .map(|(_rate, _block, _latency, tempo)| tempo as f64)
            .filter(|tempo| *tempo > 0.0)
    }

    /// Click handler that points the block editor at `focus`.
    pub(crate) fn focus_cb(
        &self,
        cx: &Context<Self>,
        focus: ChainFocus,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                this.chain_focus = Some(focus);
                cx.notify();
            });
        }
    }

    /// Click handler for a chain edit (add, move, remove a block): forwards
    /// the param diff like [`Self::click_cb`], then keeps the editor on the
    /// slot the edit lands on.
    pub(crate) fn chain_edit_cb(
        &self,
        cx: &Context<Self>,
        apply: impl Fn(&mut Params) + 'static,
        then: ChainFocus,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                let mut next = this.params.clone();
                apply(&mut next);
                this.chain_focus = Some(then);
                this.set_params(next, cx);
            });
        }
    }

    pub(crate) fn chain_focus(&self) -> Option<ChainFocus> {
        self.chain_focus
    }

    /// A clone of the current params, for the pure panel functions to read.
    pub(crate) fn params_snapshot(&self) -> Params {
        self.params.clone()
    }

    /// The host's last answer for `file`, if it is about the file this
    /// insert loads now.
    fn current_load(&self, file: RodhareistFile) -> Option<FileLoad> {
        let wanted = self.file_name(file)?;
        file_load(&self.key.insert_id, file).filter(|load| load.name == wanted)
    }

    /// The file `file` loads, as the project saves it.
    fn file_name(&self, file: RodhareistFile) -> Option<String> {
        match file {
            RodhareistFile::Capture => self.params.nam_file.clone(),
            RodhareistFile::Impulse => self.params.ir_file.clone(),
        }
    }

    fn status(&self, file: RodhareistFile) -> &LoadStatus {
        match file {
            RodhareistFile::Capture => &self.nam_status,
            RodhareistFile::Impulse => &self.ir_status,
        }
    }

    fn status_mut(&mut self, file: RodhareistFile) -> &mut LoadStatus {
        match file {
            RodhareistFile::Capture => &mut self.nam_status,
            RodhareistFile::Impulse => &mut self.ir_status,
        }
    }

    /// What `file` shows as loaded: its saved name, unless the host turned
    /// it down.
    fn loaded_name(&self, file: RodhareistFile) -> Option<String> {
        let name = self.file_name(file)?;
        if self
            .current_load(file)
            .is_some_and(|load| load.error.is_some())
        {
            return None;
        }
        Some(name)
    }

    /// Why `file` is not loaded: a pick or read that failed here, or the
    /// host's refusal (a missing or unreadable file after a project open).
    fn load_error(&self, file: RodhareistFile) -> Option<String> {
        self.status(file).error.clone().or_else(|| {
            self.current_load(file).and_then(|load| {
                load.error
                    .map(|error| format!("{}: {error}", display_name(&load.name)))
            })
        })
    }

    /// The IR the host accepted for the file this insert names. A filename
    /// in the project, with no host answer, is not a loaded IR.
    pub(crate) fn ir_loaded_info(&self) -> Option<(String, f32)> {
        let name = self.file_name(RodhareistFile::Impulse)?;
        let load = self.current_load(RodhareistFile::Impulse)?;
        if load.error.is_some() {
            return None;
        }
        let seconds = if load.frames == 0 {
            0.0
        } else {
            load.frames as f32 / self.sample_rate_hint()
        };
        Some((display_name(&name), seconds))
    }

    /// The impulse file named on the insert, whether or not the host loaded it.
    pub(crate) fn ir_project_name(&self) -> Option<String> {
        self.file_name(RodhareistFile::Impulse)
            .map(|name| display_name(&name))
    }
    pub(crate) fn ir_loading(&self) -> bool {
        self.ir_status.loading
    }
    pub(crate) fn ir_error(&self) -> Option<String> {
        self.load_error(RodhareistFile::Impulse)
    }
    pub(crate) fn nam_loaded_info(&self) -> Option<String> {
        self.loaded_name(RodhareistFile::Capture)
            .map(|name| display_name(&name))
    }
    pub(crate) fn nam_loading(&self) -> bool {
        self.nam_status.loading
    }
    pub(crate) fn nam_error(&self) -> Option<String> {
        self.load_error(RodhareistFile::Capture)
    }

    /// Click handler that opens the IR file picker, for the Cabinet card's
    /// "Load IR…" button.
    pub(crate) fn load_ir_cb(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                this.start_file_load(RodhareistFile::Impulse, cx)
            });
        }
    }

    /// Click handler that opens the NAM capture file picker, for the amp
    /// card's "Load capture…" button.
    pub(crate) fn load_nam_cb(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| {
                this.start_file_load(RodhareistFile::Capture, cx)
            });
        }
    }

    /// Picks a capture or an IR, keeps a copy in Rodhareist's own folder —
    /// what the project names and what Studio reloads it from — and sends
    /// it to the host. The host's answer arrives through
    /// [`Self::notify_nam_capture_result`] / [`Self::notify_ir_load_result`].
    fn start_file_load(&mut self, file: RodhareistFile, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let (title, filter, extensions): (&str, &str, &[&str]) = match file {
                RodhareistFile::Capture => ("Load NAM Capture", "Neural Amp Modeler", &["nam"]),
                RodhareistFile::Impulse => {
                    ("Load Impulse Response", "Cabinet IR", &["wav", "wave"])
                }
            };
            *self.status_mut(file) = LoadStatus {
                loading: true,
                ..LoadStatus::default()
            };
            cx.notify();
            cx.spawn(async move |this, cx| {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .add_filter(filter, extensions)
                    .pick_file()
                    .await
                else {
                    let _ = this.update(cx, |this, cx| {
                        this.status_mut(file).loading = false;
                        cx.notify();
                    });
                    return;
                };
                let path: PathBuf = handle.path().to_path_buf();
                let stored = cx
                    .background_spawn(async move { store_user_file(file, &path) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    match stored {
                        Ok((name, bytes)) => {
                            if file == RodhareistFile::Impulse {
                                this.engage_ir_cab(cx);
                            }
                            this.send_file(file, name, bytes);
                        }
                        Err(error) => {
                            let status = this.status_mut(file);
                            status.loading = false;
                            status.error = Some(error);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        #[cfg(not(feature = "native-dialogs"))]
        {
            self.status_mut(file).error =
                Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    /// The cabinet convolution is `CabModel::Ir`. Choosing a `.wav` selects
    /// that model so the file is the cabinet, not only a stored name.
    fn engage_ir_cab(&mut self, cx: &mut Context<Self>) {
        let Some(index) = rodharerist::CabModel::ALL
            .iter()
            .position(|model| *model == rodharerist::CabModel::Ir)
        else {
            return;
        };
        self.set_wire("cab_model", index as f32, cx);
    }

    /// Sends a stored file's bytes to the host under `name`.
    fn send_file(&mut self, file: RodhareistFile, name: String, bytes: Vec<u8>) {
        let key = self.key.clone();
        let sent = match file {
            RodhareistFile::Capture => match String::from_utf8(bytes) {
                Ok(json) => self.host_ops.load_nam_capture.clone().map(|load| {
                    load(
                        &key,
                        BuiltinNamLoadRequest {
                            name,
                            json,
                            // The DSP upgrades a capture that includes a cab
                            // to full rig itself.
                            stereo: false,
                            full_rig: false,
                        },
                    )
                }),
                Err(_) => {
                    let status = self.status_mut(file);
                    status.loading = false;
                    status.error = Some("This .nam file is not text.".into());
                    return;
                }
            },
            RodhareistFile::Impulse => self
                .host_ops
                .load_ir
                .clone()
                .map(|load| load(&key, BuiltinIrLoadRequest { name, bytes })),
        };
        if sent.is_none() {
            let status = self.status_mut(file);
            status.loading = false;
            status.error = Some("The plug-in host is not running yet.".into());
        }
    }

    /// Routed from `plugin_ops.rs` when the host answers a `load_ir` call,
    /// after Studio recorded the answer and the file name.
    pub fn notify_ir_load_result(
        &mut self,
        _ok: bool,
        _name: &str,
        _error: Option<&str>,
        _frames: u64,
        cx: &mut Context<Self>,
    ) {
        self.ir_status = LoadStatus::default();
        self.sync_from_mirror(cx);
    }

    /// Routed from `plugin_ops.rs` when the host answers a
    /// `load_nam_capture` call, after Studio recorded it.
    pub fn notify_nam_capture_result(
        &mut self,
        _ok: bool,
        _name: &str,
        _error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.nam_status = LoadStatus::default();
        self.sync_from_mirror(cx);
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
                    let source = this.host_ops.meter_source.as_ref();
                    let shell_moved = this.meter.poll(source, &key);
                    let io_moved = this.io.poll(source, &key);
                    let block = this
                        .host_ops
                        .pad_level_source
                        .as_ref()
                        .and_then(|source| source(&key))
                        .map(|(_, block)| block);
                    let pads_moved = block.is_some() && block != this.pads;
                    if pads_moved {
                        this.pads = block;
                    }
                    if shell_moved || io_moved || pads_moved {
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
        let on_close = self.on_close.clone();
        let alive = self.alive.clone();
        // A chain drag that ended anywhere but a drop target closes the
        // lane it opened.
        if !cx.has_active_drag() {
            self.ui.drag_block = None;
        }
        let (frame, moving) = chain_frame(&mut self.ui, &self.params, std::time::Instant::now());
        if moving {
            window.request_animation_frame();
        }
        let entity = cx.entity();
        let fields = RodFields {
            search: text_field_with_callbacks_and_ime(
                &self.search,
                self.search.is_focused(window),
                bind_mouse_selection(entity.clone(), |this| &mut this.search),
                entity.clone(),
            )
            .into_any_element(),
            preset_name: text_field_with_callbacks_and_ime(
                &self.preset_name,
                self.preset_name.is_focused(window),
                bind_mouse_selection(entity.clone(), |this| &mut this.preset_name),
                entity,
            )
            .into_any_element(),
        };
        let content = rodhareist_panel(self, &frame, fields, window, cx);

        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // A text field owns Space while its caret is up. Everywhere
                // else in this window Space is play/pause.
                if !this.typing(window) && claim_editor_transport_key(event, window, cx) {
                    return;
                }
                this.on_key_down(event, window, cx);
            }))
            .on_key_up(|_event, window, cx| swallow_editor_transport_key_up(window, cx))
            .child(native_plugin_shell(
                "rodhareist-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // Stop the meter timer, drop the window from Studio's
                    // map, then actually close it — `on_close` alone only
                    // forgets the handle and leaves the OS window open.
                    alive.set(false);
                    on_close(window, cx);
                    window.remove_window();
                },
                content,
            ))
    }
}

/// The OS input handler, for whichever of the two fields has focus: text and
/// IME composition (Thai, CJK, dead keys) reach the field here, not through
/// key-down. `impl_single_input_window_ime!` serves one field; this window has
/// two, so it routes by focus.
impl gpui::EntityInputHandler for RodhareistEditorWindow {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        actual_range: &mut Option<std::ops::Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        self.ime_field(window)
            .text_for_utf16_range(range, actual_range)
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        self.ime_field(window)
            .selected_text_range_utf16(ignore_disabled_input)
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        if self.preset_name.is_focused(window) {
            self.preset_name.marked_text_range_utf16()
        } else {
            self.search.marked_text_range_utf16()
        }
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ime_field(window).unmark_text();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_field(window)
            .replace_text_in_utf16_range(range, text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_field(window).replace_and_mark_text_in_utf16_range(
            range,
            new_text,
            new_selected_range,
        );
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.ime_field(window)
            .bounds_for_utf16_range(range_utf16, bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
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

/// One wire parameter's current value.
pub(crate) fn wire_value(params: &Params, id: &str) -> f32 {
    rodharerist::ui_values(params)
        .into_iter()
        .find(|(wire, _)| *wire == id)
        .map(|(_, value)| value)
        .unwrap_or(0.0)
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

    /// Loading a preset sends only the wire edits `wire_diff` finds; replaying
    /// them must rebuild the preset exactly, or part of it would never reach
    /// the DSP (a field missing from the wire table, say).
    #[test]
    fn every_factory_preset_arrives_whole_over_the_wire() {
        for start in [rodharerist::default_params(), {
            let mut p = rodharerist::factory_presets()[0].params.clone();
            p.input_trim_db = 3.0;
            p.power = false;
            p
        }] {
            for preset in rodharerist::factory_presets() {
                let target = preset_applied(&start, &preset.params);
                let mut replayed = start.clone();
                for (index, value) in wire_diff(&start, &target) {
                    let id = rodharerist::ui_param_id(index).expect("wire index has an id");
                    assert!(
                        rodharerist::apply_to_params(&mut replayed, id, value),
                        "{id}"
                    );
                }
                assert_eq!(replayed, target, "{} did not arrive whole", preset.name);
                assert_eq!(
                    matching_preset(&target).map(|i| rodharerist::factory_presets()[i].id),
                    Some(preset.id)
                );
            }
        }
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

    /// Issue #195: saving a loaded preset under a new name writes a new file
    /// (the old one is never touched), and Save asks first before replacing
    /// any user preset other than the loaded one.
    #[test]
    fn save_asks_before_replacing_another_user_preset() {
        // Clean loaded, saved as a new Crunch: just written.
        assert!(!save_needs_confirm("Crunch", false, Some("Clean"), None));
        // Clean loaded and saved as itself: an update, no question.
        assert!(!save_needs_confirm("Clean", true, Some("Clean"), None));
        // A factory Plexi Lead loaded over a user Plexi Lead: ask first.
        assert!(save_needs_confirm("Plexi Lead", true, None, None));
        // Another user preset's name while Clean is loaded: ask first.
        assert!(save_needs_confirm("Crunch", true, Some("Clean"), None));
        // Saving the same name again after the warning replaces it.
        assert!(!save_needs_confirm(
            "Plexi Lead",
            true,
            None,
            Some("Plexi Lead")
        ));
        // A warning about one name does not cover another.
        assert!(save_needs_confirm("Crunch", true, None, Some("Plexi Lead")));
    }
}
