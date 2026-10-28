//! Studio's built-in vocal tuner: the Voxtune editor on the selected audio
//! clip, as a sub-tab of the bottom Editor panel.
//!
//! The editor is the shared [`VoxtuneEditor`]; this file connects it to the
//! arrangement:
//!
//! * **Source.** The clip's take is analysed from its *original* file. Once
//!   tuned audio has been applied, the clip plays a rendered version
//!   (`<name>-edit-NNN.wav`) with a `.voxtune.json` sidecar naming the
//!   original, where in it the version starts, and the edits — so reopening
//!   the clip edits the original again, never a render of a render.
//! * **Transport.** The editor follows Studio's transport: its playhead is the
//!   arrangement playhead mapped through the clip ([`ClipMap`]), and play /
//!   seek drive Studio's transport.
//! * **Apply.** Edits are heard in the arrangement after *Apply to clip*: the
//!   take is rendered off the UI thread, the clip's window is written as a new
//!   version, and the clip is pointed at it through the same path as every
//!   destructive audio edit — one Studio undo step, original kept on disk.
//!
//! The engine here has no output device, so pressing a note does not preview
//! it (the editor knows: [`VoxtuneHost::can_audition`] is false).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, Subscription, Window};
use serde::{Deserialize, Serialize};
use sphere_voxtune::engine::{CantoAnalysisResult, CantoEngine};
use sphere_voxtune::load_job::{JobKind, LoadJob};
use sphere_voxtune::session::VoxDocument;
use sphere_voxtune::shared::Shared;
use sphere_voxtune::store::TakeStore;

use crate::audio_edit::NewSource;
use crate::components::audio_editor::ClipMap;
use crate::components::controls::{fb_badge, fb_button, FbButtonKind};
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::{ClipState, ClipType, TimelineState};
use crate::components::voxtune::{VoxTransport, VoxtuneEditor, VoxtuneHost};
use crate::theme::{space, typography, Colors};

/// What Studio lends the panel.
#[derive(Clone)]
pub struct VoxtunePanelCallbacks {
    pub project_folder: Rc<dyn Fn(&App) -> Option<PathBuf>>,
    /// Points the clip at a rendered version (one undo step).
    pub replace_source: Rc<dyn Fn(String, NewSource, &mut App)>,
    /// Starts or stops Studio's transport.
    pub set_playing: Rc<dyn Fn(bool, &mut App)>,
}

/// Sidecar of an applied version: where its audio came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoxtuneSidecar {
    /// The original take.
    pub original: PathBuf,
    /// First frame of the original this version starts at.
    pub offset_frames: u64,
    pub sample_rate: u32,
    /// The edits this version was rendered with.
    pub document: VoxDocument,
}

impl VoxtuneSidecar {
    pub fn path_for(version: &Path) -> PathBuf {
        let mut name = version.as_os_str().to_owned();
        name.push(".voxtune.json");
        PathBuf::from(name)
    }

    pub fn read(version: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path_for(version)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write(&self, version: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(Self::path_for(version), json)
    }
}

/// The clip the editor is on, and how its file relates to the original.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    clip_id: String,
    /// The file the clip plays now.
    source: PathBuf,
    /// The take the editor analysed.
    original: PathBuf,
    /// Where `source` starts inside `original`, in frames at `rate`.
    offset_frames: u64,
    rate: u32,
    /// Edits `source` was rendered with (None: the original itself).
    applied: Option<VoxDocument>,
}

fn clip_source(clip: &ClipState) -> Option<PathBuf> {
    match &clip.clip_type {
        ClipType::Audio {
            source_path: Some(path),
            ..
        } => Some(PathBuf::from(path)),
        _ => None,
    }
}

fn clip_map(state: &TimelineState, clip: &ClipState) -> Option<ClipMap> {
    let total = clip.stretch.original_duration_samples;
    ClipMap::build(clip, total, state.bpm.max(1.0) as f64, |b| {
        state.seconds_at_beat(b)
    })
}

struct StudioHost {
    timeline: Entity<Timeline>,
    target: Rc<RefCell<Option<Target>>>,
    callbacks: Rc<RefCell<Option<VoxtunePanelCallbacks>>>,
    store: Rc<TakeStore>,
}

impl StudioHost {
    fn with_clip<R>(
        &self,
        cx: &App,
        f: impl FnOnce(&TimelineState, &ClipState, &Target) -> R,
    ) -> Option<R> {
        let target = self.target.borrow();
        let target = target.as_ref()?;
        let state = &self.timeline.read(cx).state;
        let (_, clip) = state.find_clip(&target.clip_id)?;
        Some(f(state, clip, target))
    }
}

impl VoxtuneHost for StudioHost {
    fn transport(&self, cx: &App) -> VoxTransport {
        self.with_clip(cx, |state, clip, target| {
            let map = clip_map(state, clip)?;
            let rel = state.transport.playhead_beats as f64 - map.clip_start;
            let frame = map.source_at(rel) + target.offset_frames as f64;
            Some(VoxTransport {
                playing: state.transport.playing,
                position: frame / target.rate.max(1) as f64,
            })
        })
        .flatten()
        .unwrap_or_default()
    }

    fn controls_transport(&self) -> bool {
        true
    }

    fn set_playing(&mut self, playing: bool, cx: &mut App) {
        let cb = self
            .callbacks
            .borrow()
            .as_ref()
            .map(|c| c.set_playing.clone());
        if let Some(cb) = cb {
            cb(playing, cx);
        }
    }

    fn seek(&mut self, seconds: f64, cx: &mut App) {
        let beat = self.with_clip(cx, |state, clip, target| {
            let map = clip_map(state, clip)?;
            let frame = seconds * target.rate as f64 - target.offset_frames as f64;
            Some((map.clip_start + map.beat_at(frame)) as f32)
        });
        if let Some(Some(beat)) = beat {
            self.timeline.update(cx, |timeline, cx| {
                timeline.seek_to_exact_beat(beat, crate::layout::SeekReason::TimelineClick, cx);
            });
        }
    }

    fn can_audition(&self) -> bool {
        false
    }

    fn document_changed(&mut self, doc: &VoxDocument, _cx: &mut App) {
        // Kept per original take, so edits survive closing the tab before
        // they are applied.
        if let Some(target) = self.target.borrow().as_ref() {
            let _ = self.store.write_document(&target.original, doc);
        }
    }

    fn analysis_ready(&mut self, result: &CantoAnalysisResult, _cx: &mut App) {
        if let Some(target) = self.target.borrow().as_ref() {
            if let Ok(json) = serde_json::to_string(result) {
                let _ = self.store.write_analysis(&target.original, &json);
            }
        }
    }
}

pub struct VoxtunePanel {
    timeline: Entity<Timeline>,
    editor: Entity<VoxtuneEditor>,
    target: Rc<RefCell<Option<Target>>>,
    callbacks: Rc<RefCell<Option<VoxtunePanelCallbacks>>>,
    store: Rc<TakeStore>,
    /// Render in flight.
    applying: bool,
    status: Option<(String, bool)>,
    _observer: Subscription,
}

impl VoxtunePanel {
    pub fn new(timeline: Entity<Timeline>, cx: &mut Context<Self>) -> Self {
        let target = Rc::new(RefCell::new(None));
        let callbacks = Rc::new(RefCell::new(None));
        let store = Rc::new(TakeStore::new(
            crate::paths::FutureboardPaths::resolve()
                .app_cache
                .join("Voxtune"),
        ));
        let host = StudioHost {
            timeline: timeline.clone(),
            target: target.clone(),
            callbacks: callbacks.clone(),
            store: store.clone(),
        };
        let shared = Arc::new(Shared::new(CantoEngine::new(48_000.0)));
        let editor = cx.new(|cx| {
            let mut editor = VoxtuneEditor::new(shared, Box::new(host), cx);
            editor.set_empty_hint("Select an audio clip to tune its vocal.", cx);
            editor
        });
        let _observer = cx.observe(&editor, |_, _, cx| cx.notify());
        Self {
            timeline,
            editor,
            target,
            callbacks,
            store,
            applying: false,
            status: None,
            _observer,
        }
    }

    pub fn set_callbacks(&mut self, callbacks: VoxtunePanelCallbacks) {
        *self.callbacks.borrow_mut() = Some(callbacks);
    }

    /// Follows the selection: loads a newly selected clip's take, or only
    /// re-targets when the clip now plays another version of the same take
    /// (after Apply, or Studio undo of it).
    fn sync_to_selection(&mut self, cx: &mut Context<Self>) {
        let selected = {
            let state = &self.timeline.read(cx).state;
            state
                .selection
                .selected_clip_ids
                .first()
                .and_then(|id| state.find_clip(id))
                .and_then(|(_, clip)| {
                    let source = clip_source(clip)?;
                    Some((clip.id.clone(), source, clip.stretch.original_sample_rate))
                })
        };
        let Some((clip_id, source, clip_rate)) = selected else {
            return;
        };
        let current = self.target.borrow().clone();
        if current
            .as_ref()
            .is_some_and(|t| t.clip_id == clip_id && t.source == source)
        {
            return;
        }
        let sidecar = VoxtuneSidecar::read(&source);
        let (original, offset_frames, rate, applied) = match &sidecar {
            Some(s) if s.original.exists() => (
                s.original.clone(),
                s.offset_frames,
                s.sample_rate,
                Some(s.document.clone()),
            ),
            _ => (source.clone(), 0, clip_rate.max(8_000), None),
        };
        let same_take = current
            .as_ref()
            .is_some_and(|t| t.original == original && t.rate == rate);
        *self.target.borrow_mut() = Some(Target {
            clip_id,
            source,
            original: original.clone(),
            offset_frames,
            rate,
            applied: applied.clone(),
        });
        self.status = None;
        if same_take && (self.editor.read(cx).has_audio() || self.editor.read(cx).is_loading()) {
            cx.notify();
            return;
        }
        // A new take: an engine at its rate, analysis from the cache when
        // the file hasn't changed, and the edits last made to it.
        let shared = Arc::new(Shared::new(CantoEngine::new(rate as f32)));
        let cached = self.store.read_analysis(&original);
        let saved = self.store.read_document(&original).or(applied);
        let job = LoadJob::start(
            shared.clone(),
            JobKind::File {
                path: original.clone(),
                cached,
            },
        );
        let title = original
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.editor.update(cx, |editor, cx| {
            editor.set_engine(shared, cx);
            editor.start_load(job, title, saved, cx);
        });
    }

    fn dirty(&self, cx: &App) -> bool {
        let editor = self.editor.read(cx);
        if !editor.has_audio() {
            return false;
        }
        match self.target.borrow().as_ref() {
            Some(t) => match &t.applied {
                Some(applied) => applied != editor.document(),
                None => editor.session().can_undo() || editor.document().correct > 0.0,
            },
            None => false,
        }
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        let Some(callbacks) = self.callbacks.borrow().clone() else {
            return;
        };
        let Some(target) = self.target.borrow().clone() else {
            return;
        };
        let window = {
            let state = &self.timeline.read(cx).state;
            state
                .find_clip(&target.clip_id)
                .and_then(|(_, clip)| clip_map(state, clip))
                .map(|m| m.window)
        };
        let Some((s0, s1)) = window else {
            self.status = Some(("The clip is not ready yet.".into(), true));
            cx.notify();
            return;
        };
        let editor = self.editor.read(cx);
        let doc = editor.document().clone();
        // Render from a copy: the editor stays usable meanwhile.
        let engine = editor.session().shared().lock().clone();
        let project_folder = (callbacks.project_folder)(cx);
        self.applying = true;
        self.status = Some(("Rendering…".into(), false));
        cx.notify();

        let panel = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let job_target = target.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let t = job_target;
                    let rate = engine.sample_rate() as u32;
                    let pcm = engine.render_offline();
                    // The clip's window, located in the original.
                    let a = (s0 + t.offset_frames) as usize;
                    let b = ((s1 + t.offset_frames) as usize).min(pcm.len());
                    if a >= b {
                        return Err("The clip lies outside the analysed take.".to_string());
                    }
                    let slice = &pcm[a..b];
                    let dir =
                        crate::audio_edit::edit_output_dir(project_folder.as_deref(), &t.original);
                    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                    let out = crate::audio_edit::next_version_path(&dir, &t.original);
                    SphereAudioProcessor::write_wav_f32(&out, slice, 1, rate)
                        .map_err(|e| format!("{e:?}"))?;
                    VoxtuneSidecar {
                        original: t.original.clone(),
                        offset_frames: a as u64,
                        sample_rate: rate,
                        document: doc,
                    }
                    .write(&out)
                    .map_err(|e| e.to_string())?;
                    Ok(NewSource {
                        path: out,
                        sample_rate: rate,
                        frames: slice.len() as u64,
                        old_window: (s0, s1),
                        old_sample_rate: t.rate,
                        start_shift_beats: 0.0,
                    })
                })
                .await;
            let _ = panel.update(cx, |this, cx| {
                this.applying = false;
                match result {
                    Ok(source) => {
                        // The clip may have moved on to another file while
                        // rendering; then the render no longer fits it.
                        let still = this
                            .timeline
                            .read(cx)
                            .state
                            .find_clip(&target.clip_id)
                            .and_then(|(_, clip)| clip_source(clip))
                            == Some(target.source.clone());
                        if still {
                            let name = source
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            (callbacks.replace_source)(target.clip_id.clone(), source, cx);
                            this.status = Some((format!("Applied as {name}"), false));
                        } else {
                            this.status = Some((
                                "The clip changed while rendering; apply again.".into(),
                                true,
                            ));
                        }
                    }
                    Err(e) => this.status = Some((format!("Apply failed: {e}"), true)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for VoxtunePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_to_selection(cx);
        let has_audio = self.editor.read(cx).has_audio();
        let dirty = self.dirty(cx);
        let applied = self
            .target
            .borrow()
            .as_ref()
            .is_some_and(|t| t.applied.is_some());
        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .h(px(32.0))
            .px(px(space::BASE))
            .bg(Colors::surface_titlebar())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(fb_button(
                "voxtune-apply",
                if self.applying {
                    "Rendering…"
                } else {
                    "Apply to clip"
                },
                FbButtonKind::Primary,
                has_audio && dirty && !self.applying,
                cx.listener(|this, _, _, cx| this.apply(cx)),
            ))
            .when(has_audio && dirty && !self.applying, |d| {
                d.child(fb_badge("Not applied", Colors::status_warning()))
            })
            .when(has_audio && !dirty && applied, |d| {
                d.child(fb_badge("Applied", Colors::status_success()))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(typography::UI_XS))
                    .text_color(match &self.status {
                        Some((_, true)) => Colors::status_warning(),
                        _ => Colors::text_muted(),
                    })
                    .child(match &self.status {
                        Some((text, _)) => text.clone(),
                        None if has_audio => {
                            "Edits play in the arrangement after Apply. Note preview is off inside Studio."
                                .to_string()
                        }
                        None => String::new(),
                    }),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .child(bar)
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}
