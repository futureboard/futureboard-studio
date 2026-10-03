//! WrapSynth's native editor window.
//!
//! The editor of one WrapSynth insert — a built-in instrument that runs in
//! the plug-in host like every other — drawn natively with GPUI inside the
//! shared [`native_plugin_shell`] rather than as a web page in CEF.
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
    App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    div, px, size,
};

use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::native_plugin_shell::{
    NativeBuiltinEditor, SHELL_METER_INTERVAL, ShellIdentity, ShellMeter, native_plugin_shell,
};
use crate::components::wrap_synth_panel::{
    WrapSynthCallbacks, WrapSynthPanelState, wrap_synth_panel,
};
use wrapsynth::Params;

/// Opens with both oscillators on one row and the filter, envelope and
/// output on the next, keyboard below. Narrower, the cards wrap and the body
/// scrolls; the header and keyboard stay put.
pub const WRAP_SYNTH_WINDOW_WIDTH: f32 = 980.0;
pub const WRAP_SYNTH_WINDOW_HEIGHT: f32 = 720.0;
pub const WRAP_SYNTH_WINDOW_MIN_WIDTH: f32 = 660.0;
pub const WRAP_SYNTH_WINDOW_MIN_HEIGHT: f32 = 520.0;

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
        let mut window = Self {
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            panel: WrapSynthPanelState::default(),
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
        fn with<T: Clone + 'static>(
            entity: &gpui::Entity<WrapSynthEditorWindow>,
            f: impl Fn(&mut WrapSynthEditorWindow, T, &mut Context<WrapSynthEditorWindow>) + 'static,
        ) -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |value: &T, _window, app: &mut App| {
                let value = value.clone();
                let _ = entity.update(app, |this, cx| f(this, value, cx));
            })
        }
        WrapSynthCallbacks {
            on_set_params: with(&entity, |this, params: Params, cx| {
                this.set_params(params, cx)
            }),
            on_note_on: with(&entity, |this, pitch: u8, cx| this.note_on(pitch, cx)),
            on_note_off: with(&entity, |this, pitch: u8, cx| this.note_off(pitch, cx)),
            on_shift_octave: with(&entity, |this, delta: i32, cx| {
                this.release_all(cx);
                this.panel.shift_keyboard_octave(delta);
                cx.notify();
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
        let content = wrap_synth_panel(&self.panel, self.callbacks(cx));

        div()
            .size_full()
            .track_focus(&self.focus_handle)
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
            .child(native_plugin_shell(
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
