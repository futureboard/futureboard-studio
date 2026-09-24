//! Find Tempo & Key: an independent utility window that analyzes one audio
//! clip and offers the result to the project.
//!
//! Opened from the transport's Find Tempo & Key button, which is enabled only
//! while an audio clip is selected. The window owns only its analysis and
//! animation state; every project change goes back to the Studio as a
//! [`TempoKeyCommand`], applied there through the same tempo/clip/piano-roll
//! paths the rest of the app uses.
//!
//! Analysis runs on the background executor (decode, slice, downmix, tempo,
//! chromagram, key ranking) and is tagged with a generation so a re-target or
//! scope change drops a stale result instead of showing it. The wheel animates
//! only while it carries information — the analysis sweep, values settling,
//! and the beat pulse at the detected tempo — and only while the window is
//! active, so an idle window costs nothing.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, AnyElement, App, AppContext, Bounds, Context, FocusHandle, FontFeatures,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, Rgba,
    StatefulInteractiveElement, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind,
};
use SphereAudioProcessor::analysis::{
    analyze_rhythm, chroma_frames, recognize_chords, ChordKind, ChordOptions, ChordSegment,
    ChromaFrames, RhythmAnalysis, RhythmOptions,
};
use SphereAudioProcessor::{
    downmix_interleaved, estimate_bpm_candidates, pitch_class_profile, rank_keys, slice_frames,
    KeyEstimate, KeyMode, TempoCandidate,
};

use crate::components::controls::{
    fb_badge, fb_button, fb_checkbox, fb_section_label, fb_segment, fb_segmented_track, fb_tooltip,
    FbButtonKind, FbSegment,
};
use crate::components::timeline::timeline_state::{ClipType, TimelineState};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

use crate::components::pitch_wheel::{self as visual, fifths_pitch_class, ring, WheelFrame};

pub const TEMPO_KEY_WINDOW_WIDTH: f32 = 780.0;
pub const TEMPO_KEY_WINDOW_HEIGHT: f32 = 780.0;
/// Sized so both cards, the wheel and the footer fit without scrolling; the
/// card column still scrolls rather than clip if the platform chrome is taller.
const MIN_WIDTH: f32 = 700.0;
const MIN_HEIGHT: f32 = 700.0;
/// Height of the tempo / beats / chords strip.
const STRIP_TEMPO_H: f32 = 84.0;
const STRIP_BEATS_H: f32 = 18.0;
const STRIP_CHORDS_H: f32 = 26.0;
/// Tempo chips shown; the strongest readings, ordered by BPM.
const TEMPO_CHIPS: usize = 4;

/// Logical edge of the wheel. Fixed, so the pitch-class labels and the centre
/// readout are laid out from the same geometry the painters use.
const WHEEL_SIZE: f32 = 280.0;
/// Tempo search range handed to the estimator.
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;
/// Below this much audio neither estimate means anything.
const MIN_ANALYSIS_SECONDS: f32 = 1.0;
/// Animation tick. Frames are only produced while something moves.
const FRAME: Duration = Duration::from_millis(16);
/// Time constant of the segment settle after a result arrives.
const SETTLE_SECONDS: f32 = 0.14;
/// One analysis-sweep revolution.
const SWEEP_SECONDS: f32 = 1.6;
const ALTERNATE_KEYS: usize = 4;

/// The clip a window analyzes, resolved from the timeline selection.
#[derive(Debug, Clone, PartialEq)]
pub struct TempoKeyTarget {
    pub clip_id: String,
    pub clip_name: String,
    pub source_path: String,
    /// Source-file frames the clip plays, `end == 0` meaning "to the end".
    pub source_start: u64,
    pub source_end: u64,
}

impl TempoKeyTarget {
    fn trimmed(&self) -> bool {
        self.source_start > 0 || self.source_end > 0
    }

    fn file_label(&self) -> String {
        std::path::Path::new(&self.source_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.source_path.clone())
    }
}

/// The first selected audio clip with a source file — the one the transport
/// button acts on. `None` disables the button.
pub fn tempo_key_target(state: &TimelineState) -> Option<TempoKeyTarget> {
    let (_, clip) = state
        .selection
        .selected_clip_ids
        .iter()
        .filter_map(|clip_id| state.find_clip(clip_id))
        .find(|(_, clip)| matches!(clip.clip_type, ClipType::Audio { .. }))?;
    let ClipType::Audio {
        source_path: Some(path),
        ..
    } = &clip.clip_type
    else {
        return None;
    };
    if path.trim().is_empty() {
        return None;
    }
    let start = clip.stretch.source_start_samples;
    let end = if clip.stretch.source_end_samples > start {
        clip.stretch.source_end_samples
    } else {
        0
    };
    Some(TempoKeyTarget {
        clip_id: clip.id.clone(),
        clip_name: clip.name.clone(),
        source_path: path.clone(),
        source_start: start,
        source_end: end,
    })
}

/// A project change the window asks the Studio to make.
pub enum TempoKeyCommand {
    SetProjectTempo {
        bpm: f64,
    },
    SetClipTempo {
        clip_id: String,
        bpm: f64,
    },
    ApplyScale {
        tonic: usize,
        minor: bool,
    },
    SetProjectKey {
        tonic: usize,
        minor: bool,
    },
    /// Lay the project's tempo and meter over the clip's detected beats.
    /// `beats` are seconds on the source file's clock.
    MapTempo {
        clip_id: String,
        beats: Vec<f64>,
        positions: Vec<u32>,
        /// Beats on a steady section's grid (see `Beat::locked`).
        locked: Vec<bool>,
        beats_per_bar: u32,
    },
    /// Put detected chords on the Chord Track. Seconds on the source file's
    /// clock; `(start, end, root, kind)`.
    PlaceChords {
        clip_id: String,
        chords: Vec<(f64, f64, u8, ChordKind)>,
        flats: bool,
    },
}

#[derive(Clone)]
pub struct TempoKeyFinderCallbacks {
    pub on_command: Arc<dyn Fn(TempoKeyCommand, &mut App) + Send + Sync>,
    pub on_close: Arc<dyn Fn(Bounds<Pixels>, &mut App) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    ClipRange,
    WholeFile,
}

#[derive(Debug, Clone)]
struct Analysis {
    /// Tempo candidates in ascending BPM order.
    tempos: Vec<TempoCandidate>,
    /// Ranked keys, best first.
    keys: Vec<KeyEstimate>,
    /// Pitch-class energy scaled so the strongest class is 1.
    energy: [f32; 12],
    seconds: f32,
    /// Source-file seconds of the analysed audio's first sample. Every time
    /// below is relative to it.
    offset_seconds: f64,
    /// Beats, bars, meter and tempo sections.
    rhythm: Option<RhythmAnalysis>,
    /// Chroma kept so the chord vocabulary can change without re-analysing.
    chroma: Option<Arc<ChromaFrames>>,
    chords: Vec<ChordSegment>,
}

enum Phase {
    Analyzing,
    Ready(Analysis),
    Failed(String),
}

pub struct TempoKeyFinderWindow {
    target: TempoKeyTarget,
    callbacks: TempoKeyFinderCallbacks,
    focus: FocusHandle,
    scope: Scope,
    phase: Phase,
    generation: u64,
    tempo_pick: usize,
    key_pick: usize,
    notice: Option<String>,
    /// The notice reports a failure.
    notice_error: bool,
    /// Recognise 7th chords, not just major/minor.
    sevenths: bool,
    // Animation state, advanced by the tick loop only while it moves.
    pulse: bool,
    window_active: bool,
    /// A tick loop is running. It stops itself once a tick changes nothing.
    ticking: bool,
    _activation: Option<gpui::Subscription>,
    last_tick: Instant,
    shown_energy: [f32; 12],
    sweep: f32,
    beats: f64,
}

impl TempoKeyFinderWindow {
    fn new(
        target: TempoKeyTarget,
        callbacks: TempoKeyFinderCallbacks,
        cx: &mut Context<Self>,
    ) -> Self {
        let scope = if target.trimmed() {
            Scope::ClipRange
        } else {
            Scope::WholeFile
        };
        let mut window = Self {
            target,
            callbacks,
            focus: cx.focus_handle(),
            scope,
            phase: Phase::Analyzing,
            generation: 0,
            tempo_pick: 0,
            key_pick: 0,
            notice: None,
            notice_error: false,
            sevenths: true,
            pulse: true,
            window_active: true,
            ticking: false,
            _activation: None,
            last_tick: Instant::now(),
            shown_energy: [0.0; 12],
            sweep: 0.0,
            beats: 0.0,
        };
        window.analyze(cx);
        window
    }

    /// Point the window at another clip (the transport button was pressed
    /// with a different selection) and analyze it.
    pub fn retarget(&mut self, target: TempoKeyTarget, cx: &mut Context<Self>) {
        if target == self.target && !matches!(self.phase, Phase::Failed(_)) {
            return;
        }
        self.scope = if target.trimmed() {
            Scope::ClipRange
        } else {
            Scope::WholeFile
        };
        self.target = target;
        self.analyze(cx);
    }

    fn analyze(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.phase = Phase::Analyzing;
        self.notice = None;
        self.shown_energy = [0.0; 12];
        self.beats = 0.0;
        let path = self.target.source_path.clone();
        let range = match self.scope {
            Scope::ClipRange => Some((self.target.source_start, self.target.source_end)),
            Scope::WholeFile => None,
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { run_analysis(&path, range) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A newer request superseded this one; its result is stale.
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(analysis) => {
                        this.tempo_pick = best_tempo_index(&analysis.tempos);
                        this.key_pick = 0;
                        this.phase = Phase::Ready(analysis);
                    }
                    Err(error) => this.phase = Phase::Failed(error),
                }
                this.ensure_ticking(cx);
                cx.notify();
            });
        })
        .detach();
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// Start the frame loop if it is not running. Called on every event that
    /// can set something in motion; the loop ends itself when nothing moves,
    /// so an idle or background window wakes nothing.
    fn ensure_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        self.last_tick = Instant::now();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(FRAME).await;
            match this.update(cx, |this, cx| this.tick(cx)) {
                Ok(true) => {}
                _ => break,
            }
        })
        .detach();
    }

    /// Follow window activation: pause the animation in the background and
    /// resume it on return.
    fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.window_active != active {
            self.window_active = active;
            self.ensure_ticking(cx);
            cx.notify();
        }
    }

    /// Advance the animation by real elapsed time. Repaints only when a frame
    /// would differ; an inactive window snaps to its final state and stops.
    /// Returns whether the loop should keep running.
    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;
        let mut changed = false;
        match &self.phase {
            Phase::Analyzing => {
                if self.window_active {
                    self.sweep = (self.sweep + dt / SWEEP_SECONDS * TAU) % TAU;
                    changed = true;
                }
            }
            Phase::Ready(analysis) => {
                let target = analysis.energy;
                let blend = if self.window_active {
                    1.0 - (-dt / SETTLE_SECONDS).exp()
                } else {
                    1.0
                };
                for (shown, goal) in self.shown_energy.iter_mut().zip(target) {
                    let next = if (goal - *shown).abs() < 0.002 {
                        goal
                    } else {
                        *shown + (goal - *shown) * blend
                    };
                    if next != *shown {
                        *shown = next;
                        changed = true;
                    }
                }
                if self.pulse && self.window_active {
                    if let Some(bpm) = analysis.tempos.get(self.tempo_pick).map(|t| t.bpm) {
                        self.beats += dt as f64 * bpm as f64 / 60.0;
                        changed = true;
                    }
                }
            }
            Phase::Failed(_) => {}
        }
        if changed {
            cx.notify();
        } else {
            self.ticking = false;
        }
        changed
    }

    fn analysis(&self) -> Option<&Analysis> {
        match &self.phase {
            Phase::Ready(analysis) => Some(analysis),
            _ => None,
        }
    }

    fn picked_tempo(&self) -> Option<TempoCandidate> {
        self.analysis()?.tempos.get(self.tempo_pick).copied()
    }

    fn picked_key(&self) -> Option<KeyEstimate> {
        self.analysis()?.keys.get(self.key_pick).copied()
    }

    fn dispatch(&mut self, command: TempoKeyCommand, notice: String, cx: &mut Context<Self>) {
        (self.callbacks.on_command)(command, cx);
        self.notice = Some(notice);
        self.notice_error = false;
        cx.notify();
    }

    /// Report how a command went; the Studio calls this once it has applied
    /// (or refused) it.
    pub fn set_notice(&mut self, text: String, error: bool, cx: &mut Context<Self>) {
        self.notice = Some(text);
        self.notice_error = error;
        cx.notify();
    }

    fn set_sevenths(&mut self, sevenths: bool, cx: &mut Context<Self>) {
        self.sevenths = sevenths;
        if let Phase::Ready(analysis) = &mut self.phase {
            if let (Some(frames), Some(rhythm)) = (&analysis.chroma, &analysis.rhythm) {
                analysis.chords = detect_chords(frames, rhythm, sevenths);
            }
        }
        cx.notify();
    }

    fn map_tempo(&mut self, cx: &mut Context<Self>) {
        let Some(analysis) = self.analysis() else {
            return;
        };
        let Some(rhythm) = &analysis.rhythm else {
            return;
        };
        let offset = analysis.offset_seconds;
        let command = TempoKeyCommand::MapTempo {
            clip_id: self.target.clip_id.clone(),
            beats: rhythm.beats.iter().map(|b| b.seconds + offset).collect(),
            positions: rhythm.beats.iter().map(|b| b.position).collect(),
            locked: rhythm.beats.iter().map(|b| b.locked).collect(),
            beats_per_bar: rhythm.beats_per_bar,
        };
        self.dispatch(command, "Mapping tempo…".to_string(), cx);
    }

    fn place_chords(&mut self, cx: &mut Context<Self>) {
        let Some(analysis) = self.analysis() else {
            return;
        };
        let offset = analysis.offset_seconds;
        let chords: Vec<(f64, f64, u8, ChordKind)> = analysis
            .chords
            .iter()
            .filter_map(|c| {
                let label = c.chord?;
                Some((
                    c.start_seconds + offset,
                    c.end_seconds + offset,
                    label.root,
                    label.kind,
                ))
            })
            .collect();
        let flats = key_uses_flats(self.picked_key().as_ref());
        let command = TempoKeyCommand::PlaceChords {
            clip_id: self.target.clip_id.clone(),
            chords,
            flats,
        };
        self.dispatch(command, "Adding chords…".to_string(), cx);
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        (self.callbacks.on_close)(window.bounds(), cx);
        visual::release_gpu_frame(cx);
        window.remove_window();
    }

    fn wheel_frame(&self, scale: f32) -> WheelFrame {
        let (tonic, in_key) = match self.picked_key() {
            Some(key) => (Some(tonic_index(&key)), scale_members(&key)),
            None => (None, [false; 12]),
        };
        let pulsing = self.pulse && self.picked_tempo().is_some();
        let beat_phase = pulsing.then(|| self.beats.fract() as f32);
        let bar_phase = if pulsing {
            ((self.beats / 4.0).fract()) as f32
        } else {
            0.0
        };
        WheelFrame {
            size: WHEEL_SIZE,
            scale,
            energy: self.shown_energy,
            in_key,
            tonic,
            sweep: matches!(self.phase, Phase::Analyzing).then_some(self.sweep),
            beat_phase,
            bar_phase,
        }
    }

    fn wheel(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.wheel_frame(window.scale_factor());
        let painted =
            visual::render_wgpu(&frame, cx).unwrap_or_else(|| visual::render_gpui(&frame));
        let radius = WHEEL_SIZE * 0.5;
        let (tonic, in_key) = (frame.tonic, frame.in_key);
        let labels = (0..12).map(move |position| {
            let pc = fifths_pitch_class(position);
            let angle = (position as f32 + 0.5) * TAU / 12.0;
            let r = radius * ring::LABEL;
            let x = radius + r * angle.sin();
            let y = radius - r * angle.cos();
            let color = if tonic == Some(pc) {
                Colors::text_primary()
            } else if in_key[pc] {
                Colors::text_secondary()
            } else {
                Colors::text_faint()
            };
            div()
                .absolute()
                .left(px(x - 12.0))
                .top(px(y - 7.0))
                .w(px(24.0))
                .h(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(if tonic == Some(pc) {
                    gpui::FontWeight::BOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .text_color(color)
                .child(pitch_name(pc))
        });
        div()
            .relative()
            .flex_none()
            .size(px(WHEEL_SIZE))
            .child(painted)
            .children(labels)
            .child(self.wheel_readout())
    }

    /// Centre readout: the picked tempo over the picked key.
    fn wheel_readout(&self) -> impl IntoElement {
        let disc = WHEEL_SIZE * ring::DISC;
        let (value, caption, key) = match &self.phase {
            Phase::Analyzing => ("—".to_string(), "Analyzing".to_string(), String::new()),
            Phase::Failed(_) => ("—".to_string(), "No result".to_string(), String::new()),
            Phase::Ready(_) => (
                self.picked_tempo()
                    .map(|t| format_bpm(t.bpm))
                    .unwrap_or_else(|| "—".to_string()),
                "BPM".to_string(),
                self.picked_key().map(|k| k.label()).unwrap_or_default(),
            ),
        };
        div()
            .absolute()
            .left(px(WHEEL_SIZE * 0.5 - disc))
            .top(px(WHEEL_SIZE * 0.5 - disc))
            .size(px(disc * 2.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(space::HAIR))
            .child(
                div()
                    .text_size(px(30.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .font_features(tabular_features())
                    .text_color(Colors::text_primary())
                    .child(value),
            )
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_faint())
                    .child(caption),
            )
            .when(!key.is_empty(), |this| {
                this.child(
                    div()
                        .mt(px(space::TIGHT))
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::accent_primary())
                        .child(key),
                )
            })
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let summary = match &self.phase {
            Phase::Ready(analysis) => {
                format!(
                    "{} · {:.1} s analyzed",
                    self.target.file_label(),
                    analysis.seconds
                )
            }
            _ => self.target.file_label(),
        };
        let trimmed = self.target.trimmed();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .px(px(space::SECTION))
            .py(px(space::BASE))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_MD))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(self.target.clip_name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(summary),
                    ),
            )
            .child(
                div()
                    .id("tempo-key-scope")
                    .flex_none()
                    .w(px(212.0))
                    .tooltip(fb_tooltip(if trimmed {
                        "Analyze the part of the file the clip plays, or the whole file"
                    } else {
                        "This clip plays the whole file"
                    }))
                    .child(
                        fb_segmented_track()
                            .child(fb_segment(
                                "tempo-key-scope-clip",
                                "Clip Range",
                                self.scope == Scope::ClipRange,
                                FbSegment::First,
                                cx.listener(|this, _, _, cx| this.set_scope(Scope::ClipRange, cx)),
                            ))
                            .child(fb_segment(
                                "tempo-key-scope-file",
                                "Whole File",
                                self.scope == Scope::WholeFile,
                                FbSegment::Last,
                                cx.listener(|this, _, _, cx| this.set_scope(Scope::WholeFile, cx)),
                            )),
                    ),
            )
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.analyze(cx);
        }
    }

    fn tempo_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let analysis = self.analysis();
        let picked = self.picked_tempo();
        let candidates: Vec<TempoCandidate> =
            analysis.map(|a| a.tempos.clone()).unwrap_or_default();
        let count = candidates.len();
        let segments = candidates
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                fb_segment(
                    ("tempo-key-bpm", index),
                    format_bpm(candidate.bpm),
                    index == self.tempo_pick,
                    segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.tempo_pick = index;
                        this.notice = None;
                        cx.notify();
                    }),
                )
            });
        let clip_id = self.target.clip_id.clone();
        card()
            .child(card_heading(
                "TEMPO",
                picked.map(|t| tempo_confidence(t.confidence)),
            ))
            .child(value_line(
                picked
                    .map(|t| format_bpm(t.bpm))
                    .unwrap_or_else(|| "—".to_string()),
                "BPM",
            ))
            .when(count > 1, |this| {
                this.child(fb_segmented_track().children(segments))
            })
            .children(self.rhythm_summary())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(space::BASE))
                    .child(fb_button(
                        "tempo-key-map",
                        "Map Tempo",
                        FbButtonKind::Primary,
                        analysis.is_some_and(|a| a.rhythm.is_some()),
                        cx.listener(|this, _, _, cx| this.map_tempo(cx)),
                    ))
                    .child(fb_button(
                        "tempo-key-set-clip",
                        "Set Clip Tempo",
                        FbButtonKind::Default,
                        picked.is_some(),
                        cx.listener(move |this, _, _, cx| {
                            if let Some(tempo) = this.picked_tempo() {
                                let bpm = round_bpm(tempo.bpm);
                                this.dispatch(
                                    TempoKeyCommand::SetClipTempo {
                                        clip_id: clip_id.clone(),
                                        bpm,
                                    },
                                    format!("Clip tempo set to {} BPM", format_bpm(bpm as f32)),
                                    cx,
                                );
                            }
                        }),
                    )),
            )
    }

    fn key_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let picked = self.picked_key();
        let keys: Vec<KeyEstimate> = self
            .analysis()
            .map(|a| a.keys.iter().take(ALTERNATE_KEYS).copied().collect())
            .unwrap_or_default();
        let count = keys.len();
        let segments = keys.into_iter().enumerate().map(|(index, key)| {
            fb_segment(
                ("tempo-key-key", index),
                key.label(),
                index == self.key_pick,
                segment_position(index, count),
                cx.listener(move |this, _, _, cx| {
                    this.key_pick = index;
                    this.notice = None;
                    cx.notify();
                }),
            )
        });
        // Only the ranked winner carries a meaningful margin over the rest.
        let confidence = self
            .analysis()
            .and_then(|a| a.keys.first())
            .filter(|_| self.key_pick == 0)
            .map(|k| key_confidence(k.confidence));
        card()
            .child(card_heading("KEY", confidence))
            .child(value_line(
                picked
                    .map(|k| k.display_label())
                    .unwrap_or_else(|| "—".to_string()),
                "",
            ))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(
                        picked
                            .map(|k| format!("Relative {}", relative_key_label(&k)))
                            .unwrap_or_default(),
                    ),
            )
            .when(count > 1, |this| {
                this.child(fb_segmented_track().children(segments))
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(space::BASE))
                    .child(fb_button(
                        "tempo-key-set-project-key",
                        "Set Project Key",
                        FbButtonKind::Default,
                        picked.is_some(),
                        cx.listener(|this, _, _, cx| {
                            if let Some(key) = this.picked_key() {
                                this.dispatch(
                                    TempoKeyCommand::SetProjectKey {
                                        tonic: tonic_index(&key),
                                        minor: key.mode == KeyMode::Minor,
                                    },
                                    format!("Project key set to {}", key.display_label()),
                                    cx,
                                );
                            }
                        }),
                    ))
                    .child(fb_button(
                        "tempo-key-apply-scale",
                        "Apply to Piano Roll",
                        FbButtonKind::Default,
                        picked.is_some(),
                        cx.listener(|this, _, _, cx| {
                            if let Some(key) = this.picked_key() {
                                this.dispatch(
                                    TempoKeyCommand::ApplyScale {
                                        tonic: tonic_index(&key),
                                        minor: key.mode == KeyMode::Minor,
                                    },
                                    format!("Piano roll scale set to {}", key.display_label()),
                                    cx,
                                );
                            }
                        }),
                    )),
            )
    }

    /// One line under the tempo: meter, bars and whether it moves.
    fn rhythm_summary(&self) -> Option<AnyElement> {
        let rhythm = self.analysis()?.rhythm.as_ref()?;
        let bars = rhythm.beats.iter().filter(|b| b.position == 1).count();
        let tempo = if rhythm.variable && rhythm.sections.len() > 1 {
            let sections = &rhythm.sections;
            let path = if sections.len() <= 5 {
                sections
                    .iter()
                    .map(|s| format_bpm(s.bpm))
                    .collect::<Vec<_>>()
                    .join(" → ")
            } else {
                format!(
                    "{} → … → {}",
                    format_bpm(sections[0].bpm),
                    format_bpm(sections[sections.len() - 1].bpm)
                )
            };
            format!("Tempo changes: {path} BPM · {} sections", sections.len())
        } else if rhythm.variable {
            let (lo, hi) = rhythm
                .tempo_curve
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), (_, b)| {
                    (lo.min(*b), hi.max(*b))
                });
            format!("Tempo drifts {}–{} BPM", format_bpm(lo), format_bpm(hi))
        } else {
            format!("Steady {} BPM", format_bpm(rhythm.bpm))
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(tempo)
                .child(div().text_color(Colors::text_muted()).child(format!(
                    "{}/4 · {} bars · {} beats found",
                    rhythm.beats_per_bar,
                    bars,
                    rhythm.beats.len()
                )))
                .into_any_element(),
        )
    }

    fn chords_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let analysis = self.analysis();
        let flats = key_uses_flats(self.picked_key().as_ref());
        let chords: Vec<&ChordSegment> = analysis
            .map(|a| a.chords.iter().filter(|c| c.chord.is_some()).collect())
            .unwrap_or_default();
        let distinct = {
            let mut seen: Vec<String> = Vec::new();
            for c in &chords {
                let label = c.chord.unwrap();
                let name = chord_name(label.root, label.kind, flats);
                if !seen.contains(&name) {
                    seen.push(name);
                }
            }
            seen
        };
        let preview = chords
            .iter()
            .take(8)
            .map(|c| {
                let l = c.chord.unwrap();
                chord_name(l.root, l.kind, flats)
            })
            .collect::<Vec<_>>()
            .join(" · ");
        card()
            .child(card_heading("CHORDS", None))
            .child(value_line(
                if chords.is_empty() {
                    "—".to_string()
                } else {
                    chords.len().to_string()
                },
                "changes",
            ))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_secondary())
                    .truncate()
                    .child(if preview.is_empty() {
                        "No chords recognised".to_string()
                    } else {
                        format!("{preview}{}", if chords.len() > 8 { " …" } else { "" })
                    }),
            )
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(format!("{} different chords", distinct.len())),
            )
            .child(fb_checkbox(
                "tempo-key-sevenths",
                "Include 7th chords",
                self.sevenths,
                analysis.is_some(),
                cx.listener(|this, _, _, cx| {
                    let next = !this.sevenths;
                    this.set_sevenths(next, cx);
                }),
            ))
            .child(div().flex().flex_row().child(fb_button(
                "tempo-key-place-chords",
                "Add to Chord Track",
                FbButtonKind::Default,
                !chords.is_empty(),
                cx.listener(|this, _, _, cx| this.place_chords(cx)),
            )))
    }

    /// Tempo curve with its sections, the beat grid (downbeats tall) and the
    /// recognised chords, over the analysed audio.
    fn rhythm_strip(&self, window: &Window) -> Option<AnyElement> {
        let analysis = self.analysis()?;
        let rhythm = analysis.rhythm.as_ref()?;
        let duration = (analysis.seconds as f64).max(1e-3);
        let flats = key_uses_flats(self.picked_key().as_ref());
        let width_px: f32 = f32::from(window.viewport_size().width) - 2.0 * space::SECTION;
        let frac = |t: f64| (t / duration).clamp(0.0, 1.0) as f32;

        let (lo, hi) = rhythm
            .tempo_curve
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), (_, b)| {
                (lo.min(*b), hi.max(*b))
            });
        let pad = ((hi - lo) * 0.15).max(4.0);
        let (lo, hi) = (lo - pad, hi + pad);
        let curve: Vec<(f32, f32)> = rhythm
            .tempo_curve
            .iter()
            .map(|(t, b)| (frac(*t), 1.0 - (b - lo) / (hi - lo).max(1e-3)))
            .collect();
        let beats: Vec<(f32, bool)> = rhythm
            .beats
            .iter()
            .map(|b| (frac(b.seconds), b.position == 1))
            .collect();
        let sections: Vec<(f32, f32)> = rhythm
            .sections
            .iter()
            .map(|s| (frac(s.start_seconds), frac(s.end_seconds)))
            .collect();
        let chord_spans: Vec<(f32, f32, bool)> = analysis
            .chords
            .iter()
            .map(|c| {
                (
                    frac(c.start_seconds),
                    frac(c.end_seconds),
                    c.chord.is_some(),
                )
            })
            .collect();
        let line = Colors::accent_primary();
        let shade = Colors::with_alpha(Colors::text_primary(), 0.04);
        let tick = Colors::text_muted();
        let down = Colors::text_primary();
        let chord_fill = Colors::with_alpha(Colors::accent_primary(), 0.16);
        let chord_edge = Colors::border_subtle();

        let graphics = gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let ox: f32 = bounds.origin.x.into();
                let oy: f32 = bounds.origin.y.into();
                let w: f32 = bounds.size.width.into();
                let quad = |x: f32, y: f32, qw: f32, qh: f32| {
                    gpui::Bounds::new(
                        gpui::point(px(x), px(y)),
                        gpui::size(px(qw.max(0.0)), px(qh.max(0.0))),
                    )
                };
                for (i, (a, b)) in sections.iter().enumerate() {
                    if i % 2 == 1 {
                        window.paint_quad(gpui::fill(
                            quad(ox + a * w, oy, (b - a) * w, STRIP_TEMPO_H),
                            shade,
                        ));
                    }
                }
                if curve.len() >= 2 {
                    let mut path = gpui::PathBuilder::stroke(px(1.5));
                    for (i, (x, y)) in curve.iter().enumerate() {
                        let p =
                            gpui::point(px(ox + x * w), px(oy + 6.0 + y * (STRIP_TEMPO_H - 12.0)));
                        if i == 0 {
                            path.move_to(p);
                        } else {
                            path.line_to(p);
                        }
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, line);
                    }
                }
                let beats_top = oy + STRIP_TEMPO_H;
                for (x, is_down) in &beats {
                    let h = if *is_down {
                        STRIP_BEATS_H
                    } else {
                        STRIP_BEATS_H * 0.4
                    };
                    window.paint_quad(gpui::fill(
                        quad(ox + x * w, beats_top + STRIP_BEATS_H - h, 1.0, h),
                        if *is_down { down } else { tick },
                    ));
                }
                let chords_top = beats_top + STRIP_BEATS_H + 2.0;
                for (a, b, has) in &chord_spans {
                    if !has {
                        continue;
                    }
                    let r = quad(
                        ox + a * w + 0.5,
                        chords_top,
                        (b - a) * w - 1.0,
                        STRIP_CHORDS_H,
                    );
                    window.paint_quad(gpui::fill(r, chord_fill));
                    window.paint_quad(gpui::fill(
                        quad(ox + a * w, chords_top, 1.0, STRIP_CHORDS_H),
                        chord_edge,
                    ));
                }
            },
        )
        .absolute()
        .inset_0();

        // Labels, positioned by fraction so they need no measured width; each
        // stays inside its own section, dropping the unit (or itself) when
        // the section is too narrow to hold it.
        let section_labels = rhythm.sections.iter().filter_map(|s| {
            let (a, b) = (frac(s.start_seconds), frac(s.end_seconds));
            let span = (b - a) * width_px;
            let text = if span >= 64.0 {
                format!("{} BPM", format_bpm(s.bpm))
            } else if span >= 30.0 {
                format_bpm(s.bpm)
            } else {
                return None;
            };
            Some(
                div()
                    .absolute()
                    .top(px(space::HAIR))
                    .left(gpui::relative(a))
                    .w(gpui::relative(b - a))
                    .overflow_hidden()
                    .pl(px(space::TIGHT))
                    .text_size(px(typography::UI_XS))
                    .font_features(tabular_features())
                    .text_color(Colors::text_secondary())
                    .whitespace_nowrap()
                    .child(text)
                    .into_any_element(),
            )
        });
        let chord_labels = analysis.chords.iter().filter_map(|c| {
            let label = c.chord?;
            let (a, b) = (frac(c.start_seconds), frac(c.end_seconds));
            ((b - a) * width_px >= 26.0).then(|| {
                div()
                    .absolute()
                    .top(px(STRIP_TEMPO_H + STRIP_BEATS_H + 2.0))
                    .h(px(STRIP_CHORDS_H))
                    .left(gpui::relative(a))
                    .w(gpui::relative(b - a))
                    .flex()
                    .items_center()
                    .px(px(space::TIGHT))
                    .overflow_hidden()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .whitespace_nowrap()
                    .child(chord_name(label.root, label.kind, flats))
                    .into_any_element()
            })
        });
        Some(
            div()
                .flex_none()
                .px(px(space::SECTION))
                .pt(px(space::BASE))
                .child(
                    div()
                        .relative()
                        .h(px(STRIP_TEMPO_H + STRIP_BEATS_H + 2.0 + STRIP_CHORDS_H))
                        .rounded(px(radius::CONTROL))
                        .bg(Colors::surface_canvas())
                        .border(px(1.0))
                        .border_color(Colors::border_subtle())
                        .overflow_hidden()
                        .child(graphics)
                        .children(section_labels)
                        .children(chord_labels),
                )
                .into_any_element(),
        )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match (&self.phase, &self.notice) {
            (_, Some(notice)) => notice.clone(),
            (Phase::Analyzing, None) => "Analyzing…".to_string(),
            (Phase::Failed(error), None) => error.clone(),
            (Phase::Ready(_), None) => {
                "Map Tempo lays the project grid on the detected beats".to_string()
            }
        };
        let failed = (matches!(self.phase, Phase::Failed(_)) && self.notice.is_none())
            || (self.notice.is_some() && self.notice_error);
        let analyzing = matches!(self.phase, Phase::Analyzing);
        let picked = self.picked_tempo();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(if failed {
                        Colors::accent_danger()
                    } else {
                        Colors::text_muted()
                    })
                    .child(status),
            )
            .child(fb_button(
                "tempo-key-reanalyze",
                "Analyze Again",
                FbButtonKind::Ghost,
                !analyzing,
                cx.listener(|this, _, _, cx| this.analyze(cx)),
            ))
            .child(fb_button(
                "tempo-key-set-project",
                "Set Project Tempo",
                FbButtonKind::Primary,
                picked.is_some(),
                cx.listener(|this, _, _, cx| {
                    if let Some(tempo) = this.picked_tempo() {
                        let bpm = round_bpm(tempo.bpm);
                        this.dispatch(
                            TempoKeyCommand::SetProjectTempo { bpm },
                            format!("Project tempo set to {} BPM", format_bpm(bpm as f32)),
                            cx,
                        );
                    }
                }),
            ))
    }
}

impl Render for TempoKeyFinderWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pulse_enabled = self.picked_tempo().is_some();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(crate::theme::ui_font())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key.as_str() == "escape" {
                    this.close(window, cx);
                }
            }))
            .child(external_window_titlebar(
                "Find Tempo & Key",
                "tempo-key-close",
                {
                    let entity = cx.entity().downgrade();
                    move |window, cx| {
                        let _ = entity.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(self.header(cx))
            .children(self.rhythm_strip(window))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .gap(px(space::SECTION))
                    .p(px(space::SECTION))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(space::BASE))
                            .child(self.wheel(window, cx))
                            .child(fb_checkbox(
                                "tempo-key-pulse",
                                "Beat pulse",
                                self.pulse && pulse_enabled,
                                pulse_enabled,
                                cx.listener(|this, _, _, cx| {
                                    this.pulse = !this.pulse;
                                    this.ensure_ticking(cx);
                                    cx.notify();
                                }),
                            )),
                    )
                    .child(
                        div()
                            .id("tempo-key-cards")
                            .flex_1()
                            .min_w(px(0.0))
                            .h_full()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(space::LOOSE))
                            .child(self.tempo_card(cx))
                            .child(self.key_card(cx))
                            .child(self.chords_card(cx)),
                    ),
            )
            .child(self.footer(cx))
    }
}

/// Open the window for `target`, centred on the Studio.
pub fn open_tempo_key_finder_window(
    target: TempoKeyTarget,
    owner_bounds: Option<Bounds<Pixels>>,
    remembered: Option<Bounds<Pixels>>,
    callbacks: TempoKeyFinderCallbacks,
    cx: &mut App,
) -> Result<WindowHandle<TempoKeyFinderWindow>, String> {
    let window_size = size(px(TEMPO_KEY_WINDOW_WIDTH), px(TEMPO_KEY_WINDOW_HEIGHT));
    let bounds =
        remembered.unwrap_or_else(|| centered_window_bounds(owner_bounds, window_size, cx));
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(bounds));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(MIN_WIDTH), px(MIN_HEIGHT)));
    apply_owner_display(&mut options, owner_bounds, cx);
    cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| {
            let mut finder = TempoKeyFinderWindow::new(target, callbacks, cx);
            finder._activation = Some(cx.observe_window_activation(window, |this, window, cx| {
                this.set_window_active(window.is_window_active(), cx);
            }));
            finder
        });
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        view
    })
    .map_err(|e| e.to_string())
}

// ── Analysis (background executor) ───────────────────────────────────────────

fn run_analysis(path: &str, range: Option<(u64, u64)>) -> Result<Analysis, String> {
    let buffer = DirectAudio::load_audio_file_for_edit(path)?;
    let channels = buffer.channels.max(1);
    let sample_rate = buffer.sample_rate as f32;
    let samples = match range {
        Some((start, end)) => {
            let total = (buffer.samples.len() / channels) as u64;
            let end = if end == 0 { total } else { end.min(total) };
            slice_frames(&buffer.samples, channels, start as i64, end as i64)
        }
        None => buffer.samples,
    };
    let mono = downmix_interleaved(&samples, channels);
    let seconds = if sample_rate > 0.0 {
        mono.len() as f32 / sample_rate
    } else {
        0.0
    };
    if seconds < MIN_ANALYSIS_SECONDS {
        return Err("Not enough audio to analyze — needs at least 1 second".to_string());
    }
    let mut tempos = estimate_bpm_candidates(&mono, sample_rate, MIN_BPM, MAX_BPM);
    tempos.truncate(TEMPO_CHIPS);
    tempos.sort_by(|a, b| a.bpm.total_cmp(&b.bpm));
    let chroma = pitch_class_profile(&mono, sample_rate);
    let keys = chroma.as_ref().map(rank_keys).unwrap_or_default();
    if tempos.is_empty() && keys.is_empty() {
        return Err("No tempo or pitched content found".to_string());
    }
    let mut energy = chroma.unwrap_or([0.0; 12]);
    let peak = energy.iter().copied().fold(0.0f32, f32::max);
    if peak > 0.0 {
        for value in &mut energy {
            *value /= peak;
        }
    }
    // Beats, downbeats, meter and tempo sections, then chords on those beats.
    let frames = chroma_frames(&mono, sample_rate).map(Arc::new);
    let rhythm = analyze_rhythm(
        &mono,
        sample_rate,
        frames.as_deref(),
        RhythmOptions::default(),
    );
    let chords = match (&frames, &rhythm) {
        (Some(frames), Some(rhythm)) => detect_chords(frames, rhythm, true),
        _ => Vec::new(),
    };
    let offset_seconds = match range {
        Some((start, _)) if sample_rate > 0.0 => start as f64 / sample_rate as f64,
        _ => 0.0,
    };
    Ok(Analysis {
        tempos,
        keys,
        energy,
        seconds,
        offset_seconds,
        rhythm,
        chroma: frames,
        chords,
    })
}

fn detect_chords(
    frames: &ChromaFrames,
    rhythm: &RhythmAnalysis,
    sevenths: bool,
) -> Vec<ChordSegment> {
    let beats: Vec<f64> = rhythm.beats.iter().map(|b| b.seconds).collect();
    let downbeats: Vec<bool> = rhythm.beats.iter().map(|b| b.position == 1).collect();
    recognize_chords(frames, &beats, &downbeats, ChordOptions { sevenths })
}

/// Keys whose signature is written in flats, so chord names match it.
fn key_uses_flats(key: Option<&KeyEstimate>) -> bool {
    let Some(key) = key else {
        return false;
    };
    let tonic = tonic_index(key);
    if key.mode == KeyMode::Minor {
        matches!(tonic, 0 | 2 | 3 | 5 | 7 | 10)
    } else {
        matches!(tonic, 1 | 3 | 5 | 6 | 8 | 10)
    }
}

fn chord_name(root: u8, kind: ChordKind, flats: bool) -> String {
    const SHARPS: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    const FLATS: [&str; 12] = [
        "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
    ];
    let names = if flats { FLATS } else { SHARPS };
    let suffix = match kind {
        ChordKind::Major => "",
        ChordKind::Minor => "m",
        ChordKind::Dominant7 => "7",
        ChordKind::Major7 => "maj7",
        ChordKind::Minor7 => "m7",
    };
    format!("{}{}", names[root as usize % 12], suffix)
}

// ── Small pure helpers ───────────────────────────────────────────────────────

fn best_tempo_index(tempos: &[TempoCandidate]) -> usize {
    tempos
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.confidence.total_cmp(&b.1.confidence))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn tonic_index(key: &KeyEstimate) -> usize {
    (0..12)
        .find(|&pc| SphereAudioProcessor::PitchClass::from_index(pc as i32) == key.tonic)
        .unwrap_or(0)
}

/// Pitch classes of the key's major or natural-minor scale.
fn scale_members(key: &KeyEstimate) -> [bool; 12] {
    const MAJOR: [usize; 7] = [0, 2, 4, 5, 7, 9, 11];
    const MINOR: [usize; 7] = [0, 2, 3, 5, 7, 8, 10];
    let tonic = tonic_index(key);
    let steps = match key.mode {
        KeyMode::Major => MAJOR,
        KeyMode::Minor => MINOR,
    };
    let mut members = [false; 12];
    for step in steps {
        members[(tonic + step) % 12] = true;
    }
    members
}

fn relative_key_label(key: &KeyEstimate) -> String {
    let tonic = tonic_index(key);
    let (pc, mode) = match key.mode {
        KeyMode::Major => ((tonic + 9) % 12, "minor"),
        KeyMode::Minor => ((tonic + 3) % 12, "major"),
    };
    format!("{} {mode}", pitch_name(pc))
}

fn pitch_name(pc: usize) -> &'static str {
    SphereAudioProcessor::PitchClass::from_index(pc as i32).name()
}

fn round_bpm(bpm: f32) -> f64 {
    (bpm as f64 * 100.0).round() / 100.0
}

fn format_bpm(bpm: f32) -> String {
    if (bpm - bpm.round()).abs() < 0.05 {
        format!("{:.0}", bpm)
    } else {
        format!("{:.1}", bpm)
    }
}

/// The tempo estimator's confidence combines pulse strength with the winner's
/// margin over any competing (non-octave) tempo. A clear groove in a full mix
/// lands around 0.45; a click loop near 1; triplet-heavy or rubato material
/// under 0.2, where the other chips deserve a listen.
fn tempo_confidence(value: f32) -> (&'static str, Rgba) {
    if value >= 0.55 {
        ("High", Colors::accent_success())
    } else if value >= 0.3 {
        ("Medium", Colors::accent_warning())
    } else {
        ("Low", Colors::text_muted())
    }
}

/// The key estimator's confidence is the winner's correlation margin over the
/// runner-up, which is small even for a clear key.
fn key_confidence(value: f32) -> (&'static str, Rgba) {
    if value >= 0.12 {
        ("High", Colors::accent_success())
    } else if value >= 0.04 {
        ("Medium", Colors::accent_warning())
    } else {
        ("Low", Colors::text_muted())
    }
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 0 | 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (i, n) if i + 1 == n => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

fn tabular_features() -> FontFeatures {
    static FEATURES: std::sync::OnceLock<FontFeatures> = std::sync::OnceLock::new();
    FEATURES
        .get_or_init(|| FontFeatures(Arc::new(vec![("tnum".to_string(), 1)])))
        .clone()
}

fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
}

fn card_heading(label: &'static str, confidence: Option<(&'static str, Rgba)>) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(fb_section_label(label))
        .children(confidence.map(|(word, tone)| fb_badge(format!("{word} confidence"), tone)))
        .into_any_element()
}

fn value_line(value: String, unit: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(space::TIGHT))
        .child(
            div()
                .text_size(px(22.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .font_features(tabular_features())
                .text_color(Colors::text_primary())
                .child(value),
        )
        .when(!unit.is_empty(), |this| {
            this.child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(unit),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use SphereAudioProcessor::PitchClass;

    fn key(tonic: PitchClass, mode: KeyMode) -> KeyEstimate {
        KeyEstimate {
            tonic,
            mode,
            confidence: 0.1,
        }
    }

    #[test]
    fn scale_members_follow_mode() {
        let a_minor = scale_members(&key(PitchClass::A, KeyMode::Minor));
        let c_major = scale_members(&key(PitchClass::C, KeyMode::Major));
        // Relative keys share every note.
        assert_eq!(a_minor, c_major);
        assert!(c_major[0] && c_major[4] && c_major[7]);
        assert!(!c_major[1] && !c_major[6]);
    }

    #[test]
    fn relative_keys_are_a_minor_third_apart() {
        assert_eq!(
            relative_key_label(&key(PitchClass::A, KeyMode::Minor)),
            "C major"
        );
        assert_eq!(
            relative_key_label(&key(PitchClass::C, KeyMode::Major)),
            "A minor"
        );
        assert_eq!(
            relative_key_label(&key(PitchClass::Fs, KeyMode::Major)),
            "D# minor"
        );
    }

    #[test]
    fn best_tempo_is_the_most_confident_not_the_first() {
        let tempos = vec![
            TempoCandidate {
                bpm: 64.0,
                confidence: 0.4,
            },
            TempoCandidate {
                bpm: 128.0,
                confidence: 0.7,
            },
        ];
        assert_eq!(best_tempo_index(&tempos), 1);
    }

    #[test]
    fn segment_positions_round_only_the_outer_corners() {
        assert_eq!(segment_position(0, 1), FbSegment::Only);
        assert_eq!(segment_position(0, 3), FbSegment::First);
        assert_eq!(segment_position(1, 3), FbSegment::Middle);
        assert_eq!(segment_position(2, 3), FbSegment::Last);
    }

    #[test]
    fn short_or_missing_audio_reports_instead_of_guessing() {
        let error = run_analysis("/definitely/not/here.wav", None).unwrap_err();
        assert!(!error.is_empty());
    }
}
