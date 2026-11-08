//! The editor: Studio's Rodhareist window, given host ops that read and
//! write this plug-in instead of Studio's engine.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    Styled, Window,
};
use nih_plug::prelude::Editor;
use nih_plug_gpui::{EditorContext, GpuiState};

use crate::components::builtin_plugin_editor;
use crate::components::builtin_plugin_editor_window::{
    BuiltinEditorHostOps, BuiltinMeterFrame, PluginInstanceKey,
};
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::rodhareist_window::{
    RodhareistEditorWindow, RODHAREIST_WINDOW_HEIGHT, RODHAREIST_WINDOW_WIDTH,
};
use crate::files::Task;
use crate::params::{EntryKind, RodhareistParams, CLEAR_CLIP};
use crate::shared::Shared;
use rodharerist::StageKind;

/// How often the editor looks for host-side param changes (automation, a
/// host preset, an undo) and for answered file loads.
const POLL: Duration = Duration::from_millis(16);

/// The plate at its design size. It scales with the window.
pub fn default_state() -> Arc<GpuiState> {
    GpuiState::from_size(
        RODHAREIST_WINDOW_WIDTH.round() as u32,
        RODHAREIST_WINDOW_HEIGHT.round() as u32,
    )
}

/// The id the editor's state mirror knows this instance by.
pub fn insert_id(params: &Arc<RodhareistParams>) -> String {
    format!("{:p}", Arc::as_ptr(params))
}

pub fn create(
    params: Arc<RodhareistParams>,
    shared: Arc<Shared>,
    state: Arc<GpuiState>,
) -> Option<Box<dyn Editor>> {
    // Every editor in the process shares one GPUI app, and so one source;
    // after the first, this returns `false` and changes nothing.
    nih_plug_gpui::set_asset_source(crate::embedded_assets::EmbeddedAssets::new());
    nih_plug_gpui::create_gpui_editor_with_wayland_fallback(state, move |context, _window, cx| {
        let context = context.clone();
        let params = params.clone();
        let shared = shared.clone();
        cx.new(|cx| PluginEditor::new(context, params, shared, cx))
    })
}

/// Builds a picked capture or IR off the UI thread. Not nih-plug's
/// `AsyncExecutor`: the one `Plugin::editor` receives holds the wrapper
/// strongly, so an editor that kept it would never let the plug-in go.
fn load_in_background(params: &Arc<RodhareistParams>, shared: &Arc<Shared>, task: Task) {
    let params = params.clone();
    let shared = shared.clone();
    let insert_id = insert_id(&params);
    let spawned = std::thread::Builder::new()
        .name("rodhareist-file-load".to_string())
        .spawn(move || crate::files::run(task, &shared, &params, &insert_id));
    if spawned.is_err() {
        nih_plug::nih_log!("rodhareist: could not start a file-load thread");
    }
}

struct PluginEditor {
    params: Arc<RodhareistParams>,
    shared: Arc<Shared>,
    window: Entity<RodhareistEditorWindow>,
    seen: Vec<f32>,
    answers: (u32, u32),
    /// A text field had focus at the last render, so the editor holds the
    /// host's keyboard.
    keyboard: bool,
    /// A press landed in the editor since the last render.
    clicked: bool,
}

impl PluginEditor {
    fn new(
        context: EditorContext,
        params: Arc<RodhareistParams>,
        shared: Arc<Shared>,
        cx: &mut Context<Self>,
    ) -> Self {
        gpui::set_global_text_scale(crate::theme::UI_TEXT_SCALE);

        let key = PluginInstanceKey {
            track_id: String::new(),
            insert_id: insert_id(&params),
        };
        builtin_plugin_editor::register(&key.insert_id, &params);

        let host_ops = host_ops(&context, &params, &shared);
        let identity = ShellIdentity {
            plugin_name: "Rodhareist".to_string(),
            track_name: String::new(),
            insert_number: 1,
        };
        let window = cx.new(|cx| {
            RodhareistEditorWindow::new(key, identity, host_ops, Arc::new(|_, _| {}), cx)
        });

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(POLL).await;
            if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                break;
            }
        })
        .detach();

        // A model change that arrived while this editor was closed.
        params.wire.refresh_host_names();
        Self {
            seen: params.wire.values(),
            answers: shared.answers(),
            params,
            shared,
            window,
            keyboard: false,
            clicked: false,
        }
    }

    /// Takes the host's keyboard when a text field gets focus, or is clicked
    /// again after the host took the keyboard back, and returns it when no
    /// field has focus. Inside a host the editor is a child window, which
    /// Windows never gives the keyboard on its own; held all the time it
    /// would keep Space and the host's other shortcuts from the host.
    fn route_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typing = self.window.read(cx).typing(window);
        let clicked = std::mem::take(&mut self.clicked);
        let take = typing && (clicked || !self.keyboard);
        let give_back = !typing && self.keyboard;
        self.keyboard = typing;
        if take || give_back {
            window.defer(cx, move |window, _cx| {
                nih_plug_gpui::set_keyboard_focus(window, take);
            });
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let values = self.params.wire.values();
        if values != self.seen {
            self.seen = values;
            // Host automation of a model (the editor did not call
            // `set_from_editor`) still has to rename the MIDI titles.
            self.params.wire.refresh_host_names();
            self.window
                .update(cx, |window, cx| window.sync_from_mirror(cx));
        }
        let answers = self.shared.answers();
        if answers != self.answers {
            let (capture, impulse) = (answers.0 != self.answers.0, answers.1 != self.answers.1);
            self.answers = answers;
            self.window.update(cx, |window, cx| {
                if capture {
                    window.notify_nam_capture_result(true, "", None, cx);
                }
                if impulse {
                    window.notify_ir_load_result(true, "", None, 0, cx);
                }
            });
        }
    }
}

impl Render for PluginEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.route_keyboard(window, cx);
        div()
            .size_full()
            // Capture phase: seen before the field's own handlers, which may
            // stop the press from bubbling.
            .capture_any_mouse_down(cx.listener(|this, _event, _window, cx| {
                this.clicked = true;
                cx.notify();
            }))
            .child(self.window.clone())
    }
}

/// A chain edit (add, move, remove a block) reaches `forward_param` as one
/// call per slot, all inside one UI update. Those writes are collected and
/// sent at the end of the update, after the order they end in is published:
/// the host may deliver them over several audio callbacks, and the audio
/// thread holds the running chain until the published order has arrived.
#[derive(Clone)]
struct ChainEdit {
    params: Arc<RodhareistParams>,
    shared: Arc<Shared>,
    context: EditorContext,
    /// `(slot, wire index, value)` of the edit being collected.
    staged: Rc<RefCell<Vec<(usize, u32, f32)>>>,
}

impl ChainEdit {
    fn new(params: &Arc<RodhareistParams>, shared: &Arc<Shared>, context: &EditorContext) -> Self {
        Self {
            params: params.clone(),
            shared: shared.clone(),
            context: context.clone(),
            staged: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Collects a `path_slot_*` write. `false` for any other parameter.
    fn stage(&self, index: u32, value: f32, cx: &mut gpui::App) -> bool {
        let slot = rodharerist::ui_param_id(index)
            .and_then(|id| self.params.wire.get(id))
            .and_then(|entry| match entry.kind {
                EntryKind::PathSlot(slot) => Some(slot),
                EntryKind::Value => None,
            });
        let Some(slot) = slot else {
            return false;
        };
        let first = {
            let mut staged = self.staged.borrow_mut();
            let first = staged.is_empty();
            staged.retain(|(staged_slot, _, _)| *staged_slot != slot);
            staged.push((slot, index, value));
            first
        };
        if first {
            let edit = self.clone();
            cx.defer(move |_cx| edit.flush());
        }
        true
    }

    fn flush(&self) {
        let staged = std::mem::take(&mut *self.staged.borrow_mut());
        if staged.is_empty() {
            return;
        }
        // On top of an edit the host is still delivering, not of the values
        // it has not delivered yet.
        let mut target = self
            .shared
            .path_target()
            .unwrap_or_else(|| self.params.wire.path_order());
        for &(slot, _, value) in &staged {
            target[slot] = StageKind::from_index(value.round() as i32);
        }
        self.shared.publish_path_target(&target);
        for (_, index, value) in staged {
            self.params.set_from_editor(&self.context, index, value);
        }
    }
}

fn host_ops(
    context: &EditorContext,
    params: &Arc<RodhareistParams>,
    shared: &Arc<Shared>,
) -> BuiltinEditorHostOps {
    let clear_clip = rodharerist::ui_param_index(CLEAR_CLIP);
    let forward_params = params.clone();
    let forward_shared = shared.clone();
    let forward_context = context.clone();
    let chain_edit = ChainEdit::new(params, shared, context);
    let meter_shared = shared.clone();
    let status_shared = shared.clone();
    let telemetry_shared = shared.clone();
    let (nam_params, nam_shared) = (params.clone(), shared.clone());
    let (ir_params, ir_shared) = (params.clone(), shared.clone());
    BuiltinEditorHostOps {
        forward_param: Some(Arc::new(move |_key, index, value, cx| {
            if Some(index) == clear_clip {
                forward_shared
                    .clear_clip
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            } else if !chain_edit.stage(index, value, cx) {
                forward_params.set_from_editor(&forward_context, index, value);
            }
        })),
        load_nam_capture: Some(Arc::new(move |_key, request| {
            load_in_background(
                &nam_params,
                &nam_shared,
                Task::LoadCapture {
                    name: request.name,
                    json: request.json,
                },
            );
        })),
        load_ir: Some(Arc::new(move |_key, request| {
            load_in_background(
                &ir_params,
                &ir_shared,
                Task::LoadImpulse {
                    name: request.name,
                    bytes: request.bytes,
                },
            );
        })),
        meter_source: Some(Arc::new(move |_key| {
            let m = meter_shared.meter();
            Some(BuiltinMeterFrame {
                in_peak: m.in_peak,
                in_rms: m.in_rms,
                out_peak: m.out_peak,
                out_rms: m.out_rms,
                in_clip: m.in_clip,
                out_clip: m.out_clip,
            })
        })),
        host_status_source: Some(Arc::new(move |_key| {
            let (rate, block, latency, tempo) = status_shared.stream();
            (rate > 0).then_some((rate, block, latency, f64::from(tempo)))
        })),
        pad_level_source: Some(Arc::new(move |_key| telemetry_shared.telemetry())),
    }
}
