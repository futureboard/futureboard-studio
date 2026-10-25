//! The native editor window of a WhiteSharp insert.
//!
//! Three layers, so the live displays can run every frame without rebuilding
//! the controls:
//!
//! * the **stage** — the plate, the pitch band and the dial — drawn by wgpu
//!   ([`LiveDisplay::paint_stage`]) under everything;
//! * [`WhiteSharpEditor`], which owns every edit — params, presets, A/B, the
//!   dock and the preset menu — and renders the faceplate's controls over
//!   the stage. The window renders it `.cached()`, so it rebuilds only when
//!   it is notified: on an edit;
//! * an overlay over both: the note in the ring, the band's note names, the
//!   meters and the keyboard's lit note ([`LiveDisplay::paint_overlay`]).
//!
//! [`WhiteSharpWindow`] is the window's root: it asks for a frame every
//! frame while open, and that redraw reaches only it and the live layers,
//! which paint into the bounds the panel recorded.
//!
//! Every edit is a wire param sent with `forward_param` (folded into the
//! state mirror, saved with the project, replayed into a restarted host),
//! computed by [`wire_diff`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    canvas, div, px, size, AnyView, App, AppContext, Bounds, Context, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};
use whitesharp::Params;

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::{
    native_plugin_shell, NativeBuiltinEditor, ShellIdentity, ShellMeter,
};
use crate::components::white_sharp_meter::{LiveBounds, LiveDisplay, LiveInputs};
use crate::components::white_sharp_model::{
    matching_preset, preset_applied, presets, wire_diff, with,
};
use crate::components::white_sharp_panel::{white_sharp_panel, DOCK_H, SHELL_CHROME_H};
use crate::components::white_sharp_style::{ensure_fonts, PLATE_H, PLATE_W};

/// How long a passing word ("Copied A to B") stays up.
const NOTICE: Duration = Duration::from_millis(2_200);
/// A press on the preset chip this soon after the menu closed is the one
/// that closed it.
const MENU_REOPEN_GUARD: Duration = Duration::from_millis(250);

/// The A/B comparison: the snapshot not playing, and which letter plays.
#[derive(Clone, Debug)]
pub(crate) struct Compare {
    pub other: Params,
    pub on_b: bool,
}

/// Which list a pitch class is put on from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteList {
    Removed,
    Bypassed,
}

/// The panel under the plate: one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dock {
    Keys,
    Voice,
    Vibrato,
}

/// Where the panel laid out what the live layers paint into.
#[derive(Clone, Default)]
pub(crate) struct LiveCells {
    pub stage: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub orb: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub meter_in: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub meter_out: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub keyboard: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl LiveCells {
    fn bounds(&self, keys_open: bool) -> LiveBounds {
        LiveBounds {
            stage: self.stage.get(),
            orb: self.orb.get(),
            meter_in: self.meter_in.get(),
            meter_out: self.meter_out.get(),
            keyboard: self.keyboard.get().filter(|_| keys_open),
        }
    }
}

pub struct WhiteSharpEditor {
    key: PluginInstanceKey,
    host_ops: BuiltinEditorHostOps,
    pub(crate) params: Params,
    pub(crate) preset: Option<usize>,
    pub(crate) compare: Compare,
    notice: Option<(String, Instant)>,
    pub(crate) preset_open: bool,
    preset_closed_at: Option<Instant>,
    pub(crate) dock: Option<Dock>,
    /// Which list a click on the keyboard puts a note on.
    pub(crate) key_edit: NoteList,
    pub(crate) cells: LiveCells,
}

impl WhiteSharpEditor {
    fn new(key: PluginInstanceKey, host_ops: BuiltinEditorHostOps, cx: &mut Context<Self>) -> Self {
        let params = whitesharp::default_params();
        let mut editor = Self {
            key,
            host_ops,
            compare: Compare {
                other: params.clone(),
                on_b: false,
            },
            params,
            preset: None,
            notice: None,
            preset_open: false,
            preset_closed_at: None,
            dock: None,
            key_edit: NoteList::Removed,
            cells: LiveCells::default(),
        };
        editor.sync_from_mirror(cx);
        editor.compare.other = editor.params.clone();
        editor
    }

    /// Re-reads the insert's params from Studio's state mirror — after an
    /// undo, a preset, or a project reload.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        self.params = crate::components::builtin_plugin_editor::builtin_whitesharp_params(
            &self.key.insert_id,
        )
        .unwrap_or_else(whitesharp::default_params);
        self.preset = matching_preset(&self.params);
        cx.notify();
    }

    fn rebind(
        &mut self,
        key: PluginInstanceKey,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        if key != self.key {
            self.key = key;
            self.preset_open = false;
        }
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
        self.compare = Compare {
            other: self.params.clone(),
            on_b: false,
        };
    }

    /// Sends what changed between the params and `next` as wire edits.
    pub(crate) fn set_params(&mut self, next: Params, cx: &mut Context<Self>) {
        let edits = wire_diff(&self.params, &next);
        if edits.is_empty() {
            return;
        }
        self.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        self.preset = matching_preset(&self.params);
        cx.notify();
    }

    pub(crate) fn set_value(&mut self, id: &str, value: f32, cx: &mut Context<Self>) {
        let next = with(&self.params, id, value);
        self.set_params(next, cx);
    }

    pub(crate) fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        let on = crate::components::white_sharp_model::value(&self.params, id) >= 0.5;
        self.set_value(id, if on { 0.0 } else { 1.0 }, cx);
    }

    /// Puts pitch class `class` on or off `list`. A note is on one list at a
    /// time: removing a bypassed note un-bypasses it, and so on.
    pub(crate) fn toggle_note(&mut self, list: NoteList, class: u8, cx: &mut Context<Self>) {
        let bit = 1u16 << class;
        let mut next = self.params.clone();
        match list {
            NoteList::Removed => {
                next.remove_mask ^= bit;
                next.bypass_mask &= !(next.remove_mask & bit);
            }
            NoteList::Bypassed => {
                next.bypass_mask ^= bit;
                next.remove_mask &= !(next.bypass_mask & bit);
            }
        }
        self.set_params(next, cx);
    }

    pub(crate) fn set_key_edit(&mut self, list: NoteList, cx: &mut Context<Self>) {
        self.key_edit = list;
        cx.notify();
    }

    /// Takes every note off both lists.
    pub(crate) fn clear_notes(&mut self, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.remove_mask = 0;
        next.bypass_mask = 0;
        self.set_params(next, cx);
    }

    /// A new key or scale starts from its own notes: lists made for the old
    /// one would mark the wrong ones.
    pub(crate) fn set_key(&mut self, key: u8, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.key = key.min(11);
        next.remove_mask = 0;
        next.bypass_mask = 0;
        self.set_params(next, cx);
    }

    pub(crate) fn set_scale(&mut self, scale: whitesharp::Scale, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.scale = scale;
        next.remove_mask = 0;
        next.bypass_mask = 0;
        self.set_params(next, cx);
    }

    fn show_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), Instant::now()));
        cx.notify();
        // Take it down when it runs out, without waiting for the next edit.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// The rate the insert's host runs at, for the latency readout and the
    /// trace's time scale; a nominal 48 kHz where no host reports one
    /// (LiveStage, previews).
    pub(crate) fn sample_rate(&self) -> f32 {
        self.host_ops
            .host_status_source
            .as_ref()
            .and_then(|source| source(&self.key))
            .map(|(rate, ..)| rate as f32)
            .filter(|rate| *rate > 0.0)
            .unwrap_or(48_000.0)
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE)
            .map(|(text, _)| text.as_str())
    }

    // ── Presets and A/B ─────────────────────────────────────────────────

    pub(crate) fn load_preset(&mut self, index: usize, cx: &mut Context<Self>) {
        let bank = presets();
        let Some(preset) = bank.get(index) else {
            return;
        };
        let next = preset_applied(&self.params, &preset.params);
        self.close_menu(cx);
        self.set_params(next, cx);
        self.preset = Some(index);
        cx.notify();
    }

    pub(crate) fn preset_label(&self) -> String {
        self.preset
            .and_then(|index| presets().get(index).map(|preset| preset.name))
            .unwrap_or("Edited")
            .to_string()
    }

    pub(crate) fn toggle_menu(&mut self, cx: &mut Context<Self>) {
        let just_closed = self
            .preset_closed_at
            .is_some_and(|at| at.elapsed() < MENU_REOPEN_GUARD);
        if self.preset_open {
            self.close_menu(cx);
        } else if !just_closed {
            self.preset_open = true;
            cx.notify();
        }
    }

    pub(crate) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.preset_open {
            self.preset_open = false;
            self.preset_closed_at = Some(Instant::now());
            cx.notify();
        }
    }

    /// Opens `dock` under the plate, or closes it when it is the open one.
    /// The window grows by the dock's height rather than squeezing the
    /// plate.
    pub(crate) fn toggle_dock(&mut self, dock: Dock, window: &mut Window, cx: &mut Context<Self>) {
        let was_open = self.dock.is_some();
        self.dock = if self.dock == Some(dock) {
            None
        } else {
            Some(dock)
        };
        let delta = match (was_open, self.dock.is_some()) {
            (false, true) => DOCK_H,
            (true, false) => -DOCK_H,
            _ => 0.0,
        };
        if delta != 0.0 && !window.is_maximized() && !window.is_fullscreen() {
            let viewport = window.viewport_size();
            window.resize(size(viewport.width, viewport.height + px(delta)));
        }
        cx.notify();
    }

    /// Switches between A and B; power stays as it is.
    pub(crate) fn swap_compare(&mut self, cx: &mut Context<Self>) {
        let mut next = self.compare.other.clone();
        next.power = self.params.power;
        self.compare.other = self.params.clone();
        self.compare.on_b = !self.compare.on_b;
        self.set_params(next, cx);
        cx.notify();
    }

    pub(crate) fn copy_compare(&mut self, cx: &mut Context<Self>) {
        self.compare.other = self.params.clone();
        let text = if self.compare.on_b {
            "Copied B to A"
        } else {
            "Copied A to B"
        };
        self.show_notice(text, cx);
    }

    // ── Handlers for the panel ──────────────────────────────────────────

    pub(crate) fn click_cb(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| action(this, cx));
        }
    }

    pub(crate) fn click_window_cb(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, window, app: &mut App| {
            let _ = entity.update(app, |this, cx| action(this, window, cx));
        }
    }

    pub(crate) fn knob_cb(
        &self,
        cx: &Context<Self>,
        knob: KnobSpec,
    ) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |units: &f32, _window, app: &mut App| {
            let value = knob.from_knob(*units);
            let _ = entity.update(app, |this, cx| this.set_value(knob.id, value, cx));
        }
    }

    fn live_inputs(&self) -> LiveInputs {
        LiveInputs {
            key: self.params.key,
            scale: self.params.scale,
            allowed: self.params.allowed_notes(),
            power: self.params.power,
            sample_rate: self.sample_rate(),
        }
    }
}

impl Render for WhiteSharpEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        white_sharp_panel(self, window, cx)
    }
}

pub struct WhiteSharpWindow {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    editor: Entity<WhiteSharpEditor>,
    live: Rc<RefCell<LiveDisplay>>,
    meter: ShellMeter,
}

impl WhiteSharpWindow {
    pub fn new(
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        ensure_fonts(cx);
        let editor = cx.new(|cx| WhiteSharpEditor::new(key.clone(), host_ops.clone(), cx));
        let live = Rc::new(RefCell::new(LiveDisplay::default()));
        let window = Self {
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            editor,
            live,
            meter: ShellMeter::default(),
        };
        window.bind_live();
        window
    }

    fn bind_live(&self) {
        self.live.borrow_mut().bind(
            self.key.clone(),
            self.host_ops.pad_level_source.clone(),
            self.host_ops.meter_source.clone(),
        );
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    pub fn editor(&self) -> &Entity<WhiteSharpEditor> {
        &self.editor
    }

    pub fn live(&self) -> &Rc<RefCell<LiveDisplay>> {
        &self.live
    }

    /// Re-reads the params from Studio's state mirror.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.sync_from_mirror(cx));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let plain = !keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.platform;
        match keystroke.key.as_str() {
            "escape" => self.editor.update(cx, |editor, cx| editor.close_menu(cx)),
            "b" if plain && !event.is_held => self
                .editor
                .update(cx, |editor, cx| editor.toggle("power", cx)),
            _ => {}
        }
    }
}

impl NativeBuiltinEditor for WhiteSharpWindow {
    fn rebind_insert(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        self.key = key.clone();
        self.identity = identity;
        self.host_ops = host_ops.clone();
        self.editor
            .update(cx, |editor, cx| editor.rebind(key, host_ops, cx));
        self.bind_live();
        cx.notify();
    }
}

impl Render for WhiteSharpWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        // The stage and the meters follow the voice: a frame every frame,
        // which reaches this view and the live layers, not the cached panel.
        window.request_animation_frame();
        self.meter
            .poll(self.host_ops.meter_source.as_ref(), &self.key);

        let (inputs, cells, keys_open) = {
            let editor = self.editor.read(cx);
            (
                editor.live_inputs(),
                editor.cells.clone(),
                editor.dock == Some(Dock::Keys),
            )
        };
        let stage_live = self.live.clone();
        let stage_cells = cells.clone();
        let stage = canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                let bounds = stage_cells.bounds(keys_open);
                stage_live
                    .borrow_mut()
                    .paint_stage(&inputs, &bounds, window, cx);
            },
        )
        .absolute()
        .size_full();
        let overlay_live = self.live.clone();
        let overlay = canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                let bounds = cells.bounds(keys_open);
                overlay_live
                    .borrow()
                    .paint_overlay(&inputs, &bounds, window, cx);
            },
        )
        .absolute()
        .size_full();

        let panel = AnyView::from(self.editor.clone())
            .cached(gpui::StyleRefinement::default().size_full())
            .into_any_element();
        let content = div()
            .relative()
            .size_full()
            .child(stage)
            .child(panel)
            .child(overlay)
            .into_any_element();

        let on_close = self.on_close.clone();
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _window, cx| this.on_key_down(event, cx)),
            )
            .child(native_plugin_shell(
                "whitesharp-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
                content,
            ))
    }
}

/// The plate at its design size, under the shell's title bar and strip.
pub const WHITESHARP_WINDOW_SIZE: (f32, f32) = (PLATE_W, PLATE_H + SHELL_CHROME_H);
const MIN_WIDTH: f32 = 720.0;
const MIN_HEIGHT: f32 = 450.0 + SHELL_CHROME_H;

/// Opens with a track's WhiteSharp insert.
pub fn open_whitesharp_editor(
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<WhiteSharpWindow>, String> {
    let (width, height) = WHITESHARP_WINDOW_SIZE;
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(width), px(height)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(MIN_WIDTH), px(MIN_HEIGHT)));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| WhiteSharpWindow::new(key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}
