//! The native editor window of a WhiteSharp insert.
//!
//! Two views, so the live displays can run every frame without rebuilding
//! the controls:
//!
//! * [`WhiteSharpEditor`] owns every edit — params, presets, A/B, the menus'
//!   state — and renders the panel. Studio's window renders it `.cached()`,
//!   so it rebuilds only when it is notified: on an edit.
//! * [`WhiteSharpWindow`] is the window's root: the plug-in shell, the open
//!   menu, and an overlay that paints the [`LiveDisplay`] — the correction
//!   meter and the keyboard's sung and target notes — into the bounds the
//!   panel recorded. It asks for a frame every frame while open, and that
//!   redraw reaches only it and the overlay.
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
use crate::components::context_menu::{context_menu_overlay, ContextMenuEntry};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::{
    native_plugin_shell, NativeBuiltinEditor, ShellIdentity, ShellMeter,
};
use crate::components::white_sharp_meter::{paint_text, LiveDisplay};
use crate::components::white_sharp_model::{
    matching_preset, preset_applied, presets, wire_diff, with,
};
use crate::components::white_sharp_panel::white_sharp_panel;
use crate::theme::Colors;

/// How long a passing word ("Copied A to B") stays up.
const NOTICE: Duration = Duration::from_millis(2_200);

/// A menu the panel opened, at a window-space point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Menu {
    Preset(f32, f32),
    Input(f32, f32),
    Key(f32, f32),
    Scale(f32, f32),
}

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

pub struct WhiteSharpEditor {
    key: PluginInstanceKey,
    host_ops: BuiltinEditorHostOps,
    pub(crate) params: Params,
    pub(crate) preset: Option<usize>,
    pub(crate) compare: Compare,
    notice: Option<(String, Instant)>,
    pub(crate) menu: Option<Menu>,
    /// Which list a click on the keyboard puts a note on.
    pub(crate) key_edit: NoteList,
    /// The meter's and the keyboard's bounds, recorded each time the panel
    /// lays out; the window's overlay paints into them.
    pub(crate) meter_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(crate) keyboard_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
            menu: None,
            key_edit: NoteList::Removed,
            meter_bounds: Rc::new(Cell::new(None)),
            keyboard_bounds: Rc::new(Cell::new(None)),
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
            self.menu = None;
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
        self.menu = None;
        self.set_params(next, cx);
        cx.notify();
    }

    pub(crate) fn set_scale(&mut self, scale: whitesharp::Scale, cx: &mut Context<Self>) {
        let mut next = self.params.clone();
        next.scale = scale;
        next.remove_mask = 0;
        next.bypass_mask = 0;
        self.menu = None;
        self.set_params(next, cx);
        cx.notify();
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

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE)
            .map(|(text, _)| text.as_str())
    }

    // ── Presets and A/B ─────────────────────────────────────────────────

    pub(crate) fn load_preset(&mut self, index: isize, cx: &mut Context<Self>) {
        let bank = presets();
        let wrapped = index.rem_euclid(bank.len() as isize) as usize;
        let next = preset_applied(&self.params, &bank[wrapped].params);
        self.menu = None;
        self.set_params(next, cx);
        self.preset = Some(wrapped);
        cx.notify();
    }

    pub(crate) fn step_preset(&mut self, delta: isize, cx: &mut Context<Self>) {
        let from = match self.preset {
            Some(index) => index as isize,
            None if delta > 0 => -1,
            None => 0,
        };
        self.load_preset(from + delta, cx);
    }

    pub(crate) fn preset_label(&self) -> String {
        self.preset
            .and_then(|index| presets().get(index).map(|preset| preset.name))
            .unwrap_or("Edited")
            .to_string()
    }

    pub(crate) fn open_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = Some(menu);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn menu_entries(&self) -> Vec<ContextMenuEntry> {
        match self.menu {
            Some(Menu::Preset(..)) => {
                let mut entries = vec![ContextMenuEntry::Header("WhiteSharp presets".into())];
                entries.extend(presets().iter().enumerate().map(|(index, preset)| {
                    ContextMenuEntry::checked_item(
                        preset.name,
                        format!("preset:{index}"),
                        self.preset == Some(index),
                    )
                }));
                entries
            }
            Some(Menu::Input(..)) => {
                let mut entries = vec![ContextMenuEntry::Header("Input type".into())];
                entries.extend(whitesharp::InputType::ALL.iter().enumerate().map(
                    |(index, input)| {
                        ContextMenuEntry::checked_item(
                            input.label(),
                            format!("input:{index}"),
                            self.params.input_type == *input,
                        )
                    },
                ));
                entries
            }
            Some(Menu::Key(..)) => {
                let mut entries = vec![ContextMenuEntry::Header("Key".into())];
                entries.extend(
                    whitesharp::NOTE_NAMES
                        .iter()
                        .enumerate()
                        .map(|(index, name)| {
                            ContextMenuEntry::checked_item(
                                *name,
                                format!("key:{index}"),
                                usize::from(self.params.key) == index,
                            )
                        }),
                );
                entries
            }
            Some(Menu::Scale(..)) => {
                let mut entries = vec![ContextMenuEntry::Header("Scale".into())];
                entries.extend(
                    whitesharp::Scale::ALL
                        .iter()
                        .enumerate()
                        .map(|(index, scale)| {
                            ContextMenuEntry::checked_item(
                                scale.label(),
                                format!("scale:{index}"),
                                self.params.scale == *scale,
                            )
                        }),
                );
                entries
            }
            None => Vec::new(),
        }
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        self.menu = None;
        let number = |prefix: &str| {
            command
                .strip_prefix(prefix)
                .and_then(|index| index.parse::<usize>().ok())
        };
        if let Some(index) = number("preset:") {
            self.load_preset(index as isize, cx);
        } else if let Some(index) = number("input:") {
            if let Some(input) = whitesharp::InputType::ALL.get(index) {
                self.set_value("inputType", input.to_wire(), cx);
            }
        } else if let Some(key) = number("key:") {
            self.set_key(key as u8, cx);
        } else if let Some(index) = number("scale:") {
            if let Some(scale) = whitesharp::Scale::ALL.get(index) {
                self.set_scale(*scale, cx);
            }
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
}

impl Render for WhiteSharpEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        white_sharp_panel(self, cx)
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
        self.live
            .borrow_mut()
            .bind(self.key.clone(), self.host_ops.pad_level_source.clone());
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
        if event.keystroke.key == "escape" {
            self.editor.update(cx, |editor, cx| editor.close_menu(cx));
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
        // The meter follows the voice: a frame every frame, which reaches
        // this view and the overlay, not the cached panel.
        window.request_animation_frame();
        self.meter
            .poll(self.host_ops.meter_source.as_ref(), &self.key);

        let (meter_bounds, keyboard_bounds, menu, entries) = {
            let editor = self.editor.read(cx);
            (
                editor.meter_bounds.clone(),
                editor.keyboard_bounds.clone(),
                editor.menu,
                editor.menu_entries(),
            )
        };
        let live = self.live.clone();
        let overlay = canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                let (meter, keyboard) = (meter_bounds.get(), keyboard_bounds.get());
                let mut live = live.borrow_mut();
                live.paint(meter, keyboard, window, cx);
                // What is sung against its note, over the meter's middle.
                if let (Some(bounds), Some(text)) = (meter, live.readout()) {
                    let origin =
                        bounds.origin + gpui::point(bounds.size.width * 0.5 - px(64.0), px(-22.0));
                    paint_text(window, cx, &text, Colors::text_primary().into(), origin);
                }
            },
        )
        .absolute()
        .size_full();

        let menu = menu.map(|menu| {
            let (x, y) = match menu {
                Menu::Preset(x, y) | Menu::Input(x, y) | Menu::Key(x, y) | Menu::Scale(x, y) => {
                    (x, y)
                }
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
        let content = AnyView::from(self.editor.clone())
            .cached(gpui::StyleRefinement::default().size_full())
            .into_any_element();
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
            .child(overlay)
            .children(menu)
    }
}

pub const WHITESHARP_WINDOW_SIZE: (f32, f32) = (1_000.0, 820.0);
const MIN_WIDTH: f32 = 900.0;
const MIN_HEIGHT: f32 = 800.0;

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
