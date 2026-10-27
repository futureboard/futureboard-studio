//! The editor: Studio's WhiteSharp window, given host ops that read and
//! write this plug-in instead of Studio's engine.

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    div, AppContext, AssetSource, Context, Entity, IntoElement, ParentElement, Render,
    SharedString, Styled, Window,
};
use nih_plug::prelude::Editor;
use nih_plug_gpui::{EditorContext, GpuiState};
use whitesharp::ipc::PARAM_COUNT;

use crate::assets;
use crate::components::builtin_plugin_editor;
use crate::components::builtin_plugin_editor_window::{
    BuiltinEditorHostOps, BuiltinMeterFrame, PluginInstanceKey,
};
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::white_sharp_panel::DOCK_H;
use crate::components::white_sharp_style::{PLATE_H, PLATE_W};
use crate::components::white_sharp_window::WhiteSharpWindow;
use crate::params::WhiteSharpParams;
use crate::shared::Shared;

/// How often the editor looks for host-side param changes (automation, a
/// host preset, an undo) and for the dock opening or closing.
const POLL: Duration = Duration::from_millis(16);
/// The window and its live layers both read the meters every frame; within
/// this they get the same reading instead of the second getting none.
const METER_REUSE: Duration = Duration::from_millis(4);

/// The plate at its design size, the dock closed. Not persisted: the editor
/// always opens with the dock closed.
pub fn default_state() -> Arc<GpuiState> {
    GpuiState::from_size(PLATE_W as u32, PLATE_H as u32)
}

pub fn create(
    params: Arc<WhiteSharpParams>,
    shared: Arc<Shared>,
    state: Arc<GpuiState>,
) -> Option<Box<dyn Editor>> {
    // Every editor in the process shares one GPUI app, and so one source;
    // after the first, this returns `false` and changes nothing.
    nih_plug_gpui::set_asset_source(Assets);
    nih_plug_gpui::create_gpui_editor(state, move |context, _window, cx| {
        let context = context.clone();
        let params = params.clone();
        let shared = shared.clone();
        cx.new(|cx| PluginEditor::new(context, params, shared, cx))
    })
}

struct PluginEditor {
    context: EditorContext,
    params: Arc<WhiteSharpParams>,
    window: Entity<WhiteSharpWindow>,
    seen: [f32; PARAM_COUNT],
    dock_open: bool,
}

impl PluginEditor {
    fn new(
        context: EditorContext,
        params: Arc<WhiteSharpParams>,
        shared: Arc<Shared>,
        cx: &mut Context<Self>,
    ) -> Self {
        gpui::set_global_text_scale(crate::theme::UI_TEXT_SCALE);

        let key = PluginInstanceKey {
            track_id: String::new(),
            insert_id: format!("{:p}", Arc::as_ptr(&params)),
        };
        builtin_plugin_editor::register(&key.insert_id, &params);

        let host_ops = host_ops(&context, &params, &shared);
        let identity = ShellIdentity {
            plugin_name: "WhiteSharp".to_string(),
            track_name: String::new(),
            insert_number: 1,
        };
        let window =
            cx.new(|cx| WhiteSharpWindow::new(key, identity, host_ops, Arc::new(|_, _| {}), cx));

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(POLL).await;
            if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                break;
            }
        })
        .detach();

        Self {
            context,
            seen: params.wire_values(),
            params,
            window,
            dock_open: false,
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let values = self.params.wire_values();
        if values != self.seen {
            self.seen = values;
            self.window
                .update(cx, |window, cx| window.sync_from_mirror(cx));
        }

        // The panel grows the window by the dock rather than squeezing the
        // plate; inside a host that is the host's window to resize.
        let dock_open = self.window.read(cx).editor().read(cx).dock.is_some();
        if dock_open != self.dock_open {
            self.dock_open = dock_open;
            let height = PLATE_H + if dock_open { DOCK_H } else { 0.0 };
            self.context
                .request_resize(PLATE_W as u32, height.round() as u32);
        }
    }
}

impl Render for PluginEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.window.clone())
    }
}

fn host_ops(
    context: &EditorContext,
    params: &Arc<WhiteSharpParams>,
    shared: &Arc<Shared>,
) -> BuiltinEditorHostOps {
    let forward_params = params.clone();
    let forward_context = context.clone();
    let meter_shared = shared.clone();
    let last_meter: Rc<Cell<Option<(Instant, BuiltinMeterFrame)>>> = Rc::default();
    let status_shared = shared.clone();
    let telemetry_shared = shared.clone();
    BuiltinEditorHostOps {
        forward_param: Some(Arc::new(move |_key, index, value, _cx| {
            forward_params.set_from_editor(&forward_context, index, value);
        })),
        meter_source: Some(Arc::new(move |_key| {
            let now = Instant::now();
            if let Some((at, frame)) = last_meter.get() {
                if now.duration_since(at) < METER_REUSE {
                    return Some(frame);
                }
            }
            let reading = meter_shared.take_meter();
            let frame = BuiltinMeterFrame {
                in_peak: reading.in_peak,
                in_rms: reading.in_rms,
                out_peak: reading.out_peak,
                out_rms: reading.out_rms,
                in_clip: reading.in_peak > 1.0,
                out_clip: reading.out_peak > 1.0,
            };
            last_meter.set(Some((now, frame)));
            Some(frame)
        })),
        host_status_source: Some(Arc::new(move |_key| {
            let (rate, block, latency) = status_shared.stream();
            // No tempo: nothing in WhiteSharp's editor reads it.
            (rate > 0).then_some((rate, block, latency, 0.0))
        })),
        pad_level_source: Some(Arc::new(move |_key| telemetry_shared.telemetry())),
    }
}

/// The two assets the WhiteSharp editor draws: its wordmark and the
/// checkbox tick.
struct Assets;

const ASSET_PATHS: [&str; 2] = [assets::WHITESHARP_WORDMARK_PATH, assets::ICON_CHECK_PATH];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let bytes = match path {
            assets::WHITESHARP_WORDMARK_PATH => Some(assets::WHITESHARP_WORDMARK.as_bytes()),
            assets::ICON_CHECK_PATH => Some(assets::icons::CHECK.as_bytes()),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ASSET_PATHS
            .into_iter()
            .filter(|asset| asset.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
