//! The editor: Studio's Voxtune editor, with a header for what a plug-in
//! adds — the ARA region it shows, or (plain VST3) opening a take and
//! aligning it to the timeline.

use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{div, px, App, AssetSource, Context, Entity, PathPromptOptions, SharedString, Window};
use nih_plug::prelude::Editor;
use nih_plug_gpui::{EditorContext, GpuiState};
use sphere_voxtune::session::VoxDocument;

use crate::assets;
use crate::components::controls::{fb_badge, fb_button, fb_progress, FbButtonKind};
use crate::components::voxtune::{VoxTransport, VoxtuneEditor, VoxtuneHost};
use crate::instance::{take_store, Instance, LoadStatus, PersistedTake};
use crate::theme::{space, typography, Colors};
use crate::VoxParams;

const POLL: Duration = Duration::from_millis(50);
const WIDTH: u32 = 1180;
const HEIGHT: u32 = 720;

pub fn default_state() -> Arc<GpuiState> {
    GpuiState::from_size(WIDTH, HEIGHT)
}

pub fn create(params: Arc<VoxParams>, instance: Arc<Instance>) -> Option<Box<dyn Editor>> {
    nih_plug_gpui::set_asset_source(Assets);
    let state = params.editor_state.clone();
    nih_plug_gpui::create_gpui_editor(state, move |context, _window, cx| {
        let context = context.clone();
        let params = params.clone();
        let instance = instance.clone();
        cx.new(|cx| PluginEditor::new(context, params, instance, cx))
    })
}

/// The editor's view of the host: transport from the audio thread, edits
/// to the plug-in state (plain) or the ARA modification.
struct PluginHost {
    params: Arc<VoxParams>,
    instance: Arc<Instance>,
}

impl PluginHost {
    fn focused_region(&self) -> Option<Arc<crate::ara::PlaybackRegion>> {
        self.instance.ara.lock().as_ref()?.focus()
    }
}

impl VoxtuneHost for PluginHost {
    fn transport(&self, _cx: &App) -> VoxTransport {
        let playing = self.instance.playing.load(Ordering::Relaxed);
        let host = self.instance.position();
        let position = match self.instance.ara_bound() {
            true => match self.focused_region() {
                Some(r) => r.start_in_modification() + (host - r.start_in_playback()),
                None => 0.0,
            },
            false => host - self.instance.offset(),
        };
        VoxTransport {
            playing,
            position: position.max(0.0),
        }
    }

    fn controls_transport(&self) -> bool {
        false
    }

    fn set_playing(&mut self, _playing: bool, _cx: &mut App) {}

    fn seek(&mut self, _seconds: f64, _cx: &mut App) {}

    fn can_audition(&self) -> bool {
        // Plain mode renders the take engine even while stopped; ARA renders
        // only the regions under the playhead.
        !self.instance.ara_bound()
    }

    fn document_changed(&mut self, doc: &VoxDocument, _cx: &mut App) {
        if self.instance.ara_bound() {
            if let Some(region) = self.focused_region() {
                region.modification.store_edit(doc);
            }
            return;
        }
        let mut take = self.params.take.write();
        if let Some(take) = take.as_mut() {
            take.doc = Some(doc.clone());
            let _ = take_store().write_document(&take.path, doc);
        }
    }
}

struct PluginEditor {
    context: EditorContext,
    params: Arc<VoxParams>,
    instance: Arc<Instance>,
    editor: Entity<VoxtuneEditor>,
    /// What the editor shows: the engine generation (plain) or the ARA
    /// region and its modification's generation.
    seen: (u64, usize, u64),
    seen_load: u64,
}

impl PluginEditor {
    fn new(
        context: EditorContext,
        params: Arc<VoxParams>,
        instance: Arc<Instance>,
        cx: &mut Context<Self>,
    ) -> Self {
        gpui::set_global_text_scale(crate::theme::UI_TEXT_SCALE);
        let _ = crate::theme::initialize_theme_system();
        let host = PluginHost {
            params: params.clone(),
            instance: instance.clone(),
        };
        let engine = instance.engine();
        let editor = cx.new(|cx| {
            let mut editor = VoxtuneEditor::new(engine, Box::new(host), cx);
            editor.set_empty_hint(
                if instance.ara_bound() {
                    "Select a region in your DAW to tune it."
                } else {
                    "Open a vocal take, then align it to the timeline."
                },
                cx,
            );
            editor
        });
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(POLL).await;
            if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                break;
            }
        })
        .detach();
        let mut me = Self {
            context,
            params,
            instance,
            editor,
            seen: (0, 0, 0),
            seen_load: u64::MAX,
        };
        me.poll(cx);
        me
    }

    /// Picks up a newly loaded take, a new ARA selection, or a restored
    /// archive, and shows it.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let load = self.instance.load_generation.load(Ordering::Acquire);
        if load != self.seen_load {
            self.seen_load = load;
            cx.notify();
        }
        let ara = self.instance.ara.lock().clone();
        match ara {
            Some(binding) => {
                let region = binding.focus();
                let key = match &region {
                    Some(r) => (
                        binding.focus_generation.load(Ordering::Acquire),
                        Arc::as_ptr(r) as usize,
                        r.modification.generation.load(Ordering::Acquire),
                    ),
                    None => (0, 0, 0),
                };
                if key != self.seen {
                    self.seen = key;
                    if let Some(region) = region {
                        let m = &region.modification;
                        let doc = m.doc.lock().clone();
                        let title = m.source.name.lock().clone();
                        let engine = m.engine.clone();
                        self.editor.update(cx, |editor, cx| {
                            editor.set_engine(engine, cx);
                            if let Some(doc) = doc {
                                editor.adopt(doc, title, cx);
                            }
                        });
                    }
                    cx.notify();
                } else if binding.focus_status().is_some_and(|(_, p, _)| p.is_some()) {
                    cx.notify();
                }
            }
            None => {
                let generation = self.instance.generation.load(Ordering::Acquire);
                if (generation, 0, 0) != self.seen && self.instance.take_len() > 0.0 {
                    self.seen = (generation, 0, 0);
                    let engine = self.instance.engine();
                    let doc = VoxDocument::from_engine(&engine.lock());
                    let title = self
                        .params
                        .take
                        .read()
                        .as_ref()
                        .and_then(|t| t.path.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_default();
                    self.editor.update(cx, |editor, cx| {
                        editor.set_engine(engine, cx);
                        editor.adopt(doc, title, cx);
                    });
                    cx.notify();
                }
                if matches!(*self.instance.load.lock(), LoadStatus::Loading { .. }) {
                    cx.notify();
                }
            }
        }
    }

    fn open_take(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open vocal take".into()),
        });
        let params = self.params.clone();
        let instance = self.instance.clone();
        cx.spawn(async move |_, _| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            // The edits last made to this file, if any.
            let doc = take_store().read_document(&path);
            let offset = instance.offset();
            *params.take.write() = Some(PersistedTake {
                path: path.clone(),
                offset_sec: offset,
                doc: doc.clone(),
            });
            instance.load_take(path, offset, doc);
        })
        .detach();
    }

    /// The take starts where the host's playhead is now.
    fn align_to_playhead(&mut self, cx: &mut Context<Self>) {
        let at = self.instance.position();
        self.instance.set_offset(at);
        if let Some(take) = self.params.take.write().as_mut() {
            take.offset_sec = at;
        }
        cx.notify();
    }
}

impl Render for PluginEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.context;
        let ara = self.instance.ara.lock().clone();
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .h(px(36.0))
            .px(px(space::BASE))
            .bg(Colors::surface_titlebar())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle());
        match ara {
            Some(binding) => {
                bar = bar.child(fb_badge("ARA", Colors::accent_primary()));
                let status = binding.focus_status();
                let region = binding.focus().map(|r| r.name.lock().clone());
                bar = bar.child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::text_secondary())
                        .child(match (&status, region) {
                            (Some((take, _, _)), Some(region)) if !region.is_empty() => {
                                format!("{region} · {take}")
                            }
                            (Some((take, _, _)), _) => take.clone(),
                            (None, _) => "No region selected".to_string(),
                        }),
                );
                match status {
                    Some((_, Some(p), _)) => {
                        bar = bar.child(div().w(px(160.0)).child(fb_progress(p))).child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .child(format!("Reading and analysing… {}%", (p * 100.0).round())),
                        );
                    }
                    Some((_, None, true)) => {
                        bar = bar.child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::status_warning())
                                .child("The host's audio could not be read."),
                        );
                    }
                    _ => {}
                }
            }
            None => {
                let loading = self.instance.load.lock().clone();
                let take = self.params.take.read().clone();
                bar = bar
                    .child(fb_button(
                        "voxtune-plugin-open",
                        "Open take…",
                        FbButtonKind::Default,
                        true,
                        cx.listener(|this, _, _, cx| this.open_take(cx)),
                    ))
                    .child(fb_button(
                        "voxtune-plugin-align",
                        "Align to playhead",
                        FbButtonKind::Default,
                        take.is_some(),
                        cx.listener(|this, _, _, cx| this.align_to_playhead(cx)),
                    ))
                    .when_some(take, |d, t| {
                        d.child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .child(format!("Starts at {:.2}s on the timeline", t.offset_sec)),
                        )
                    });
                match loading {
                    LoadStatus::Loading { title, progress } => {
                        let p = progress.permille.load(Ordering::Relaxed) as f32 / 1000.0;
                        bar = bar.child(div().w(px(160.0)).child(fb_progress(p))).child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .child(format!("Loading {title}…")),
                        );
                    }
                    LoadStatus::Failed(e) => {
                        bar = bar.child(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::status_warning())
                                .child(e),
                        );
                    }
                    LoadStatus::Idle => {}
                }
            }
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .child(bar)
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

/// The icons the Voxtune editor and its controls draw.
struct Assets;

const ICONS: &[(&str, &str)] = &[
    (assets::ICON_PLAY_PATH, assets::icons::PLAY),
    (assets::ICON_PAUSE_PATH, assets::icons::PAUSE),
    (assets::ICON_SKIP_BACK_PATH, assets::icons::SKIP_BACK),
    (assets::ICON_REPEAT_PATH, assets::icons::REPEAT),
    (assets::ICON_UNDO_PATH, assets::icons::UNDO),
    (assets::ICON_REDO_PATH, assets::icons::REDO),
    (assets::ICON_HISTORY_PATH, assets::icons::HISTORY),
    (assets::ICON_ZOOM_IN_PATH, assets::icons::ZOOM_IN),
    (assets::ICON_ZOOM_OUT_PATH, assets::icons::ZOOM_OUT),
    (assets::ICON_SCAN_PATH, assets::icons::SCAN),
    (assets::ICON_CHEVRON_DOWN_PATH, assets::icons::CHEVRON_DOWN),
    (assets::ICON_CHECK_PATH, assets::icons::CHECK),
    (
        assets::ICON_MOUSE_POINTER_PATH,
        assets::icons::MOUSE_POINTER,
    ),
    (assets::ICON_AUDIO_LINES_PATH, assets::icons::AUDIO_LINES),
    (
        assets::ICON_MOVE_HORIZONTAL_PATH,
        assets::icons::MOVE_HORIZONTAL,
    ),
    (assets::ICON_SPARKLES_PATH, assets::icons::SPARKLES),
    (assets::ICON_VOLUME_2_PATH, assets::icons::VOLUME_2),
    (assets::ICON_SCISSORS_PATH, assets::icons::SCISSORS),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = crate::components::voxtune::icons::svg(path) {
            return Ok(Some(Cow::Borrowed(svg.as_bytes())));
        }
        Ok(ICONS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, svg)| Cow::Borrowed(svg.as_bytes())))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(p, _)| *p)
            .chain(crate::components::voxtune::icons::ALL.iter().copied())
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
