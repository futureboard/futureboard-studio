//! WrapSynth's native editor window.
//!
//! The editor of one WrapSynth insert — a built-in instrument that runs in
//! the plug-in host like every other — drawn natively with GPUI inside the
//! shared [`native_plugin_shell`] rather than as a web page in CEF, as a
//! plate ([`wrap_synth_plate`]) scaled to the window.
//!
//! It talks to the plug-in through the host ops `plugin_ops.rs` injects:
//!
//! * every edit is a wire param, sent with `forward_param` — which also folds
//!   it into Studio's state mirror (what the project saves and a restarted
//!   host replays) and marks the project edited;
//! * the keyboard plays through `preview_note`, the engine's plug-in preview.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::builtin_preset_menu::BuiltinPresetMenu;
use crate::components::native_plugin_shell::{
    claim_editor_transport_key, native_plugin_shell_with, swallow_editor_transport_key_up,
    NativeBuiltinEditor, ShellIdentity, ShellMeter, SHELL_METER_INTERVAL,
};
use crate::components::plate;
use crate::components::wrap_synth_plate::{
    wrap_synth_plate, SynthPage, WrapSynthCallbacks, WrapSynthPanelState,
};
use wrapsynth::Params;

/// The plate at its design size, and the smallest the window may get; the
/// plate scales to whatever size it is between.
pub const WRAP_SYNTH_WINDOW_WIDTH: f32 = plate::WINDOW_SIZE.0;
pub const WRAP_SYNTH_WINDOW_HEIGHT: f32 = plate::WINDOW_SIZE.1;
pub const WRAP_SYNTH_WINDOW_MIN_WIDTH: f32 = plate::MIN_WINDOW_SIZE.0;
pub const WRAP_SYNTH_WINDOW_MIN_HEIGHT: f32 = plate::MIN_WINDOW_SIZE.1;

const PREVIEW_VELOCITY: u8 = 100;

pub struct WrapSynthEditorWindow {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    panel: WrapSynthPanelState,
    meter: ShellMeter,
    /// The `.pst` preset menu in the identity strip.
    presets: gpui::Entity<BuiltinPresetMenu>,
    /// Cleared when the window closes, ending the meter timer.
    alive: Rc<Cell<bool>>,
}

impl WrapSynthEditorWindow {
    pub fn new(
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let presets = cx.new(|cx| BuiltinPresetMenu::new(key.clone(), &identity, cx));
        let mut window = Self {
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            panel: WrapSynthPanelState::default(),
            meter: ShellMeter::default(),
            presets,
            alive: Rc::new(Cell::new(true)),
        };
        plate::ensure_fonts(cx);
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
        self.panel.params =
            crate::components::builtin_plugin_editor::builtin_wrapsynth_params(&self.key.insert_id)
                .unwrap_or_else(wrapsynth::default_params);
        cx.notify();
    }

    /// Rebinds the window to another insert (the same plug-in opened from
    /// elsewhere), keeping the OS window.
    pub fn rebind(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        if key != self.key {
            self.release_all(cx);
            self.key = key;
        }
        let presets_key = self.key.clone();
        self.presets
            .update(cx, |menu, cx| menu.rebind(presets_key, &identity, cx));
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
    }

    /// Sends what changed between the panel's params and `next` as wire
    /// edits. Each one reaches the host DSP, the state mirror, and the
    /// project's dirty flag through `forward_param`.
    fn set_params(&mut self, mut next: Params, cx: &mut Context<Self>) {
        wrapsynth::ipc::sanitize_params(&mut next);
        let edits = wrapsynth::ipc::wire_diff(&self.panel.params, &next);
        if !next.power {
            // The DSP goes quiet at once; keys held here would come back on
            // when it is switched on again.
            self.release_all(cx);
        }
        self.panel.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        cx.notify();
    }

    /// The UI preview's scenes: one page, one patch, without a host.
    pub fn show_for_preview(&mut self, page: SynthPage, params: Params) {
        self.panel.page = page;
        self.panel.params = params;
    }

    fn preview(&self, pitch: u8, velocity: Option<u8>, cx: &mut App) {
        if let Some(preview) = self.host_ops.preview_note.as_ref() {
            preview(&self.key, 0, pitch, velocity, cx);
        }
    }

    fn note_on(&mut self, pitch: u8, cx: &mut Context<Self>) {
        if !self.panel.params.power || self.panel.active_notes.contains(&pitch) {
            return;
        }
        self.panel.active_notes.push(pitch);
        self.preview(pitch, Some(PREVIEW_VELOCITY), cx);
        cx.notify();
    }

    fn note_off(&mut self, pitch: u8, cx: &mut Context<Self>) {
        if !self.panel.active_notes.contains(&pitch) {
            return;
        }
        self.panel.active_notes.retain(|held| *held != pitch);
        self.preview(pitch, None, cx);
        cx.notify();
    }

    fn release_all(&mut self, cx: &mut App) {
        for pitch in std::mem::take(&mut self.panel.active_notes) {
            self.preview(pitch, None, cx);
        }
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

    fn callbacks(&self, cx: &mut Context<Self>) -> WrapSynthCallbacks {
        let entity = cx.entity().clone();
        /// `f` on this window with the value an element hands over.
        fn with<T: 'static>(
            entity: &gpui::Entity<WrapSynthEditorWindow>,
            f: impl Fn(&mut WrapSynthEditorWindow, T, &mut Context<WrapSynthEditorWindow>) + 'static,
        ) -> Rc<dyn Fn(T, &mut App)> {
            let entity = entity.clone();
            Rc::new(move |value: T, app: &mut App| {
                let _ = entity.update(app, |this, cx| f(this, value, cx));
            })
        }
        let note_on = with(&entity, |this, pitch: u8, cx| this.note_on(pitch, cx));
        let note_off = with(&entity, |this, pitch: u8, cx| this.note_off(pitch, cx));
        let shift = with(&entity, |this, delta: i32, cx| {
            this.release_all(cx);
            this.panel.shift_keyboard_octave(delta);
            cx.notify();
        });
        WrapSynthCallbacks {
            on_set_params: with(&entity, |this, params: Params, cx| {
                this.set_params(params, cx)
            }),
            on_page: with(&entity, |this, page: SynthPage, cx| {
                this.panel.page = page;
                cx.notify();
            }),
            on_note_on: Rc::new(move |pitch: u8, _window: &mut Window, app: &mut App| {
                note_on(pitch, app)
            }),
            on_note_off: Rc::new(move |pitch: u8, _window: &mut Window, app: &mut App| {
                note_off(pitch, app)
            }),
            on_shift_octave: Rc::new(move |delta: i32, _window: &mut Window, app: &mut App| {
                shift(delta, app)
            }),
        }
    }
}

impl NativeBuiltinEditor for WrapSynthEditorWindow {
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

impl Render for WrapSynthEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let alive = self.alive.clone();
        let viewport = window.viewport_size();
        let s = plate::plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let content = wrap_synth_plate(&self.panel, self.callbacks(cx), s);

        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .capture_key_down(|event, window, cx| {
                claim_editor_transport_key(event, window, cx);
            })
            .on_key_up(|_event, window, cx| swallow_editor_transport_key_up(window, cx))
            // A key is held while the button is down; let go anywhere — off
            // the key, off the keyboard — and it stops, so a sustained patch
            // is never left droning.
            .on_mouse_up(gpui::MouseButton::Left, {
                let entity = entity.clone();
                move |_, _window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        if !this.panel.active_notes.is_empty() {
                            this.release_all(cx);
                            cx.notify();
                        }
                    });
                }
            })
            .on_mouse_up_out(gpui::MouseButton::Left, {
                let entity = entity.clone();
                move |_, _window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        if !this.panel.active_notes.is_empty() {
                            this.release_all(cx);
                            cx.notify();
                        }
                    });
                }
            })
            .child(native_plugin_shell_with(
                "wrap-synth-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // Closing must not leave a note held — nothing would
                    // ever send its note-off.
                    let _ = entity.update(cx, |this, cx| this.release_all(cx));
                    alive.set(false);
                    on_close(window, cx);
                    window.remove_window();
                },
                Some(self.presets.clone().into_any_element()),
                content,
            ))
    }
}

pub fn open_wrap_synth_editor(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<WrapSynthEditorWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(WRAP_SYNTH_WINDOW_WIDTH), px(WRAP_SYNTH_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(WRAP_SYNTH_WINDOW_MIN_WIDTH),
        px(WRAP_SYNTH_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| WrapSynthEditorWindow::new(key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}
