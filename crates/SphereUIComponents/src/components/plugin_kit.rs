//! The frame every native built-in editor from the dynamics, band and rack
//! families shares: one window type, one editor type and the header, knob and
//! card pieces their panels are drawn from.
//!
//! A family describes itself as a [`KitModel`] — its params, their wire table,
//! presets, knobs and panel — and gets back:
//!
//! * a [`KitEditor`] that owns every edit: params, presets, A/B, the open
//!   menu and the family's own UI state. Its window renders it `.cached()`, so
//!   it rebuilds only when an edit notifies it;
//! * a [`KitWindow`], the window's root: the plug-in shell, the open menu, and
//!   an overlay that paints the [`Live`] displays — meters, needles, scopes —
//!   into the bounds the panel recorded. A telemetry timer redraws only the
//!   root and the overlay, and only when a reading moved.
//!
//! Every edit is a wire param sent with `forward_param` (folded into the
//! state mirror, saved with the project, replayed into a restarted host),
//! computed by [`KitModel::wire_values`] diffs.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    canvas, div, px, size, AnyElement, AnyView, App, AppContext, Bounds, Context, Entity,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::context_menu::{context_menu_overlay, ContextMenuEntry};
use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::fx_model::KnobSpec;
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::native_plugin_shell::{
    native_plugin_shell, NativeBuiltinEditor, ShellIdentity, ShellMeter, SHELL_METER_INTERVAL,
};
use crate::components::plugin_live::Live;
use crate::components::quick_sampler_panel::knob_cell;
use crate::theme::{radius, space, state, typography, Colors};

/// How long a passing word ("Copied A to B") stays up.
const NOTICE: Duration = Duration::from_millis(2_200);
/// A knob cell's width plus the gap after it, for sizing a card.
pub(crate) const KNOB_PITCH: f32 = 58.0 + space::TIGHT;
/// The knob size most cards use.
pub(crate) const KNOB: f32 = 34.0;
/// The size of a card's lead control.
pub(crate) const HERO_KNOB: f32 = 56.0;

/// One factory preset.
pub struct KitPreset<P> {
    pub name: &'static str,
    pub params: P,
}

/// A display the overlay paints live, by where the panel put it. The index
/// tells apart displays of one kind (a band's correlation meter, a rack
/// position's level bars).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiveSlot {
    History,
    Meters,
    Vu,
    Transfer,
    Spectrum,
    Bands,
    Scope,
    Correlation(usize),
    Reduction(usize),
    Stage(usize),
    Fader(usize),
}

/// Where each live display sat at the panel's last layout. The panel clears
/// it when it renders and its canvases fill it as they lay out, so a display
/// the panel stopped drawing stops being painted over.
#[derive(Clone, Default)]
pub struct LiveBounds(Rc<RefCell<HashMap<LiveSlot, Bounds<Pixels>>>>);

impl LiveBounds {
    pub fn get(&self, slot: LiveSlot) -> Option<Bounds<Pixels>> {
        self.0.borrow().get(&slot).copied()
    }

    fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// A canvas that records its bounds as `slot` and paints nothing: the
    /// overlay paints into it.
    pub fn slot(&self, slot: LiveSlot) -> gpui::Canvas<()> {
        let map = self.0.clone();
        canvas(
            move |bounds, _, _| {
                map.borrow_mut().insert(slot, bounds);
            },
            |_, _, _, _| {},
        )
    }
}

/// A menu the panel opened, at a window-space point: the preset bank, or one
/// of the family's own (`Own(n)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KitMenu {
    Preset(f32, f32),
    Own(u8, f32, f32),
}

/// What a family of editors is.
pub trait KitModel: Copy + PartialEq + 'static {
    type Params: Clone + 'static;
    /// The family's own editor state: a meter's mode, a selected module, a
    /// drag in progress.
    type Ui: Default + 'static;
    /// What the overlay needs to paint, cloned out of the editor each frame.
    type LiveView: 'static;

    fn title(self) -> &'static str;
    fn subtitle(self, params: &Self::Params) -> String;
    /// Element-id prefix, so two editors' controls never share ids.
    fn key(self) -> &'static str;
    fn defaults(self) -> Self::Params;
    /// The insert's params in Studio's state mirror.
    fn from_mirror(self, insert_id: &str) -> Option<Self::Params>;
    /// Every param as `(wire index, value)`, in the order an editor sends
    /// them.
    fn wire_values(self, params: &Self::Params) -> Vec<(u32, f32)>;
    /// Applies one edit by id the way the DSP will.
    fn with(self, params: &Self::Params, id: &str, value: f32) -> Self::Params;
    fn value(self, params: &Self::Params, id: &str) -> f32;
    fn presets(self) -> Arc<Vec<KitPreset<Self::Params>>>;
    /// `preset` as it would play here: listening state (power, a soloed
    /// band) stays as it is.
    fn preset_applied(self, current: &Self::Params, preset: &Self::Params) -> Self::Params;
    fn knob(self, id: &str) -> Option<KnobSpec>;
    fn panel(editor: &KitEditor<Self>, cx: &mut Context<KitEditor<Self>>) -> AnyElement;
    fn live_view(editor: &KitEditor<Self>) -> Self::LiveView;
    fn paint_live(
        view: &Self::LiveView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    );
    fn window_size(self) -> (f32, f32);
    fn min_size(self) -> (f32, f32);

    /// The edit to make as the window closes: a soloed band is let go, so a
    /// closed editor never leaves a track playing one band.
    fn on_close(self, _params: &Self::Params) -> Option<Self::Params> {
        None
    }
    /// A key the family handles; true when it did.
    fn key_down(
        _editor: &mut KitEditor<Self>,
        _key: &str,
        _cx: &mut Context<KitEditor<Self>>,
    ) -> bool {
        false
    }
    fn mouse_move(
        _editor: &mut KitEditor<Self>,
        _event: &MouseMoveEvent,
        _cx: &mut Context<KitEditor<Self>>,
    ) {
    }
    fn mouse_up(_editor: &mut KitEditor<Self>, _cx: &mut Context<KitEditor<Self>>) {}
    /// The entries of the family's menu `n`.
    fn own_menu(_editor: &KitEditor<Self>, _menu: u8) -> Vec<ContextMenuEntry> {
        Vec::new()
    }
    fn run_own_command(
        _editor: &mut KitEditor<Self>,
        _command: &str,
        _cx: &mut Context<KitEditor<Self>>,
    ) {
    }
}

/// The A/B comparison: the snapshot not playing, and which letter plays.
pub struct Compare<P> {
    pub other: P,
    pub on_b: bool,
}

pub struct KitEditor<M: KitModel> {
    pub model: M,
    key: PluginInstanceKey,
    host_ops: BuiltinEditorHostOps,
    pub params: M::Params,
    pub preset: Option<usize>,
    pub compare: Compare<M::Params>,
    notice: Option<(String, Instant)>,
    pub menu: Option<KitMenu>,
    pub ui: M::Ui,
    pub live_bounds: LiveBounds,
    /// The host's sample rate, for displays drawn from the DSP's filters.
    pub sample_rate: f32,
}

impl<M: KitModel> KitEditor<M> {
    fn new(
        model: M,
        key: PluginInstanceKey,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) -> Self {
        let params = model.defaults();
        let mut editor = Self {
            model,
            key,
            host_ops,
            compare: Compare {
                other: params.clone(),
                on_b: false,
            },
            params,
            preset: None,
            notice: None,
            menu: None,
            ui: M::Ui::default(),
            live_bounds: LiveBounds::default(),
            sample_rate: 48_000.0,
        };
        editor.sync_from_mirror(cx);
        editor.compare.other = editor.params.clone();
        editor
    }

    /// Re-reads the insert's params from Studio's state mirror — after an
    /// undo, a preset, or a project reload.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        self.params = self
            .model
            .from_mirror(&self.key.insert_id)
            .unwrap_or_else(|| self.model.defaults());
        self.preset = self.matching_preset();
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
            self.menu = None;
            self.ui = M::Ui::default();
        }
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
        self.compare = Compare {
            other: self.params.clone(),
            on_b: false,
        };
    }

    /// The wire edits that turn `self.params` into `next`.
    fn wire_diff(&self, next: &M::Params) -> Vec<(u32, f32)> {
        let before = self.model.wire_values(&self.params);
        self.model
            .wire_values(next)
            .into_iter()
            .filter(|(index, value)| {
                before
                    .iter()
                    .find(|(i, _)| i == index)
                    .is_none_or(|(_, old)| old != value)
            })
            .collect()
    }

    /// Sends what changed between the params and `next` as wire edits. Each
    /// reaches the host DSP, the state mirror, and the project's dirty flag
    /// through `forward_param`.
    pub fn set_params(&mut self, next: M::Params, cx: &mut Context<Self>) {
        let edits = self.wire_diff(&next);
        if edits.is_empty() {
            return;
        }
        self.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        self.preset = self.matching_preset();
        cx.notify();
    }

    /// Sets one param by id, the way the DSP will take it.
    pub fn set_value(&mut self, id: &str, value: f32, cx: &mut Context<Self>) {
        let next = self.model.with(&self.params, id, value);
        self.set_params(next, cx);
    }

    pub fn value(&self, id: &str) -> f32 {
        self.model.value(&self.params, id)
    }

    pub fn flag(&self, id: &str) -> bool {
        self.value(id) >= 0.5
    }

    pub fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        let on = self.flag(id);
        self.set_value(id, if on { 0.0 } else { 1.0 }, cx);
    }

    pub fn power(&self) -> bool {
        self.flag("power")
    }

    pub fn show_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), Instant::now()));
        cx.notify();
        // Take it down when it runs out, without waiting for the next edit.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// The passing word to show, if one is still up.
    pub fn notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE)
            .map(|(text, _)| text.as_str())
    }

    // ── Presets and A/B ─────────────────────────────────────────────────

    /// Whether two param sets sound the same: equal within the rounding a
    /// knob and the wire leave behind.
    fn same_sound(&self, a: &M::Params, b: &M::Params) -> bool {
        let theirs = self.model.wire_values(b);
        self.model.wire_values(a).iter().all(|(index, value)| {
            theirs
                .iter()
                .find(|(i, _)| i == index)
                .is_some_and(|(_, other)| (value - other).abs() <= 0.002 * value.abs().max(1.0))
        })
    }

    /// The preset the params are, if any, whatever the listening state.
    pub fn matching_preset(&self) -> Option<usize> {
        self.model.presets().iter().position(|preset| {
            let applied = self.model.preset_applied(&self.params, &preset.params);
            self.same_sound(&applied, &self.params)
        })
    }

    /// Loads preset `index` — wrapping, so the arrows cycle — as ordinary
    /// wire edits, so it saves and undoes like any other edit.
    pub fn load_preset(&mut self, index: isize, cx: &mut Context<Self>) {
        let bank = self.model.presets();
        let wrapped = index.rem_euclid(bank.len() as isize) as usize;
        let next = self
            .model
            .preset_applied(&self.params, &bank[wrapped].params);
        self.menu = None;
        self.set_params(next, cx);
        self.preset = Some(wrapped);
        cx.notify();
    }

    pub fn step_preset(&mut self, delta: isize, cx: &mut Context<Self>) {
        let from = match self.preset {
            Some(index) => index as isize,
            // Edited: the arrows start from either end of the bank.
            None if delta > 0 => -1,
            None => 0,
        };
        self.load_preset(from + delta, cx);
    }

    pub fn preset_label(&self) -> String {
        self.preset
            .and_then(|index| self.model.presets().get(index).map(|preset| preset.name))
            .unwrap_or("Edited")
            .to_string()
    }

    /// Switches between A and B: what plays parks, the other plays, sent as
    /// wire edits. The listening state stays as it is.
    pub fn swap_compare(&mut self, cx: &mut Context<Self>) {
        let next = self.model.preset_applied(&self.params, &self.compare.other);
        self.compare.other = self.params.clone();
        self.compare.on_b = !self.compare.on_b;
        self.set_params(next, cx);
        cx.notify();
    }

    /// Copies what plays into the other letter.
    pub fn copy_compare(&mut self, cx: &mut Context<Self>) {
        self.compare.other = self.params.clone();
        let text = if self.compare.on_b {
            "Copied B to A"
        } else {
            "Copied A to B"
        };
        self.show_notice(text, cx);
    }

    // ── Menus ───────────────────────────────────────────────────────────

    pub fn open_menu(&mut self, menu: KitMenu, cx: &mut Context<Self>) {
        self.menu = Some(menu);
        cx.notify();
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn menu_entries(&self) -> Vec<ContextMenuEntry> {
        match self.menu {
            Some(KitMenu::Preset(..)) => {
                let mut entries = vec![ContextMenuEntry::Header(format!(
                    "{} presets",
                    self.model.title()
                ))];
                entries.extend(
                    self.model
                        .presets()
                        .iter()
                        .enumerate()
                        .map(|(index, preset)| {
                            ContextMenuEntry::checked_item(
                                preset.name,
                                format!("preset:{index}"),
                                self.preset == Some(index),
                            )
                        }),
                );
                entries
            }
            Some(KitMenu::Own(menu, ..)) => M::own_menu(self, menu),
            None => Vec::new(),
        }
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(index) = command
            .strip_prefix("preset:")
            .and_then(|index| index.parse::<isize>().ok())
        {
            self.load_preset(index, cx);
        } else {
            M::run_own_command(self, command, cx);
        }
        cx.notify();
    }

    // ── Handlers for the panel ──────────────────────────────────────────

    /// A click handler that runs `action` on this editor.
    pub fn click_cb(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |_event, _window, app: &mut App| {
            let _ = entity.update(app, |this, cx| action(this, cx));
        }
    }

    /// A mouse-down handler that runs `action` with the window-space point.
    pub fn press_cb(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, (f32, f32), &MouseDownEvent, &mut Context<Self>) + 'static,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        let entity = cx.entity().clone();
        move |event: &MouseDownEvent, _window, app: &mut App| {
            let at = (f32::from(event.position.x), f32::from(event.position.y));
            let _ = entity.update(app, |this, cx| action(this, at, event, cx));
        }
    }

    /// A knob handler: the widget's value, through `knob`'s taper, into its
    /// param.
    pub fn knob_cb(
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

    fn closing(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.model.on_close(&self.params) {
            self.set_params(next, cx);
        }
    }
}

impl<M: KitModel> Render for KitEditor<M> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The canvases below record where the live displays sit now.
        self.live_bounds.clear();
        M::panel(self, cx)
    }
}

pub struct KitWindow<M: KitModel> {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    editor: Entity<KitEditor<M>>,
    live: Rc<RefCell<Live>>,
    meter: ShellMeter,
    /// Cleared when the window closes, ending the telemetry timer.
    alive: Rc<Cell<bool>>,
}

impl<M: KitModel> KitWindow<M> {
    pub fn new(
        model: M,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| KitEditor::new(model, key.clone(), host_ops.clone(), cx));
        let window = Self {
            live: Rc::new(RefCell::new(Live::new(key.clone(), &host_ops))),
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            editor,
            meter: ShellMeter::default(),
            alive: Rc::new(Cell::new(true)),
        };
        window.start_telemetry(cx);
        window
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    pub fn editor(&self) -> &Entity<KitEditor<M>> {
        &self.editor
    }

    pub fn live(&self) -> &Rc<RefCell<Live>> {
        &self.live
    }

    /// Re-reads the params from Studio's state mirror.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.sync_from_mirror(cx));
    }

    /// Polls the insert's telemetry at the shell's rate, redrawing the root
    /// — and so the overlay — only when a reading moved.
    fn start_telemetry(&self, cx: &mut Context<Self>) {
        let alive = self.alive.clone();
        cx.spawn(async move |this, cx| {
            while alive.get() {
                cx.background_executor().timer(SHELL_METER_INTERVAL).await;
                let keep = this.update(cx, |this, cx| this.poll_telemetry(cx));
                if keep.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll_telemetry(&mut self, cx: &mut Context<Self>) {
        let mut moved = self
            .meter
            .poll(self.host_ops.meter_source.as_ref(), &self.key);
        moved |= self.live.borrow_mut().poll();
        // A filter curve follows the host's rate; a change re-renders the
        // panel, which is rare.
        if let Some(rate) = self.live.borrow().sample_rate() {
            let editor = self.editor.read(cx);
            if (editor.sample_rate - rate).abs() > 0.5 {
                self.editor.update(cx, |editor, cx| {
                    editor.sample_rate = rate;
                    cx.notify();
                });
            }
        }
        if moved {
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.clone();
        self.editor.update(cx, |editor, cx| {
            if key == "escape" && editor.menu.is_some() {
                editor.close_menu(cx);
            } else {
                M::key_down(editor, &key, cx);
            }
        });
    }
}

impl<M: KitModel> NativeBuiltinEditor for KitWindow<M> {
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
        self.editor.update(cx, |editor, cx| {
            editor.rebind(key.clone(), host_ops.clone(), cx)
        });
        *self.live.borrow_mut() = Live::new(key, &host_ops);
        cx.notify();
    }
}

impl<M: KitModel> Render for KitWindow<M> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let (view, bounds, menu, entries) = {
            let editor = self.editor.read(cx);
            (
                M::live_view(editor),
                editor.live_bounds.clone(),
                editor.menu,
                editor.menu_entries(),
            )
        };
        let live = self.live.clone();
        let overlay = canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                M::paint_live(&view, &mut live.borrow_mut(), &bounds, window, cx);
            },
        )
        .absolute()
        .size_full();

        let menu = menu.map(|menu| {
            let (x, y) = match menu {
                KitMenu::Preset(x, y) | KitMenu::Own(_, x, y) => (x, y),
            };
            let viewport = window.viewport_size();
            let command_target = self.editor.clone();
            let close_target = self.editor.clone();
            context_menu_overlay(
                entries,
                x,
                y,
                viewport.width.into(),
                viewport.height.into(),
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ =
                        command_target.update(cx, |this, cx| this.run_menu_command(&command, cx));
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| this.close_menu(cx));
                }),
            )
        });

        let on_close = self.on_close.clone();
        let editor = self.editor.clone();
        let alive = self.alive.clone();
        let content = AnyView::from(self.editor.clone())
            .cached(gpui::StyleRefinement::default().size_full())
            .into_any_element();
        let move_target = self.editor.clone();
        let up_target = self.editor.clone();
        let up_out_target = self.editor.clone();
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _window, cx| this.on_key_down(event, cx)),
            )
            // Gestures that start on a display follow the pointer anywhere
            // over the window.
            .on_mouse_move(move |event: &MouseMoveEvent, _window, cx| {
                let _ = move_target.update(cx, |editor, cx| M::mouse_move(editor, event, cx));
            })
            .on_mouse_up(MouseButton::Left, move |_: &MouseUpEvent, _window, cx| {
                let _ = up_target.update(cx, |editor, cx| M::mouse_up(editor, cx));
            })
            .on_mouse_up_out(MouseButton::Left, move |_: &MouseUpEvent, _window, cx| {
                let _ = up_out_target.update(cx, |editor, cx| M::mouse_up(editor, cx));
            })
            .child(native_plugin_shell(
                "kit-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    alive.set(false);
                    let _ = editor.update(cx, |editor, cx| editor.closing(cx));
                    on_close(window, cx);
                    window.remove_window();
                },
                content,
            ))
            .child(overlay)
            .children(menu)
    }
}

/// Opens a family's editor on a track's insert.
pub fn open_kit_editor<M: KitModel>(
    model: M,
    owner_bounds: Option<Bounds<Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<KitWindow<M>>, String> {
    let (width, height) = model.window_size();
    let (min_width, min_height) = model.min_size();
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
    options.window_min_size = Some(size(px(min_width), px(min_height)));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| KitWindow::new(model, key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

// ── Pieces the panels share ─────────────────────────────────────────────────

pub(crate) type Cx<'a, M> = Context<'a, KitEditor<M>>;

pub(crate) fn caption(text: impl Into<String>) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text.into())
        .into_any_element()
}

/// Muted explanatory text under a control.
pub(crate) fn hint(text: impl Into<String>) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_muted())
        .child(text.into())
        .into_any_element()
}

/// A row of related controls.
pub(crate) fn cluster() -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(space::TIGHT))
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match index {
        0 => FbSegment::First,
        i if i + 1 == count => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

/// The width a segmented track needs for labels: segments split a track
/// equally, so each is as wide as the widest label, and never narrower than
/// `fb_segment`'s own 44 px minimum.
pub(crate) fn track_width<S: AsRef<str>>(labels: &[S]) -> f32 {
    let widest = labels
        .iter()
        .map(|label| (label.as_ref().chars().count() as f32 * 8.0 + 2.0 * space::BASE).max(44.0))
        .fold(0.0f32, f32::max);
    widest * labels.len() as f32 + 2.0 * space::TIGHT + 2.0
}

/// A segmented choice over `labels`, `selected` lit, `pick(i)` on a click.
pub(crate) fn choice<M: KitModel>(
    e: &KitEditor<M>,
    cx: &mut Cx<M>,
    id: &'static str,
    labels: &[&str],
    selected: Option<usize>,
    pick: impl Fn(&mut KitEditor<M>, usize, &mut Cx<M>) + Copy + 'static,
) -> AnyElement {
    let mut track = fb_segmented_track();
    for (index, label) in labels.iter().enumerate() {
        track = track.child(fb_segment(
            (id, index),
            label.to_string(),
            selected == Some(index),
            segment_position(index, labels.len()),
            e.click_cb(cx, move |this, cx| pick(this, index, cx)),
        ));
    }
    track.w(px(track_width(labels))).into_any_element()
}

/// A checkbox for a boolean param.
pub(crate) fn flag<M: KitModel>(
    e: &KitEditor<M>,
    cx: &mut Cx<M>,
    id: &'static str,
    label: &'static str,
    enabled: bool,
) -> AnyElement {
    let element_id = format!("{}-{id}", e.model.key());
    let id_owned = id;
    fb_checkbox(
        gpui::ElementId::Name(element_id.into()),
        label,
        e.flag(id),
        enabled,
        e.click_cb(cx, move |this, cx| this.toggle(id_owned, cx)),
    )
    .into_any_element()
}

/// The header: the plug-in's name, the preset bar, A/B and power.
pub(crate) fn header<M: KitModel>(e: &KitEditor<M>, cx: &mut Cx<M>) -> AnyElement {
    let key = e.model.key();
    let preset_picker = div()
        .id((key, 0usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .w(px(180.0))
        .h(px(26.0))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::button_border())
        .bg(Colors::surface_canvas())
        .hover(|style| style.border_color(Colors::border_normal()))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::UI_SM))
        .text_color(Colors::text_primary())
        .child(div().flex_1().truncate().child(e.preset_label()))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child("▾"),
        )
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, |this, (x, y), _, cx| {
                this.open_menu(KitMenu::Preset(x, y), cx)
            }),
        );

    let presets = cluster()
        .child(fb_button(
            (key, 1usize),
            "‹",
            FbButtonKind::Ghost,
            true,
            e.click_cb(cx, |this, cx| this.step_preset(-1, cx)),
        ))
        .child(preset_picker)
        .child(fb_button(
            (key, 2usize),
            "›",
            FbButtonKind::Ghost,
            true,
            e.click_cb(cx, |this, cx| this.step_preset(1, cx)),
        ))
        .child(fb_button(
            (key, 3usize),
            "Reset",
            FbButtonKind::Ghost,
            true,
            e.click_cb(cx, |this, cx| this.load_preset(0, cx)),
        ));

    let on_b = e.compare.on_b;
    let compare = cluster()
        .child(choice(
            e,
            cx,
            "kit-compare",
            &["A", "B"],
            Some(usize::from(on_b)),
            |this, index, cx| {
                if (index == 1) != this.compare.on_b {
                    this.swap_compare(cx);
                }
            },
        ))
        .child(fb_button(
            (key, 4usize),
            if on_b { "Copy B → A" } else { "Copy A → B" },
            FbButtonKind::Ghost,
            true,
            e.click_cb(cx, |this, cx| this.copy_compare(cx)),
        ));

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::LOOSE))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .min_w(px(120.0))
                .child(
                    div()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_primary())
                        .child(e.model.title()),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(e.model.subtitle(&e.params)),
                ),
        )
        .child(presets)
        .child(compare)
        .child(div().flex_1())
        .child(fb_checkbox(
            (key, 5usize),
            "Power",
            e.power(),
            true,
            e.click_cb(cx, |this, cx| this.toggle("power", cx)),
        ))
        .into_any_element()
}

/// A card: a caption over a row of controls. It takes a share of the row by
/// `grow` and never gets narrower than `min_w`.
pub(crate) fn card(
    title: impl Into<String>,
    grow: f32,
    min_w: f32,
    body: impl IntoElement,
) -> AnyElement {
    let mut card = div()
        .flex()
        .flex_col()
        .flex_basis(px(0.0))
        .min_w(px(min_w))
        .gap(px(space::SNUG));
    card.style().flex_grow = Some(grow);
    card.p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(caption(title))
        .child(body)
        .into_any_element()
}

/// A card of knobs, as wide as they need.
pub(crate) fn knob_card(title: impl Into<String>, knobs: Vec<AnyElement>) -> AnyElement {
    let count = knobs.len().max(1) as f32;
    card(
        title,
        count,
        count * KNOB_PITCH + 2.0 * space::BASE + 2.0,
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap(px(space::TIGHT))
            .children(knobs),
    )
}

/// The knob for param `id` at `size`, live or — when the setting leaves it
/// nothing to do — greyed with `why` in place of its value.
pub(crate) fn knob_for<M: KitModel>(
    e: &KitEditor<M>,
    cx: &mut Cx<M>,
    id: &'static str,
    size: f32,
    why_not: Option<&str>,
) -> AnyElement {
    let Some(spec) = e.model.knob(id) else {
        return div().into_any_element();
    };
    let element_id = format!("{}-{id}", e.model.key());
    if let Some(why) = why_not {
        return div()
            .opacity(state::DISABLED_CONTENT)
            .child(knob_cell(
                spec.label,
                why.to_string(),
                knob_with_default(
                    element_id,
                    0.0,
                    0.0,
                    1.0,
                    size,
                    Colors::text_disabled(),
                    0.0,
                    |_, _, _| {},
                ),
            ))
            .into_any_element();
    }
    let value = e.value(id);
    let default = e.model.value(&e.model.defaults(), id);
    let (min, max) = spec.knob_range();
    let on_change = e.knob_cb(cx, spec);
    let accent = Colors::accent_primary();
    let control = if spec.bipolar {
        knob_bipolar(
            element_id,
            spec.to_knob(value),
            min,
            max,
            size,
            accent,
            None,
            spec.to_knob(default),
            on_change,
        )
        .into_any_element()
    } else {
        knob_with_default(
            element_id,
            spec.to_knob(value),
            min,
            max,
            size,
            accent,
            spec.to_knob(default),
            on_change,
        )
        .into_any_element()
    };
    knob_cell(spec.label, spec.readout(value), control)
}

/// The framed well a display sits in: a caption tag and an optional legend
/// over its top edge, a passing notice along its bottom.
pub(crate) fn display_frame(
    title: impl Into<String>,
    legend: Option<String>,
    notice: Option<String>,
    body: impl IntoElement,
) -> gpui::Div {
    let tag = |text: String| {
        div()
            .px(px(space::SNUG))
            .py(px(space::HAIR))
            .rounded(px(radius::CONTROL_SM))
            .bg(Colors::with_alpha(Colors::surface_base(), 0.88))
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(Colors::text_secondary())
            .child(text)
    };
    let top = div()
        .absolute()
        .top(px(space::SNUG))
        .left(px(space::SNUG))
        .right(px(space::SNUG))
        .flex()
        .flex_row()
        .justify_between()
        .child(tag(title.into()))
        .children(legend.map(tag));
    let notice = notice.map(|text| {
        div()
            .absolute()
            .bottom(px(space::LOOSE))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .px(px(space::BASE))
                    .py(px(space::TIGHT))
                    .rounded(px(radius::CONTROL))
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .bg(Colors::with_alpha(Colors::surface_base(), 0.92))
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_secondary())
                    .child(text),
            )
    });
    div()
        .relative()
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_canvas())
        .child(div().absolute().inset_0().child(body))
        .child(top)
        .children(notice)
}

/// The notice a bypassed editor shows over its main display.
pub(crate) fn bypass_notice<M: KitModel>(e: &KitEditor<M>) -> Option<String> {
    e.notice().map(str::to_string).or_else(|| {
        (!e.power()).then(|| {
            format!(
                "Bypassed — {} passes audio through unchanged",
                e.model.title()
            )
        })
    })
}

/// A preset bank from a DSP crate's factory list.
pub(crate) fn bank<P, Q>(
    presets: Vec<Q>,
    split: impl Fn(Q) -> (&'static str, P),
) -> Arc<Vec<KitPreset<P>>> {
    Arc::new(
        presets
            .into_iter()
            .map(|preset| {
                let (name, params) = split(preset);
                KitPreset { name, params }
            })
            .collect(),
    )
}
