//! Audio Editor: destructive, sample-accurate editing of one audio clip.
//!
//! The editor shows the selected clip's source as it plays — each channel in
//! its own lane, on the arrangement's own bar/beat axis — and edits the audio
//! itself. Every edit renders a new version of the file through
//! [`crate::audio_edit`] and points the clip at it as one undo step, so the
//! original is never touched and Undo simply returns to the previous file.
//!
//! Ownership:
//!
//! * The project (clip, selection, playhead, undo) lives in the [`Timeline`];
//!   the editor reads it every render and writes it only through the
//!   timeline's gesture/command paths.
//! * The editor owns its view (scroll, zoom, selection, cursor, mode) and the
//!   decoded PCM + peak pyramid of the clip it shows, decoded off the UI
//!   thread and kept for the last few versions so Undo/Redo redraw instantly.
//! * Anything that needs the Studio (swapping a clip's source, opening a tool
//!   window, playing a range) goes out through [`AudioEditorCallbacks`].
//!
//! One coordinate model: [`ClipMap`] converts clip-relative beats to source
//! frames for drawing, hit-testing and edit ranges alike (see `geometry.rs`).

mod geometry;
mod paint;
mod peaks;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, AnyElement, App, AppContext, Bounds, ClickEvent, Context, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, Role, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use sphere_audio_editor::{
    AudioRangeSelection, AudioToolKind, AudioToolTarget, ClipEnvelope, EnvelopeCurve, EnvelopePoint,
};

pub use geometry::{ClipMap, Viewport};
use paint::{GridKind, GridLine, LaneFrame, Palette, FADE_HANDLE, HANDLE_BAND};
use peaks::PeakPyramid;

use crate::audio_edit::{self, apply_edit, clipboard, EditOp, NewSource, Pcm};
use crate::components::controls::{fb_segment, fb_segmented_track, fb_tooltip, FbSegment};
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::{
    beats_per_bar_from_sig, AudioClipStretchState, AudioImportState, ClipEdge, ClipState, ClipType,
    StretchMode, TimeSignatureMap, TimelineState, TrackState, WarpMarker,
};
use crate::components::timeline::waveform_cache;
use crate::theme::{radius, size, space, typography, Colors};

const OVERVIEW_H: f32 = 28.0;
const RULER_H: f32 = 22.0;
/// Pointer travel before a press on the lanes becomes a range drag.
const DRAG_THRESHOLD: f32 = 3.0;
/// Reach of a clip edge or selection edge, in pixels.
const EDGE_GRAB: f32 = 5.0;
/// Reach of an envelope point or warp marker, in pixels.
const POINT_GRAB: f32 = 7.0;
/// Peak level Normalize raises the selection to.
const NORMALIZE_TARGET_DB: f32 = -0.1;
/// Deepest zoom: pixels per source frame.
const MAX_PIXELS_PER_FRAME: f64 = 24.0;
/// Decoded versions kept for instant Undo/Redo.
const AUDIO_CACHE: usize = 3;

/// Tools that still open in their own window, listed in the Tools menu.
const TOOL_MENU: &[AudioToolKind] = &[
    AudioToolKind::Normalize,
    AudioToolKind::Loudness,
    AudioToolKind::SpectrumAnalyzer,
    AudioToolKind::PhaseAnalyzer,
    AudioToolKind::TransientDetector,
    AudioToolKind::BpmAnalysis,
    AudioToolKind::KeyAnalysis,
    AudioToolKind::TimePitch,
    AudioToolKind::Resample,
    AudioToolKind::ChannelTools,
    AudioToolKind::DcOffset,
    AudioToolKind::AudioRepair,
    AudioToolKind::SpectralProcessor,
];

/// Decoded source of one clip version.
pub(crate) struct EditorAudio {
    pub path: String,
    pub pcm: Pcm,
    pub peaks: PeakPyramid,
}

impl EditorAudio {
    fn new(path: String, pcm: Pcm) -> Self {
        let peaks = PeakPyramid::build(&pcm.samples, pcm.channels);
        Self { path, pcm, peaks }
    }
}

/// How the editor reaches the Studio.
#[derive(Clone)]
pub struct AudioEditorCallbacks {
    pub project_folder: Rc<dyn Fn(&App) -> Option<PathBuf>>,
    /// Point a clip at a rendered version of its audio (one undo step).
    pub replace_source: Rc<dyn Fn(String, NewSource, &mut App)>,
    pub open_tool: Rc<dyn Fn(AudioToolKind, AudioToolTarget, &mut App)>,
    /// Play absolute beats `[start, end)`, then stop.
    pub play_range: Rc<dyn Fn(f64, f64, &mut App)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    /// Select ranges and edit the audio.
    Select,
    /// Draw the clip's gain envelope.
    Gain,
    /// Place and drag warp markers.
    Warp,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// Pressed on the lanes; becomes a range once the pointer travels.
    Pending {
        x: f32,
        rel: f64,
    },
    Select {
        anchor: f64,
    },
    Fade {
        out: bool,
    },
    /// `anchor` is the absolute beat at the left edge when the drag began, so
    /// the view stays put while the clip start moves under it.
    Trim {
        left: bool,
        anchor: f64,
    },
    Envelope {
        id: u64,
    },
    Warp {
        id: u64,
    },
    Overview,
}

/// What happens to the selection once an edit lands.
#[derive(Debug, Clone, Copy)]
enum AfterEdit {
    Keep,
    CollapseTo(f64),
}

/// The clip being edited, as the project has it right now.
#[derive(Clone)]
struct ClipView {
    id: String,
    name: String,
    asset_key: Option<String>,
    path: Option<String>,
    file_name: String,
    track_color: gpui::Rgba,
    abs_start: f64,
    duration: f64,
    map: Option<ClipMap>,
    total_frames: u64,
    sample_rate: u32,
    channels: usize,
    reverse: bool,
    fade_in_end: f64,
    fade_out_start: f64,
    envelope: ClipEnvelope,
    warp: Vec<(u64, f64, bool)>,
    import_label: Option<String>,
}

/// The playhead line, repainted on transport ticks without re-rendering the
/// editor (see [`AudioEditorHost::publish_playhead`]).
pub struct EditorPlayhead {
    x: Rc<Cell<Option<f32>>>,
}

impl Render for EditorPlayhead {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.x.get() {
            Some(x) => div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(x - 0.5))
                .w(px(1.0))
                .bg(Colors::timeline_playhead()),
            None => div(),
        }
    }
}

pub struct AudioEditorHost {
    timeline: Entity<Timeline>,
    callbacks: Option<AudioEditorCallbacks>,
    focus: FocusHandle,
    /// Focus as of the last render; how the Studio decides whether Edit menu
    /// commands and shortcuts belong to the editor.
    focused: Rc<Cell<bool>>,
    clip_id: Option<String>,
    last_selected: Option<String>,
    cache: Vec<Arc<EditorAudio>>,
    loading: Option<String>,
    failed: Option<(String, String)>,
    view: Viewport,
    fitted: Option<String>,
    selection: Option<(f64, f64)>,
    cursor: f64,
    mode: EditMode,
    drag: Drag,
    gesture_clip: Option<String>,
    busy: Option<&'static str>,
    status: Option<(String, bool)>,
    amp_zoom: f32,
    tools_menu_open: bool,
    lanes_bounds: Rc<Cell<Bounds<Pixels>>>,
    overview_bounds: Rc<Cell<Bounds<Pixels>>>,
    playhead: Entity<EditorPlayhead>,
    playhead_x: Rc<Cell<Option<f32>>>,
    _timeline_observer: Subscription,
}

pub fn selected_audio_clip(state: &TimelineState) -> Option<(&TrackState, &ClipState)> {
    // The first *audio* clip of the selection, so selecting a MIDI clip along
    // with an audio clip still leaves the editor on real audio.
    state
        .selection
        .selected_clip_ids
        .iter()
        .filter_map(|clip_id| state.find_clip(clip_id))
        .find(|(_, clip)| matches!(clip.clip_type, ClipType::Audio { .. }))
}

pub fn clip_type_hint_for_selection(
    state: &TimelineState,
) -> Option<sphere_audio_editor::ClipTypeHint> {
    let clip_id = state.selection.selected_clip_ids.first()?;
    let (_, clip) = state.find_clip(clip_id)?;
    match clip.clip_type {
        ClipType::Audio { .. } => Some(sphere_audio_editor::ClipTypeHint::Audio),
        ClipType::Midi { .. } => Some(sphere_audio_editor::ClipTypeHint::Midi),
        // The audio editor has nothing to show for a reference video clip.
        ClipType::Video { .. } => None,
    }
}

impl AudioEditorHost {
    pub fn new(timeline: Entity<Timeline>, cx: &mut Context<Self>) -> Self {
        let _timeline_observer = cx.observe(&timeline, |_, _, cx| cx.notify());
        let playhead_x = Rc::new(Cell::new(None));
        let playhead = {
            let x = playhead_x.clone();
            cx.new(|_| EditorPlayhead { x })
        };
        Self {
            timeline,
            callbacks: None,
            focus: cx.focus_handle(),
            focused: Rc::new(Cell::new(false)),
            clip_id: None,
            last_selected: None,
            cache: Vec::new(),
            loading: None,
            failed: None,
            view: Viewport {
                scroll: 0.0,
                pixels_per_beat: 40.0,
                width: 0.0,
            },
            fitted: None,
            selection: None,
            cursor: 0.0,
            mode: EditMode::Select,
            drag: Drag::None,
            gesture_clip: None,
            busy: None,
            status: None,
            amp_zoom: 1.0,
            tools_menu_open: false,
            lanes_bounds: Rc::new(Cell::new(Bounds::default())),
            overview_bounds: Rc::new(Cell::new(Bounds::default())),
            playhead,
            playhead_x,
            _timeline_observer,
        }
    }

    pub fn set_callbacks(&mut self, callbacks: AudioEditorCallbacks) {
        self.callbacks = Some(callbacks);
    }

    /// Whether Edit commands (cut/copy/paste/delete/select-all) belong to the
    /// editor: it held keyboard focus when it last drew.
    pub fn owns_edit_commands(&self) -> bool {
        self.focused.get() && self.clip_id.is_some()
    }

    /// Run a Studio Edit command on the editor's selection. Returns whether
    /// the editor handled it.
    pub fn run_command(&mut self, command_id: &str, cx: &mut Context<Self>) -> bool {
        match command_id {
            "edit:select-all" => self.select_all(cx),
            "edit:deselect-all" => {
                self.selection = None;
                cx.notify();
            }
            "edit:copy" => {
                self.copy_selection(cx);
            }
            "edit:cut" => {
                if self.busy.is_none() && self.copy_selection(cx) {
                    self.run_edit(EditOp::Delete, "Cut", cx);
                }
            }
            "edit:paste" => self.paste(cx),
            "edit:delete" | "edit:delete-backspace" | "clip:delete" | "clip:erase" => {
                self.run_edit(EditOp::Delete, "Delete", cx)
            }
            _ => return false,
        }
        true
    }

    /// Redraw after the Studio changed the clip. Decoded audio stays cached:
    /// edits always write a new file, so no cached version can go stale.
    pub(crate) fn refresh_clip_visuals(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Called by the panel when it shows something else in the editor's
    /// place: the editor is off screen, so it no longer owns Edit commands.
    pub fn mark_hidden(&self) {
        self.focused.set(false);
    }

    /// The selection as a tool window target.
    pub fn current_tool_target(&self, cx: &App) -> Option<AudioToolTarget> {
        let view = self.clip_view(cx)?;
        let time_selection = match (self.selection, view.map.as_ref()) {
            (Some((a, b)), Some(map)) if (a - b).abs() > 1.0e-9 => {
                let (start, end) = map.source_range(a, b);
                Some(AudioRangeSelection::new(start as i64, end as i64))
            }
            _ => None,
        };
        Some(AudioToolTarget {
            source_id: view.asset_key.clone().unwrap_or_else(|| view.id.clone()),
            clip_id: view.id,
            clip_name: view.name,
            file_label: view.file_name,
            source_path: view.path,
            sample_rate: view.sample_rate.max(1),
            channels: view.channels.max(1) as u16,
            source_frames: view.total_frames as i64,
            time_selection,
            spectral_selection: None,
        })
    }

    /// Move the playhead line on a transport tick. Repaints only the line
    /// (and scrolls the view when playback runs off its right edge).
    pub fn publish_playhead(host: &Entity<Self>, cx: &mut App) {
        let _ = host.update(cx, |this, cx| this.sync_playhead(cx));
    }

    fn sync_playhead(&mut self, cx: &mut Context<Self>) {
        let Some((rel, duration, playing)) = self.playhead_rel(cx) else {
            return;
        };
        let inside = (0.0..=duration).contains(&rel);
        if playing && inside && self.drag == Drag::None && self.view.width > 1.0 {
            let x = self.view.x_at(rel);
            if x > self.view.width || x < 0.0 {
                // Page forward, keeping a little context behind the line.
                self.view.scroll = rel - self.view.visible_beats() * 0.05;
                self.view.clamp(duration);
                cx.notify();
            }
        }
        if self.place_playhead(rel, duration) {
            self.playhead.update(cx, |_, cx| cx.notify());
        }
    }

    /// Playhead relative to the clip, the clip length and whether the
    /// transport runs. Cheap: a transport tick calls this at display rate.
    fn playhead_rel(&self, cx: &App) -> Option<(f64, f64, bool)> {
        let state = &self.timeline.read(cx).state;
        let (_, clip) = state.find_clip(self.clip_id.as_deref()?)?;
        let rel = state.transport.playhead_beats as f64 - clip.start_beat.max(0.0) as f64;
        Some((
            rel,
            clip.duration_beats.max(0.0) as f64,
            state.transport.playing,
        ))
    }

    /// Put the playhead line at `rel`; `true` when it moved on screen.
    fn place_playhead(&mut self, rel: f64, duration: f64) -> bool {
        let width = self.view.width as f32;
        let next = (0.0..=duration)
            .contains(&rel)
            .then(|| self.view.x_at(rel) as f32)
            .filter(|x| (0.0..=width).contains(x));
        let changed = match (self.playhead_x.get(), next) {
            (Some(a), Some(b)) => (a - b).abs() >= 0.5,
            (None, None) => false,
            _ => true,
        };
        self.playhead_x.set(next);
        changed
    }

    // ── Project reads ──────────────────────────────────────────────────────

    fn audio_for(&self, path: Option<&str>) -> Option<Arc<EditorAudio>> {
        let path = path?;
        self.cache.iter().find(|audio| audio.path == path).cloned()
    }

    fn remember(&mut self, audio: Arc<EditorAudio>) {
        self.cache.retain(|cached| cached.path != audio.path);
        self.cache.insert(0, audio);
        self.cache.truncate(AUDIO_CACHE);
    }

    fn clip_view(&self, cx: &App) -> Option<ClipView> {
        let id = self.clip_id.as_deref()?;
        let state = &self.timeline.read(cx).state;
        let (track, clip) = state.find_clip(id)?;
        let ClipType::Audio { source_path, .. } = &clip.clip_type else {
            return None;
        };
        let path = source_path.clone();
        let audio = self.audio_for(path.as_deref());
        let asset_key = clip.audio_asset_key().map(str::to_string);
        let meta = asset_key.as_deref().and_then(waveform_cache::get_file_meta);
        let (total_frames, sample_rate, channels) = match (&audio, &meta) {
            (Some(audio), _) => (
                audio.pcm.frames() as u64,
                audio.pcm.sample_rate,
                audio.pcm.channels,
            ),
            (None, Some(meta)) => (meta.total_frames, meta.sample_rate, meta.channels as usize),
            (None, None) => (
                clip.stretch.original_duration_samples,
                clip.stretch.original_sample_rate,
                2,
            ),
        };
        let bpm = state.bpm.max(1.0) as f64;
        let map = ClipMap::build(clip, total_frames, bpm, |beat| state.seconds_at_beat(beat));
        let abs_start = clip.start_beat.max(0.0) as f64;
        let duration = clip.duration_beats.max(0.0) as f64;
        let start_seconds = state.seconds_at_beat(abs_start);
        let end_seconds = state.seconds_at_beat(abs_start + duration);
        let fade_in_end = (state
            .beat_at_seconds(start_seconds + clip.stretch.fade_in_ms.max(0.0) as f64 / 1000.0)
            - abs_start)
            .clamp(0.0, duration);
        let fade_out_start = (state
            .beat_at_seconds(end_seconds - clip.stretch.fade_out_ms.max(0.0) as f64 / 1000.0)
            - abs_start)
            .clamp(0.0, duration);
        let import_label = match &clip.audio_import {
            AudioImportState::Ready => None,
            AudioImportState::Failed { message } => Some(message.clone()),
            _ => Some("Importing…".to_string()),
        };
        let file_name = path
            .as_deref()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(p)
                    .to_string()
            })
            .unwrap_or_else(|| "No source file".to_string());
        Some(ClipView {
            id: clip.id.clone(),
            name: clip.name.clone(),
            asset_key,
            path,
            file_name,
            track_color: track.color,
            abs_start,
            duration,
            map,
            total_frames,
            sample_rate,
            channels: channels.max(1),
            reverse: clip.stretch.reverse,
            fade_in_end,
            fade_out_start,
            envelope: clip.stretch.gain_envelope.clone(),
            warp: clip
                .stretch
                .warp_markers
                .iter()
                .map(|m| (m.id, m.timeline_beat - abs_start, m.locked))
                .collect(),
            import_label,
        })
    }

    /// Follow the arrangement's selection. A switch is a gesture boundary:
    /// a live preview on the old clip is rolled back, not left half-applied.
    fn sync_clip(&mut self, cx: &mut Context<Self>) {
        let selected =
            selected_audio_clip(&self.timeline.read(cx).state).map(|(_, c)| c.id.clone());
        if selected != self.last_selected {
            if self.drag != Drag::None {
                self.cancel_gesture(cx);
                self.drag = Drag::None;
            }
            self.last_selected = selected.clone();
            if self.clip_id != selected {
                self.clip_id = selected;
                self.selection = None;
                self.cursor = 0.0;
                self.fitted = None;
                self.status = None;
                self.tools_menu_open = false;
            }
        }
    }

    fn ensure_audio(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.audio_for(Some(path)).is_some()
            || self.loading.as_deref() == Some(path)
            || self.failed.as_ref().is_some_and(|(p, _)| p == path)
        {
            return;
        }
        self.loading = Some(path.to_string());
        let path = path.to_string();
        let host = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let load_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let buffer = DirectAudio::load_audio_file_for_edit(&load_path)?;
                    let pcm = Pcm {
                        sample_rate: buffer.sample_rate,
                        channels: buffer.channels.max(1),
                        samples: buffer.samples,
                    };
                    Ok::<_, String>(Arc::new(EditorAudio::new(load_path, pcm)))
                })
                .await;
            let _ = host.update(cx, |this, cx| {
                if this.loading.as_deref() != Some(path.as_str()) {
                    return;
                }
                this.loading = None;
                match result {
                    Ok(audio) => this.remember(audio),
                    Err(error) => this.failed = Some((path, error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_status(&mut self, message: impl Into<String>, error: bool) {
        self.status = Some((message.into(), error));
    }

    // ── Gestures on clip settings (one undo step each) ─────────────────────

    fn begin_gesture(&mut self, clip_id: &str, cx: &mut Context<Self>) {
        self.gesture_clip = Some(clip_id.to_string());
        let clip_id = clip_id.to_string();
        let _ = self.timeline.update(cx, |timeline, _| {
            timeline.begin_inspector_clip_gesture(&clip_id);
        });
    }

    fn commit_gesture(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.gesture_clip.take() else {
            return;
        };
        let _ = self.timeline.update(cx, |timeline, cx| {
            if timeline.commit_inspector_clip_gesture(&clip_id, cx) {
                timeline.mark_media_changed(cx);
            }
            cx.notify();
        });
    }

    fn cancel_gesture(&mut self, cx: &mut Context<Self>) {
        if self.gesture_clip.take().is_none() {
            return;
        }
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.cancel_inspector_clip_gesture(cx);
        });
    }

    /// Live-edit the clip's stretch state inside an open gesture.
    fn edit_stretch(
        &mut self,
        clip_id: &str,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut AudioClipStretchState),
    ) {
        let clip_id = clip_id.to_string();
        let _ = self.timeline.update(cx, |timeline, cx| {
            let Some(mut stretch) = timeline.state.clip_stretch(&clip_id).cloned() else {
                return;
            };
            edit(&mut stretch);
            stretch.dirty = true;
            if timeline.state.set_clip_stretch(&clip_id, stretch) {
                cx.notify();
            }
        });
    }

    fn seek(&mut self, abs_beat: f64, cx: &mut Context<Self>) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.seek_to_exact_beat(
                abs_beat as f32,
                crate::layout::SeekReason::TimelineClick,
                cx,
            );
        });
    }

    /// Clip-relative beat under `x`, snapped to the project grid unless
    /// `bypass`, clamped to the clip.
    fn beat_under(&self, view: &ClipView, x: f32, bypass: bool, cx: &App) -> f64 {
        let rel = self.view.beat_at(x as f64).clamp(0.0, view.duration);
        if bypass {
            return rel;
        }
        let abs = view.abs_start + rel;
        let snapped = self
            .timeline
            .read(cx)
            .state
            .snap_beats_with_bypass(abs as f32, false) as f64;
        (snapped - view.abs_start).clamp(0.0, view.duration)
    }

    fn max_pixels_per_beat(view: &ClipView) -> f64 {
        view.map
            .as_ref()
            .map(|map| map.mean_frames_per_beat() * MAX_PIXELS_PER_FRAME)
            .unwrap_or(4_000.0)
    }

    // ── Pointer ────────────────────────────────────────────────────────────

    fn local(bounds: Bounds<Pixels>, position: Point<Pixels>) -> (f32, f32) {
        (
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        )
    }

    fn lanes_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.tools_menu_open = false;
        if self.busy.is_some() {
            return;
        }
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let bounds = self.lanes_bounds.get();
        let (x, y) = Self::local(bounds, event.position);
        let height = f32::from(bounds.size.height);
        let bypass = event.modifiers.alt;
        let rel = self.beat_under(&view, x, bypass, cx);
        let start_x = self.view.x_at(0.0) as f32;
        let end_x = self.view.x_at(view.duration) as f32;

        match self.mode {
            EditMode::Select => {
                if y < HANDLE_BAND {
                    // Trim from just outside an edge, fade from just inside.
                    let fade_in_x = self.view.x_at(view.fade_in_end) as f32;
                    let fade_out_x = self.view.x_at(view.fade_out_start) as f32;
                    let drag = if x <= start_x && x >= start_x - EDGE_GRAB {
                        Some(Drag::Trim {
                            left: true,
                            anchor: view.abs_start + self.view.scroll,
                        })
                    } else if x >= end_x && x <= end_x + EDGE_GRAB {
                        Some(Drag::Trim {
                            left: false,
                            anchor: view.abs_start + self.view.scroll,
                        })
                    } else if (x - fade_in_x).abs() <= FADE_HANDLE {
                        Some(Drag::Fade { out: false })
                    } else if (x - fade_out_x).abs() <= FADE_HANDLE {
                        Some(Drag::Fade { out: true })
                    } else {
                        None
                    };
                    if let Some(drag) = drag {
                        self.begin_gesture(&view.id, cx);
                        self.drag = drag;
                        cx.notify();
                        return;
                    }
                }
                if event.click_count >= 2 {
                    self.select_all(cx);
                    return;
                }
                if event.modifiers.shift {
                    let anchor = match self.selection {
                        Some((a, b)) => {
                            if (rel - a).abs() > (rel - b).abs() {
                                a
                            } else {
                                b
                            }
                        }
                        None => self.cursor,
                    };
                    self.selection = Some((anchor, rel));
                    self.drag = Drag::Select { anchor };
                    cx.notify();
                    return;
                }
                if let Some((a, b)) = self.selection {
                    let (lo, hi) = (a.min(b), a.max(b));
                    if (self.view.x_at(lo) as f32 - x).abs() <= EDGE_GRAB {
                        self.drag = Drag::Select { anchor: hi };
                        return;
                    }
                    if (self.view.x_at(hi) as f32 - x).abs() <= EDGE_GRAB {
                        self.drag = Drag::Select { anchor: lo };
                        return;
                    }
                }
                self.drag = Drag::Pending { x, rel };
                cx.notify();
            }
            EditMode::Gain => {
                let top = HANDLE_BAND;
                let lane_h = height - HANDLE_BAND;
                let hit = view.envelope.points.iter().find(|p| {
                    let px = self.view.x_at(p.time as f64 * view.duration) as f32;
                    let py = paint::envelope_y(p.value_db, top, lane_h);
                    (px - x).abs() <= POINT_GRAB && (py - y).abs() <= POINT_GRAB
                });
                self.begin_gesture(&view.id, cx);
                match hit.map(|p| p.id) {
                    Some(id) if event.modifiers.alt => {
                        self.edit_stretch(&view.id, cx, |s| {
                            s.gain_envelope.points.retain(|p| p.id != id);
                        });
                        self.commit_gesture(cx);
                    }
                    Some(id) => self.drag = Drag::Envelope { id },
                    None => {
                        let id = view.envelope.points.iter().map(|p| p.id).max().unwrap_or(0) + 1;
                        let time = (rel / view.duration.max(1.0e-9)) as f32;
                        let value_db = paint::envelope_db(y, top, lane_h);
                        self.edit_stretch(&view.id, cx, |s| {
                            s.gain_envelope.points.push(EnvelopePoint {
                                id,
                                time,
                                value_db,
                                curve: EnvelopeCurve::Linear,
                            });
                            s.gain_envelope.sanitize_in_place();
                        });
                        self.drag = Drag::Envelope { id };
                    }
                }
                cx.notify();
            }
            EditMode::Warp => {
                if view.reverse {
                    self.set_status("Warp markers are not available on a reversed clip", true);
                    cx.notify();
                    return;
                }
                let Some(map) = view.map.clone() else {
                    return;
                };
                let hit = view
                    .warp
                    .iter()
                    .find(|(_, marker_rel, _)| {
                        (self.view.x_at(*marker_rel) as f32 - x).abs() <= POINT_GRAB
                    })
                    .copied();
                self.begin_gesture(&view.id, cx);
                match hit {
                    Some((id, _, locked)) if event.modifiers.alt => {
                        if !locked {
                            self.edit_stretch(&view.id, cx, |s| {
                                s.warp_markers.retain(|m| m.id != id);
                            });
                        }
                        self.commit_gesture(cx);
                    }
                    Some((_, _, true)) => self.cancel_gesture(cx),
                    Some((id, _, false)) => self.drag = Drag::Warp { id },
                    None => {
                        let abs_start = view.abs_start;
                        let abs_end = view.abs_start + view.duration;
                        let (s0, s1) = map.window;
                        let source_sample = map.source_at(rel).round() as u64;
                        let mut new_id = 0;
                        self.edit_stretch(&view.id, cx, |s| {
                            let mut next_id =
                                s.warp_markers.iter().map(|m| m.id).max().unwrap_or(0);
                            let mut marker = |source_sample: u64, beat: f64, locked: bool| {
                                next_id += 1;
                                WarpMarker {
                                    id: next_id,
                                    source_sample,
                                    timeline_beat: beat,
                                    locked,
                                }
                            };
                            if s.warp_markers.is_empty() {
                                // Pin both ends, so one marker bends the audio
                                // around it instead of freezing it.
                                let start = marker(s0, abs_start, true);
                                let end = marker(s1, abs_end, true);
                                s.warp_markers.push(start);
                                s.warp_markers.push(end);
                            }
                            let placed = marker(source_sample, abs_start + rel, false);
                            new_id = placed.id;
                            s.warp_markers.push(placed);
                            s.warp_markers
                                .sort_by(|a, b| a.timeline_beat.total_cmp(&b.timeline_beat));
                            s.mode = StretchMode::Warp;
                        });
                        self.drag = Drag::Warp { id: new_id };
                    }
                }
                cx.notify();
            }
        }
    }

    fn pointer_move(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        if self.drag == Drag::None {
            return;
        }
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        if self.drag == Drag::Overview {
            self.scroll_overview_to(position, &view, cx);
            return;
        }
        let bounds = self.lanes_bounds.get();
        let (x, y) = Self::local(bounds, position);
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        let bypass = modifiers.alt;
        match self.drag {
            Drag::Pending { x: x0, rel: rel0 } => {
                if (x - x0).abs() >= DRAG_THRESHOLD {
                    self.drag = Drag::Select { anchor: rel0 };
                    let rel = self.beat_under(&view, x, bypass, cx);
                    self.selection = Some((rel0, rel));
                    cx.notify();
                }
            }
            Drag::Select { anchor } => {
                // Drag past an edge scrolls the view along.
                if x < 0.0 || x > width {
                    let over = if x < 0.0 { x } else { x - width };
                    self.view.scroll += over as f64 / self.view.pixels_per_beat * 0.25;
                    self.view.clamp(view.duration);
                }
                let rel = self.beat_under(&view, x, bypass, cx);
                self.selection = Some((anchor, rel));
                cx.notify();
            }
            Drag::Fade { out } => {
                let rel = self.beat_under(&view, x, bypass, cx);
                let (start_s, end_s, at_s) = {
                    let state = &self.timeline.read(cx).state;
                    (
                        state.seconds_at_beat(view.abs_start),
                        state.seconds_at_beat(view.abs_start + view.duration),
                        state.seconds_at_beat(view.abs_start + rel),
                    )
                };
                let length = (end_s - start_s).max(0.0);
                let ms =
                    if out { end_s - at_s } else { at_s - start_s }.clamp(0.0, length) * 1000.0;
                self.edit_stretch(&view.id, cx, |s| {
                    if out {
                        s.fade_out_ms = ms as f32;
                    } else {
                        s.fade_in_ms = ms as f32;
                    }
                });
            }
            Drag::Trim { left, anchor } => {
                let abs = anchor + x as f64 / self.view.pixels_per_beat;
                let clip_id = view.id.clone();
                let new_start = self.timeline.update(cx, |timeline, cx| {
                    let edge = if left {
                        ClipEdge::Left
                    } else {
                        ClipEdge::Right
                    };
                    let beat = timeline.state.snap_beats_with_bypass(abs as f32, bypass);
                    timeline
                        .state
                        .resize_clip_with_bypass(&clip_id, edge, beat, true);
                    cx.notify();
                    timeline
                        .state
                        .find_clip(&clip_id)
                        .map(|(_, clip)| clip.start_beat.max(0.0) as f64)
                });
                if let Some(start) = new_start {
                    // Keep the audio still on screen while the clip start moves.
                    self.view.scroll = anchor - start;
                }
            }
            Drag::Envelope { id } => {
                let rel = self.beat_under(&view, x, bypass, cx);
                let time = (rel / view.duration.max(1.0e-9)) as f32;
                let value_db = paint::envelope_db(y, HANDLE_BAND, height - HANDLE_BAND);
                self.edit_stretch(&view.id, cx, |s| {
                    if let Some(point) = s.gain_envelope.points.iter_mut().find(|p| p.id == id) {
                        point.time = time;
                        point.value_db = value_db;
                    }
                    s.gain_envelope.sanitize_in_place();
                });
            }
            Drag::Warp { id } => {
                let rel = self.beat_under(&view, x, bypass, cx);
                let target = view.abs_start + rel;
                self.edit_stretch(&view.id, cx, |s| {
                    let Some(index) = s.warp_markers.iter().position(|m| m.id == id) else {
                        return;
                    };
                    let lo = index
                        .checked_sub(1)
                        .map(|i| s.warp_markers[i].timeline_beat + 1.0e-3)
                        .unwrap_or(f64::MIN);
                    let hi = s
                        .warp_markers
                        .get(index + 1)
                        .map(|m| m.timeline_beat - 1.0e-3)
                        .unwrap_or(f64::MAX);
                    if hi > lo {
                        s.warp_markers[index].timeline_beat = target.clamp(lo, hi);
                    }
                });
            }
            Drag::None | Drag::Overview => {}
        }
    }

    fn pointer_up(&mut self, cx: &mut Context<Self>) {
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        match drag {
            Drag::Pending { rel, .. } => {
                self.selection = None;
                self.cursor = rel;
                if let Some(view) = self.clip_view(cx) {
                    self.seek(view.abs_start + rel, cx);
                }
            }
            Drag::Select { .. } => {
                if let Some((a, b)) = self.selection {
                    if ((a - b).abs() * self.view.pixels_per_beat) < 1.0 {
                        self.selection = None;
                        self.cursor = a;
                    } else {
                        self.cursor = a.min(b);
                    }
                }
            }
            Drag::Fade { .. } | Drag::Trim { .. } | Drag::Envelope { .. } | Drag::Warp { .. } => {
                self.commit_gesture(cx);
            }
            Drag::Overview | Drag::None => {}
        }
        cx.notify();
    }

    fn scroll_overview_to(
        &mut self,
        position: Point<Pixels>,
        view: &ClipView,
        cx: &mut Context<Self>,
    ) {
        let bounds = self.overview_bounds.get();
        let (x, _) = Self::local(bounds, position);
        let width = f32::from(bounds.size.width).max(1.0);
        let rel = (x / width) as f64 * view.duration;
        self.view.scroll = rel - self.view.visible_beats() * 0.5;
        self.view.clamp(view.duration);
        cx.notify();
    }

    fn overview_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        self.drag = Drag::Overview;
        self.scroll_overview_to(event.position, &view, cx);
    }

    fn ruler_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let (x, _) = Self::local(self.lanes_bounds.get(), event.position);
        let rel = self.beat_under(&view, x, event.modifiers.alt, cx);
        self.cursor = rel;
        self.seek(view.abs_start + rel, cx);
        cx.notify();
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let delta = event.delta.pixel_delta(px(20.0));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        let (anchor, _) = Self::local(self.lanes_bounds.get(), event.position);
        if event.modifiers.platform || event.modifiers.control {
            let factor = ((dy as f64) * 0.01).exp();
            self.view
                .zoom(factor, anchor as f64, Self::max_pixels_per_beat(&view));
        } else if event.modifiers.alt {
            self.amp_zoom = (self.amp_zoom * (dy * 0.01).exp()).clamp(1.0, 32.0);
        } else {
            let d = if dx.abs() > dy.abs() { dx } else { dy };
            self.view.scroll -= d as f64 / self.view.pixels_per_beat;
        }
        self.view.clamp(view.duration);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if m.control || m.platform || m.alt {
            return;
        }
        match key {
            "escape" => {
                if self.drag != Drag::None {
                    self.cancel_gesture(cx);
                    self.drag = Drag::None;
                } else if self.tools_menu_open {
                    self.tools_menu_open = false;
                } else {
                    self.selection = None;
                }
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    // ── Commands ───────────────────────────────────────────────────────────

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.clip_view(cx) {
            self.selection = Some((0.0, view.duration));
            cx.notify();
        }
    }

    fn zoom_by(&mut self, factor: f64, cx: &mut Context<Self>) {
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let anchor = match self.selection {
            Some((a, b)) => self.view.x_at((a + b) * 0.5),
            None => self.view.x_at(self.cursor),
        }
        .clamp(0.0, self.view.width);
        self.view
            .zoom(factor, anchor, Self::max_pixels_per_beat(&view));
        self.view.clamp(view.duration);
        cx.notify();
    }

    fn fit(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.clip_view(cx) {
            self.view.fit(view.duration);
            cx.notify();
        }
    }

    /// The selection (or the cursor) and the source frames under it.
    fn target_range(&self, view: &ClipView) -> Option<((f64, f64), (u64, u64))> {
        let map = view.map.as_ref()?;
        let (lo, hi) = match self.selection {
            Some((a, b)) => (a.min(b), a.max(b)),
            None => (self.cursor, self.cursor),
        };
        Some(((lo, hi), map.source_range(lo, hi)))
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(view) = self.clip_view(cx) else {
            return false;
        };
        let Some(audio) = self.audio_for(view.path.as_deref()) else {
            self.set_status("Audio is still loading", true);
            cx.notify();
            return false;
        };
        let Some((_, (a, b))) = self.target_range(&view).filter(|(_, (a, b))| b > a) else {
            self.set_status("Select some audio first", true);
            cx.notify();
            return false;
        };
        let mut pcm = audio.pcm.slice(a as usize, b as usize);
        if view.reverse {
            // Copy what is heard, not the file's order.
            audio_edit::reverse_frames(&mut pcm.samples, pcm.channels);
        }
        let seconds = pcm.frames() as f64 / pcm.sample_rate.max(1) as f64;
        clipboard::set(pcm);
        self.set_status(format!("Copied {seconds:.3} s"), false);
        cx.notify();
        true
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        match clipboard::get() {
            Some(pcm) if pcm.frames() > 0 => self.run_edit(EditOp::Paste(pcm), "Paste", cx),
            _ => {
                self.set_status("Nothing to paste — copy some audio first", true);
                cx.notify();
            }
        }
    }

    fn play_selection(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let Some(callbacks) = self.callbacks.clone() else {
            return;
        };
        let (lo, hi) = match self.selection {
            Some((a, b)) if (a - b).abs() > 1.0e-9 => (a.min(b), a.max(b)),
            _ => (self.cursor, view.duration),
        };
        if hi <= lo {
            return;
        }
        (callbacks.play_range)(view.abs_start + lo, view.abs_start + hi, cx);
    }

    fn split_at_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let beat = (view.abs_start + self.cursor) as f32;
        let clip_id = view.id.clone();
        let split = self.timeline.update(cx, |timeline, cx| {
            let split = timeline.split_audio_clip_at_beat(&clip_id, beat, cx);
            if split {
                timeline.mark_media_changed(cx);
            }
            split
        });
        if !split {
            self.set_status("Put the cursor inside the clip to split it", true);
            cx.notify();
        }
    }

    fn open_tool(&mut self, kind: AudioToolKind, cx: &mut Context<Self>) {
        self.tools_menu_open = false;
        let Some(callbacks) = self.callbacks.clone() else {
            return;
        };
        if let Some(target) = self.current_tool_target(cx) {
            (callbacks.open_tool)(kind, target, cx);
        }
        cx.notify();
    }

    /// Render `op` over the selection into a new version of the file and
    /// point the clip at it. Runs off the UI thread; the editor is read-only
    /// until the version lands.
    fn run_edit(&mut self, op: EditOp, label: &'static str, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(callbacks) = self.callbacks.clone() else {
            return;
        };
        let Some(view) = self.clip_view(cx) else {
            return;
        };
        let (Some(map), Some(path)) = (view.map.clone(), view.path.clone()) else {
            return;
        };
        let Some(audio) = self.audio_for(Some(&path)) else {
            self.set_status("Audio is still loading", true);
            cx.notify();
            return;
        };
        let Some(((lo, _hi), (a, b))) = self.target_range(&view) else {
            return;
        };
        if a == b && !op.accepts_empty_range() {
            self.set_status("Select some audio first", true);
            cx.notify();
            return;
        }
        let after = match op {
            EditOp::Delete | EditOp::Paste(_) => AfterEdit::CollapseTo(lo),
            EditOp::Crop => AfterEdit::CollapseTo(0.0),
            _ => AfterEdit::Keep,
        };
        let start_shift_beats = if matches!(op, EditOp::Crop) {
            lo as f32
        } else {
            0.0
        };
        let op = if map.reverse {
            op.for_reversed_playback()
        } else {
            op
        };
        let (s0, s1) = map.window;
        let project_folder = (callbacks.project_folder)(cx);
        let clip_id = view.id.clone();
        self.busy = Some(label);
        self.set_status(format!("{label}…"), false);
        cx.notify();

        let host = cx.entity().downgrade();
        let old_path = path.clone();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let pcm = &audio.pcm;
                    let channels = pcm.channels.max(1);
                    let rate = pcm.sample_rate;
                    let op = match op {
                        EditOp::Paste(clip) => {
                            EditOp::Paste(Arc::new(clip.conformed(channels, rate)?))
                        }
                        other => other,
                    };
                    let frames = pcm.frames() as u64;
                    let (s0, s1) = (s0.min(frames), s1.min(frames));
                    let window = &pcm.samples[s0 as usize * channels..s1 as usize * channels];
                    let edited = apply_edit(
                        window,
                        channels,
                        rate,
                        a.saturating_sub(s0) as usize,
                        b.saturating_sub(s0) as usize,
                        &op,
                    )?;
                    if edited.is_empty() {
                        return Err("the edit would leave no audio".to_string());
                    }
                    let source = Path::new(&path);
                    let dir = audio_edit::edit_output_dir(project_folder.as_deref(), source);
                    std::fs::create_dir_all(&dir)
                        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
                    let out = audio_edit::next_version_path(&dir, source);
                    SphereAudioProcessor::write_wav_f32(&out, &edited, channels as u16, rate)
                        .map_err(|e| e.to_string())?;
                    let new_frames = (edited.len() / channels) as u64;
                    let out_string = out.to_string_lossy().into_owned();
                    let decoded = EditorAudio::new(
                        out_string,
                        Pcm {
                            sample_rate: rate,
                            channels,
                            samples: edited,
                        },
                    );
                    Ok::<_, String>((
                        Arc::new(decoded),
                        NewSource {
                            path: out,
                            sample_rate: rate,
                            frames: new_frames,
                            old_window: (s0, s1),
                            old_sample_rate: rate,
                            start_shift_beats,
                        },
                    ))
                })
                .await;
            let _ = host.update(cx, |this, cx| {
                this.finish_edit(clip_id, old_path, label, after, result, cx);
            });
        })
        .detach();
    }

    fn finish_edit(
        &mut self,
        clip_id: String,
        old_path: String,
        label: &'static str,
        after: AfterEdit,
        result: Result<(Arc<EditorAudio>, NewSource), String>,
        cx: &mut Context<Self>,
    ) {
        self.busy = None;
        cx.notify();
        let (audio, source) = match result {
            Ok(done) => done,
            Err(error) => {
                self.set_status(format!("{label} failed: {error}"), true);
                return;
            }
        };
        // The clip may have been undone, moved to another file or deleted
        // while this was rendering; never apply an edit to audio it was not
        // made from.
        let still_current = self
            .timeline
            .read(cx)
            .state
            .find_clip(&clip_id)
            .and_then(|(_, clip)| match &clip.clip_type {
                ClipType::Audio { source_path, .. } => source_path.clone(),
                _ => None,
            })
            .is_some_and(|path| path == old_path);
        if !still_current {
            self.set_status(
                format!("{label} discarded — the clip changed meanwhile"),
                true,
            );
            return;
        }
        let Some(callbacks) = self.callbacks.clone() else {
            return;
        };
        let name = source
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        self.remember(audio);
        if let AfterEdit::CollapseTo(rel) = after {
            self.selection = None;
            self.cursor = rel;
        }
        self.set_status(format!("{label} → {name}"), false);
        (callbacks.replace_source)(clip_id, source, cx);
    }

    // ── View ───────────────────────────────────────────────────────────────

    fn palette(track_color: gpui::Rgba) -> Palette {
        Palette {
            lane_bg: Colors::timeline_background(),
            outside_bg: Colors::surface_canvas(),
            lane_divider: Colors::border_subtle(),
            center_line: Colors::timeline_grid_minor(),
            grid_bar: Colors::timeline_grid_bar(),
            grid_beat: Colors::timeline_grid_major(),
            grid_sub: Colors::timeline_grid_minor(),
            wave: Colors::with_alpha(track_color, 0.9),
            wave_dim: Colors::with_alpha(track_color, 0.55),
            selection: Colors::timeline_selection(),
            selection_edge: Colors::accent_primary(),
            cursor: Colors::accent_primary(),
            fade_shade: Colors::with_alpha(Colors::surface_canvas(), 0.55),
            fade_line: Colors::text_secondary(),
            handle: Colors::text_secondary(),
            envelope: Colors::state_automation(),
            envelope_point: Colors::state_automation(),
            warp: Colors::semantic_warning(),
            warp_locked: Colors::text_muted(),
            viewport_frame: Colors::text_secondary(),
            viewport_fill: Colors::with_alpha(Colors::text_primary(), 0.06),
        }
    }

    fn toolbar(&self, view: &ClipView, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.busy.is_none() && self.audio_for(view.path.as_deref()).is_some();
        let has_range = self.selection.is_some_and(|(a, b)| (a - b).abs() > 1.0e-9);
        let edit = ready && has_range;
        let can_paste = ready && clipboard::has_audio();

        let modes = [
            (EditMode::Select, "Select", FbSegment::First),
            (EditMode::Gain, "Gain", FbSegment::Middle),
            (EditMode::Warp, "Warp", FbSegment::Last),
        ];
        let mode_track =
            fb_segmented_track().children(modes.into_iter().map(|(mode, label, pos)| {
                fb_segment(
                    ("audio-editor-mode", mode as usize),
                    label,
                    self.mode == mode,
                    pos,
                    cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.focus.focus(window, cx);
                        if this.drag != Drag::None {
                            this.cancel_gesture(cx);
                            this.drag = Drag::None;
                        }
                        this.mode = mode;
                        cx.notify();
                    }),
                )
            }));

        let button = action_button;

        let group = || {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::HAIR))
                .flex_none()
        };
        let edits = group()
            .child(button(
                "ae-cut",
                "Cut",
                "Cut the selection (⌘X)",
                edit,
                |t, cx| {
                    if t.copy_selection(cx) {
                        t.run_edit(EditOp::Delete, "Cut", cx);
                    }
                },
                cx,
            ))
            .child(button(
                "ae-copy",
                "Copy",
                "Copy the selection (⌘C)",
                edit,
                |t, cx| {
                    t.copy_selection(cx);
                },
                cx,
            ))
            .child(button(
                "ae-paste",
                "Paste",
                "Paste at the cursor or over the selection (⌘V)",
                can_paste,
                |t, cx| t.paste(cx),
                cx,
            ))
            .child(button(
                "ae-delete",
                "Delete",
                "Delete the selection and close the gap (⌫)",
                edit,
                |t, cx| t.run_edit(EditOp::Delete, "Delete", cx),
                cx,
            ));
        let process = group()
            .child(button(
                "ae-silence",
                "Silence",
                "Replace the selection with silence",
                edit,
                |t, cx| t.run_edit(EditOp::Silence, "Silence", cx),
                cx,
            ))
            .child(button(
                "ae-crop",
                "Crop",
                "Keep only the selection",
                edit,
                |t, cx| t.run_edit(EditOp::Crop, "Crop", cx),
                cx,
            ))
            .child(button(
                "ae-fade-in",
                "Fade In",
                "Fade the selection in",
                edit,
                |t, cx| t.run_edit(EditOp::FadeIn, "Fade In", cx),
                cx,
            ))
            .child(button(
                "ae-fade-out",
                "Fade Out",
                "Fade the selection out",
                edit,
                |t, cx| t.run_edit(EditOp::FadeOut, "Fade Out", cx),
                cx,
            ))
            .child(button(
                "ae-normalize",
                "Normalize",
                "Raise the selection's peak to −0.1 dBFS",
                edit,
                |t, cx| {
                    t.run_edit(
                        EditOp::Normalize {
                            target_db: NORMALIZE_TARGET_DB,
                        },
                        "Normalize",
                        cx,
                    )
                },
                cx,
            ))
            .child(button(
                "ae-reverse",
                "Reverse",
                "Reverse the selection",
                edit,
                |t, cx| t.run_edit(EditOp::Reverse, "Reverse", cx),
                cx,
            ));
        let transport = group()
            .child(button(
                "ae-play",
                "Play",
                "Play the selection, or from the cursor to the clip end",
                true,
                |t, cx| t.play_selection(cx),
                cx,
            ))
            .child(button(
                "ae-split",
                "Split",
                "Split the clip at the cursor",
                self.busy.is_none(),
                |t, cx| t.split_at_cursor(cx),
                cx,
            ));
        let zoom = group()
            .child(button(
                "ae-zoom-out",
                "−",
                "Zoom out (⌘ + wheel)",
                true,
                |t, cx| t.zoom_by(0.5, cx),
                cx,
            ))
            .child(button(
                "ae-zoom-in",
                "+",
                "Zoom in (⌘ + wheel)",
                true,
                |t, cx| t.zoom_by(2.0, cx),
                cx,
            ))
            .child(button(
                "ae-fit",
                "Fit",
                "Show the whole clip",
                true,
                |t, cx| t.fit(cx),
                cx,
            ));
        let tools_open = self.tools_menu_open;
        let tools = tool_button(
            "ae-tools",
            "Tools ▾",
            "Analysis and processing tools",
            true,
            cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.focus.focus(window, cx);
                this.tools_menu_open = !tools_open;
                cx.notify();
            }),
        );

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .px(px(space::BASE))
            .py(px(space::TIGHT))
            .flex_none()
            .overflow_hidden()
            .bg(Colors::surface_panel())
            .border_b_1()
            .border_color(Colors::border_subtle())
            .child(div().flex_none().child(mode_track))
            .child(edits)
            .child(process)
            .child(transport)
            .child(zoom)
            .child(div().flex_1())
            .child(tools)
    }

    fn tools_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("audio-editor-tools-menu")
            .absolute()
            .top(px(size::PROMINENT + space::TIGHT * 2.0 + space::HAIR))
            .right(px(space::BASE))
            .w(px(220.0))
            .max_h(px(360.0))
            .overflow_y_scroll()
            .p(px(space::TIGHT))
            .rounded(px(radius::SURFACE))
            .bg(Colors::surface_card())
            .border_1()
            .border_color(Colors::border_normal())
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .children(TOOL_MENU.iter().enumerate().map(|(index, kind)| {
                let kind = *kind;
                div()
                    .id(("audio-editor-tool", index))
                    .role(Role::MenuItem)
                    .h(px(size::ROW))
                    .px(px(space::BASE))
                    .flex()
                    .items_center()
                    .rounded(px(radius::CONTROL))
                    .text_size(px(typography::UI_SM))
                    .text_color(Colors::text_primary())
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|s| s.bg(Colors::surface_control_hover()))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.open_tool(kind, cx)),
                    )
                    .child(kind.label())
            }))
            .into_any_element()
    }

    fn status_bar(&self, view: &ClipView, cx: &App) -> impl IntoElement {
        let state = &self.timeline.read(cx).state;
        let channels = match view.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} ch"),
        };
        let source = format!(
            "{} · {:.1} kHz · {}",
            view.file_name,
            view.sample_rate as f64 / 1000.0,
            channels
        );
        let position = match self.selection {
            Some((a, b)) if (a - b).abs() > 1.0e-9 => {
                let (lo, hi) = (a.min(b), a.max(b));
                let seconds = state.seconds_at_beat(view.abs_start + hi)
                    - state.seconds_at_beat(view.abs_start + lo);
                format!(
                    "Selection {} – {} · {:.3} s",
                    state.format_position_at(view.abs_start + lo),
                    state.format_position_at(view.abs_start + hi),
                    seconds
                )
            }
            _ => format!(
                "Cursor {}",
                state.format_position_at(view.abs_start + self.cursor)
            ),
        };
        let (message, error) = match (&self.status, self.mode) {
            (Some((message, error)), _) => (message.clone(), *error),
            (None, EditMode::Select) => (
                "Drag to select · Shift extends · ⌥ ignores the grid · double-click selects all"
                    .to_string(),
                false,
            ),
            (None, EditMode::Gain) => (
                "Click to add a point · drag to move · ⌥-click removes".to_string(),
                false,
            ),
            (None, EditMode::Warp) => (
                "Click to add a marker · drag to warp · ⌥-click removes".to_string(),
                false,
            ),
        };
        let text = |content: String| {
            div()
                .flex_none()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(content)
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .h(px(size::DEFAULT))
            .px(px(space::BASE))
            .flex_none()
            .overflow_hidden()
            .bg(Colors::statusbar_bg())
            .border_t_1()
            .border_color(Colors::border_subtle())
            .text_size(px(typography::UI_XS))
            .text_color(Colors::statusbar_text())
            .child(text(source).max_w(px(320.0)))
            .child(text(position).text_color(Colors::text_secondary()))
            .child(div().flex_1())
            .child(
                text(message)
                    .flex_shrink(1.0)
                    .min_w_0()
                    .text_color(if error {
                        Colors::status_error()
                    } else {
                        Colors::statusbar_text_muted()
                    }),
            )
    }

    fn ruler(
        &self,
        view: &ClipView,
        grid: Arc<Vec<GridLine>>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let vp = self.view;
        let duration = view.duration;
        let labels: Vec<AnyElement> = grid
            .iter()
            .filter_map(|line| {
                let label = line.label.clone()?;
                let x = vp.x_at(line.rel) as f32;
                (x >= -40.0 && x <= vp.width as f32).then(|| {
                    div()
                        .absolute()
                        .left(px(x + space::HAIR + 1.0))
                        .top(px(space::HAIR))
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::timeline_ruler_text())
                        .whitespace_nowrap()
                        .child(label)
                        .into_any_element()
                })
            })
            .collect();
        let ticks = grid.clone();
        div()
            .relative()
            .h(px(RULER_H))
            .flex_none()
            .overflow_hidden()
            .bg(Colors::surface_panel())
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::ruler_down))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        paint::paint_ruler(bounds, &ticks, vp, duration, &palette, window);
                    },
                )
                .absolute()
                .inset_0(),
            )
            .children(labels)
    }

    fn overview(
        &self,
        view: &ClipView,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let map = view.map.clone();
        let audio = self.audio_for(view.path.as_deref());
        let duration = view.duration;
        let vp = self.view;
        let bounds_cell = self.overview_bounds.clone();
        div()
            .h(px(OVERVIEW_H))
            .flex_none()
            .border_b_1()
            .border_color(Colors::border_subtle())
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::overview_down))
            .child(
                canvas(
                    move |bounds, _, _| bounds_cell.set(bounds),
                    move |bounds, _, window, _| {
                        paint::paint_overview(
                            bounds,
                            map.as_ref(),
                            audio.as_deref(),
                            duration,
                            vp,
                            &palette,
                            window,
                        );
                    },
                )
                .size_full(),
            )
    }

    fn lanes(
        &self,
        view: &ClipView,
        grid: Arc<Vec<GridLine>>,
        palette: Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let audio = self.audio_for(view.path.as_deref());
        let lanes = audio
            .as_ref()
            .map(|a| a.pcm.channels)
            .unwrap_or(view.channels)
            .max(1);
        let envelope = match self.mode {
            EditMode::Gain => Some((view.envelope.clone(), true)),
            _ if !view.envelope.points.is_empty() => Some((view.envelope.clone(), false)),
            _ => None,
        };
        let frame = LaneFrame {
            view: self.view,
            map: view.map.clone(),
            duration: view.duration,
            audio: audio.clone(),
            lanes,
            amp_zoom: self.amp_zoom,
            grid,
            selection: self.selection,
            cursor: (self.selection.is_none() || self.drag != Drag::None).then_some(self.cursor),
            fade_in_end: view.fade_in_end,
            fade_out_start: view.fade_out_start,
            envelope,
            warp: view.warp.clone(),
            warp_emphasized: self.mode == EditMode::Warp,
            palette,
        };
        let bounds_cell = self.lanes_bounds.clone();
        let measured_width = self.view.width;
        let overlay_label = if audio.is_some() {
            None
        } else if let Some((path, error)) = &self.failed {
            (Some(path) == view.path.as_ref())
                .then(|| (format!("Could not open the audio: {error}"), true))
        } else if let Some(label) = &view.import_label {
            Some((label.clone(), false))
        } else {
            Some(("Loading audio…".to_string(), false))
        };
        let channel_labels: Vec<AnyElement> = if lanes == 2 {
            ["L", "R"]
                .iter()
                .enumerate()
                .map(|(i, label)| {
                    div()
                        .absolute()
                        .left(px(space::TIGHT))
                        .top(px(HANDLE_BAND + space::HAIR))
                        .when(i == 1, |d| {
                            d.top_1_2().mt(px(HANDLE_BAND * 0.5 + space::HAIR))
                        })
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(*label)
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        // While a gesture is live, listen window-wide so a drag that leaves
        // the lanes keeps tracking and still ends where it is released.
        let tracker = (self.drag != Drag::None).then(|| {
            let this = cx.weak_entity();
            let up = this.clone();
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        let _ = this.update(cx, |this, cx| {
                            this.pointer_move(event.position, event.modifiers, cx)
                        });
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase != gpui::DispatchPhase::Bubble || event.button != MouseButton::Left
                        {
                            return;
                        }
                        let _ = up.update(cx, |this, cx| this.pointer_up(cx));
                    });
                },
            )
            .absolute()
            .inset_0()
        });
        let cursor_style = match self.mode {
            EditMode::Select => gpui::CursorStyle::IBeam,
            EditMode::Gain | EditMode::Warp => gpui::CursorStyle::Crosshair,
        };
        let _ = window;

        div()
            .id("audio-editor-lanes")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .cursor(cursor_style)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::lanes_down))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.drag != Drag::None {
                        this.pointer_up(cx);
                    }
                }),
            )
            .child(
                canvas(
                    move |bounds, window, _| {
                        bounds_cell.set(bounds);
                        let width: f32 = bounds.size.width.into();
                        if (width as f64 - measured_width).abs() > 0.5 {
                            window.refresh();
                        }
                    },
                    move |bounds, _, window, _| {
                        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                            paint::paint_lanes(bounds, &frame, window);
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .children(channel_labels)
            .child(self.playhead.clone())
            .when_some(overlay_label, |d, (label, error)| {
                d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(typography::UI_SM))
                        .text_color(if error {
                            Colors::status_error()
                        } else {
                            Colors::text_muted()
                        })
                        .child(label),
                )
            })
            .children(tracker)
    }
}

/// A toolbar button that re-takes keyboard focus for the editor (the Studio
/// root claims focus on every press) and then runs `action`.
fn action_button(
    id: &'static str,
    label: &'static str,
    tip: &'static str,
    enabled: bool,
    action: fn(&mut AudioEditorHost, &mut Context<AudioEditorHost>),
    cx: &mut Context<AudioEditorHost>,
) -> AnyElement {
    tool_button(
        id,
        label,
        tip,
        enabled,
        cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.focus.focus(window, cx);
            this.tools_menu_open = false;
            action(this, cx);
        }),
    )
    .into_any_element()
}

/// Compact ghost button for the editor toolbar: no fill at rest, the shared
/// hover/pressed state layers, and a tooltip naming what it does.
fn tool_button(
    id: &'static str,
    label: &'static str,
    tooltip: &'static str,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let base = Colors::surface_panel();
    let hover = Colors::composite(base, Colors::state_hover());
    let pressed = Colors::composite(base, Colors::state_recessed());
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .aria_disabled(!enabled)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .h(px(size::DEFAULT))
        .min_w(px(size::DEFAULT))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if enabled {
            Colors::text_secondary()
        } else {
            Colors::text_disabled()
        })
        .tooltip(fb_tooltip(tooltip))
        .when(enabled, |b| {
            b.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover).text_color(Colors::text_primary()))
                .active(move |s| s.bg(pressed))
                .on_click(on_click)
        })
        .child(label)
}

/// Bar, beat and subdivision lines across the visible part of the clip,
/// thinned so neither lines nor labels crowd at any zoom.
fn build_grid(sig: &TimeSignatureMap, clip_start: f64, view: Viewport) -> Vec<GridLine> {
    const MIN_LINE_PX: f64 = 6.0;
    const MIN_LABEL_PX: f64 = 44.0;
    let ppb = view.pixels_per_beat;
    let left = (clip_start + view.scroll).max(0.0);
    let right = clip_start + view.scroll + view.visible_beats();
    let mut out = Vec::new();
    if right <= left || ppb <= 0.0 {
        return out;
    }
    let mut bar = sig.bar_at_beat(left);
    for _ in 0..4096 {
        let start = sig.bar_start_beat(bar);
        if start > right {
            break;
        }
        let bb = sig.bar_beat_at_beat(start);
        let beats_per_bar = beats_per_bar_from_sig(bb.numerator, bb.denominator);
        let bar_px = beats_per_bar * ppb;
        let stride = |min_px: f64| {
            let mut k = 1i64;
            while (k as f64) * bar_px < min_px && k < 1 << 20 {
                k *= 2;
            }
            k
        };
        let index = bar - 1;
        if index % stride(MIN_LINE_PX) == 0 {
            out.push(GridLine {
                rel: start - clip_start,
                kind: GridKind::Bar,
                label: (index % stride(MIN_LABEL_PX) == 0).then(|| bar.to_string()),
            });
        }
        let beats = bb.numerator.max(1) as usize;
        let unit = beats_per_bar / beats as f64;
        if unit * ppb >= MIN_LINE_PX {
            for i in 0..beats {
                let beat_start = start + unit * i as f64;
                if i > 0 {
                    out.push(GridLine {
                        rel: beat_start - clip_start,
                        kind: GridKind::Beat,
                        label: (unit * ppb >= MIN_LABEL_PX).then(|| format!("{bar}.{}", i + 1)),
                    });
                }
                for division in [4usize, 2] {
                    let sub = unit / division as f64;
                    if sub * ppb >= MIN_LINE_PX {
                        for j in 1..division {
                            out.push(GridLine {
                                rel: beat_start + sub * j as f64 - clip_start,
                                kind: GridKind::Sub,
                                label: None,
                            });
                        }
                        break;
                    }
                }
            }
        }
        bar += 1;
    }
    out
}

impl Render for AudioEditorHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_clip(cx);
        self.focused.set(self.focus.is_focused(window));

        let root = div()
            .id("audio-editor")
            .key_context("AudioEditor")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::surface_base());

        let Some(view) = self.clip_view(cx) else {
            return root
                .items_center()
                .justify_center()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_muted())
                .child("Select an audio clip to edit it here");
        };
        if let Some(path) = view.path.clone() {
            self.ensure_audio(&path, cx);
        }

        self.view.width = f32::from(self.lanes_bounds.get().size.width) as f64;
        if self.fitted.as_deref() != Some(view.id.as_str()) && self.view.width > 1.0 {
            self.view.fit(view.duration);
            self.fitted = Some(view.id.clone());
        }
        if self.view.width > 1.0 {
            let max = Self::max_pixels_per_beat(&view);
            self.view.pixels_per_beat = self
                .view
                .pixels_per_beat
                .clamp(Viewport::MIN_PIXELS_PER_BEAT, max.max(1.0));
            self.view.clamp(view.duration);
        }
        self.cursor = self.cursor.clamp(0.0, view.duration);

        // Scrolling and zooming move the line too, not only the transport.
        if let Some((rel, duration, _)) = self.playhead_rel(cx) {
            self.place_playhead(rel, duration);
        }

        let grid = Arc::new(build_grid(
            &self.timeline.read(cx).state.time_signature_map,
            view.abs_start,
            self.view,
        ));
        let palette = Self::palette(view.track_color);
        let tools_menu = self.tools_menu_open.then(|| self.tools_menu(cx));

        root.on_scroll_wheel(cx.listener(Self::on_wheel))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if this.tools_menu_open {
                        this.tools_menu_open = false;
                        cx.notify();
                    }
                }),
            )
            .child(self.toolbar(&view, cx))
            .child(self.overview(&view, palette, cx))
            .child(self.ruler(&view, grid.clone(), palette, cx))
            .child(self.lanes(&view, grid, palette, window, cx))
            .child(self.status_bar(&view, cx))
            .children(tools_menu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_thins_lines_and_labels_with_zoom() {
        let sig = TimeSignatureMap::with_default_4_4();
        let close = Viewport {
            scroll: 0.0,
            pixels_per_beat: 80.0,
            width: 640.0,
        };
        let lines = build_grid(&sig, 0.0, close);
        // 8 beats on screen: bars, beats and subdivisions, bars labelled.
        assert!(lines.iter().any(|l| l.kind == GridKind::Sub));
        assert!(lines
            .iter()
            .any(|l| l.kind == GridKind::Bar && l.label.as_deref() == Some("1")));
        let far = Viewport {
            scroll: 0.0,
            pixels_per_beat: 1.0,
            width: 640.0,
        };
        let lines = build_grid(&sig, 0.0, far);
        assert!(lines.iter().all(|l| l.kind == GridKind::Bar));
        // At 4 px a bar, lines every 2nd bar and labels every 16th.
        let labelled: Vec<_> = lines.iter().filter(|l| l.label.is_some()).collect();
        assert!(labelled.len() >= 2);
        assert!(labelled
            .windows(2)
            .all(|w| (w[1].rel - w[0].rel) * 1.0 >= 44.0));
    }

    #[test]
    fn grid_positions_are_clip_relative() {
        let sig = TimeSignatureMap::with_default_4_4();
        let view = Viewport {
            scroll: 0.0,
            pixels_per_beat: 20.0,
            width: 400.0,
        };
        // Clip starts on beat 6: the bar at beat 8 sits 2 beats in.
        let lines = build_grid(&sig, 6.0, view);
        assert!(lines
            .iter()
            .any(|l| l.kind == GridKind::Bar && (l.rel - 2.0).abs() < 1e-9));
    }
}
