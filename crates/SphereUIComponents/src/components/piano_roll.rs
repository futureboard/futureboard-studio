//! Native GPUI Piano Roll editor.
//!
//! Ported from the WebUI `MidiEditorPanel`. Edits the currently selected MIDI
//! clip in the shared [`Timeline`] entity. All note mutations go through the
//! single-source-of-truth helpers on `TimelineState` (see
//! `timeline_state.rs`) and mark the project dirty so the engine sync /
//! autosave observe the change.
//!
//! Coordinate model (matches WebUI):
//! - notes are stored in beats relative to the clip start
//! - the grid maps beats → x with `ppb` (pixels per beat) and a horizontal
//!   scroll offset; pitch → y with independent `row_h` (px/semitone) and a
//!   vertical scroll. Zoom X (`ppb`) and Zoom Y (`row_h`) are independent.
//!
//! Interaction state (tool, selection, zoom, snap, drag) lives on this entity
//! — never recomputed in render. Note geometry is only built for the visible
//! pitch/beat range each frame.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::SharedString;
use gpui::{
    canvas, deferred, div, fill, point, px, size, svg, Animation, AnimationExt, AppContext, Bounds,
    Context, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

use crate::assets;
use crate::components::edit::{normalize_range, EditCommand};
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::{
    midi_debug_enabled, ArticulationId, ChordInsertKind, GridStep, MeterBars, MeterGrid,
    MidiArticulationEvent, MidiChannel, MidiChannelMask, MidiControllerKind, MidiControllerPoint,
    MidiNoteState, PitchCurve, PitchTransformContext, ScaleKind, ScaleRoot, StretchAnchor,
    StretchSpec, TimelineState, MIN_NOTE_BEATS,
};
use crate::theme::Colors;
use sphere_midi_service::{NoteExpression, NoteExpressionLane};

// ── Layout constants (CSS px) ───────────────────────────────────────────────
pub mod advanced_tools;
mod articulation_lane;
mod cc_lane;
mod cc_lane_render;
mod drum_map;
mod grid_render;
mod native_menus;
pub mod playhead;
mod render;
pub mod scope;
pub use drum_map::DrumLanes;
mod stretch;
mod tools_panel;

/// Default note-row height (px per semitone). Reset-zoom restores this value so
/// note height matches the editor's default density after Zoom Y changes.
const DEFAULT_ROW_H: f32 = 14.0;
/// Row height drum mode opens at: a lane carries a name and a hit, and is
/// read across a bar, so it gets more room than a semitone.
const DRUM_ROW_H: f32 = 22.0;
/// Width of the drum lane list: room for "Closed Hi-Hat" and its note.
const DRUM_LANE_W: f32 = 132.0;
/// Legacy alias used by unit tests and denser-zoom comparisons that still mean
/// "the default row height".
const ROW_H: f32 = DEFAULT_ROW_H;
/// Number of MIDI pitch rows the editor spans (0..=127). Public so a surface
/// sharing [`PianoRollViewport`] can derive the same vertical extent.
pub const PITCH_COUNT: i32 = 128;
const PITCH_CNT: i32 = PITCH_COUNT;
/// Vertical zoom clamps (px/semitone). Floor keeps notes hittable.
///
/// The ceiling used to be 48px — plenty for note editing, where a semitone is
/// the finest grid that matters. The Solfege Pitch tab shares this exact
/// field for its continuous cents axis, though, and at 48px/semitone a cent
/// is under half a pixel: dragging a breakpoint to a specific cent value was
/// unworkable, and the Pitch tab's own Zoom In button hit this ceiling after
/// a handful of clicks with no further room to zoom. 200px/semitone (2
/// px/cent) is comfortable for that without changing anything for a MIDI-tab
/// user who never zooms this deep.
const PIANO_ROLL_MIN_ROW_H: f32 = 6.0;
const PIANO_ROLL_MAX_ROW_H: f32 = 200.0;
/// Horizontal overview zoom floor for long MIDI clips. 1 px/beat lets a
/// 200-bar/800-beat song fit in an ~800px editor while preserving interactions.
const PIANO_ROLL_MIN_PPB: f32 = 1.0;
const PIANO_ROLL_MAX_PPB: f32 = 400.0;
/// Paint priority for the toolbar's deferred dropdown menus (Grid / Scale /
/// Lane). Without `deferred()`, an absolutely-positioned panel still paints
/// in normal tree order and gets covered by the piano roll body (keys/grid),
/// which is a later sibling with an opaque background. Matches the priority
/// used by other popovers in this crate (e.g. `SELECT_MENU_PRIORITY`).
const PIANO_ROLL_MENU_PRIORITY: usize = 100;
const PIANO_ROLL_FIT_PAD_PX: f32 = 48.0;

/// Reversed-pitch row mapping shared by the note grid and the left piano-key
/// lane: higher pitches sit at the top, lower at the bottom, offset by the
/// vertical scroll, clamped to the valid MIDI range. This is the single source
/// of truth for "which row is which pitch" — the keys and the grid both go
/// through it (via [`PianoRoll::y_to_pitch`]) so they can never drift apart.
/// Out-of-lane detection is layered on top in [`PianoRoll::key_lane_pitch_at`].
/// `row_h` is the current Zoom Y (px/semitone).
fn local_y_to_pitch(local_y: f32, scroll_y: f32, row_h: f32) -> u8 {
    let row_h = row_h.max(0.0001);
    let row = ((local_y + scroll_y) / row_h).floor() as i32;
    (PITCH_CNT - 1 - row).clamp(0, PITCH_CNT - 1) as u8
}

/// Fractional-pitch inverse of the vertical transform. The integer
/// [`local_y_to_pitch`] snaps to a note row; the Pitch editor needs the
/// continuous value, so both live here against the same constants.
fn local_y_to_pitch_f32(local_y: f32, scroll_y: f32, row_h: f32) -> f32 {
    let row_h = row_h.max(0.0001);
    (PITCH_CNT - 1) as f32 - (local_y + scroll_y - row_h * 0.5) / row_h
}

fn clamp_velocity(value: i32) -> u8 {
    value.clamp(1, 127) as u8
}

fn relative_velocity(original: u8, delta: i32) -> u8 {
    clamp_velocity(original as i32 + delta)
}

fn velocity_drag_delta(start_y: f32, current_y: f32, lane_h: f32, fine: bool) -> i32 {
    let usable_h = (lane_h - 8.0).max(1.0);
    let scale = if fine { 0.2 } else { 1.0 };
    (-(current_y - start_y) * 126.0 / usable_h * scale).round() as i32
}

fn interpolate_velocity(from: u8, to: u8, t: f32, curve: VelocityCurve) -> u8 {
    let shaped = curve.sample(t);
    clamp_velocity((from as f32 + (to as f32 - from as f32) * shaped).round() as i32)
}

/// One label + tick position on the bar/beat ruler.
///
/// Produced by [`PianoRollViewport::ruler_marks`] so the piano roll's ruler and
/// the Solfege Pitch tab's ruler are the same marks at the same pixels —
/// styling stays with each renderer, the math does not.
#[derive(Debug, Clone, PartialEq)]
pub struct RulerMark {
    /// Local x in the grid's coordinate space.
    pub x: f32,
    /// Clip-local beat this mark sits on.
    pub beat: f32,
    /// Formatted label: bar number alone when labelling bars, `bar.beat` when
    /// zoomed in far enough to label every beat.
    pub label: String,
    /// `true` when this mark falls on a bar line.
    pub on_bar: bool,
}

/// Strength tier of a vertical timing gridline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridLineKind {
    Bar,
    Beat,
    Subdivision,
}

impl GridLineKind {
    /// Colour the MIDI editor paints this tier in. The arrangement's grid
    /// tokens, so a bar line reads the same weight in the editor as under the
    /// clip it belongs to, and follows the theme. Shared so every surface in
    /// the editor draws the same grid.
    pub fn color(self) -> gpui::Rgba {
        match self {
            Self::Bar => Colors::timeline_grid_bar(),
            Self::Beat => Colors::timeline_grid_major(),
            Self::Subdivision => Colors::timeline_grid_minor(),
        }
    }
}

/// One stretch of constant meter on the editor's axis, in project beats
/// (quarter notes).
#[derive(Debug, Clone, Copy, PartialEq)]
struct MeterSegment {
    start: f32,
    /// First beat of the next meter; `f32::INFINITY` for the last one.
    end: f32,
    /// Bar length in quarter beats (`6/8` → 3.0, `7/8` → 3.5).
    bar_beats: f32,
    /// One counted beat in quarter beats (the denominator: `/8` → 0.5).
    beat_unit: f32,
    /// Counted beats per bar (the numerator).
    beats: u16,
    /// Song bar number of this segment's first bar (1-based).
    first_bar: i64,
}

/// The project's meter as the MIDI editor's grid and ruler see it.
///
/// The grid used to take a single beats-per-bar, sampled at the playhead and
/// applied from beat 0. Any meter change put every bar line after it in the
/// wrong place — and moved them all when the playhead crossed the change.
/// This carries the whole time-signature map instead, numbered the way
/// [`TimeSignatureMap::bar_beat_at_beat`] numbers bars, so the ruler, the
/// arrangement and the editor agree on which bar is which.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorMeter {
    segments: Vec<MeterSegment>,
}

impl EditorMeter {
    /// One meter for the whole axis: `bar_beats` quarter beats per bar,
    /// counted in quarters.
    pub fn constant(bar_beats: f32) -> Self {
        let bar_beats = bar_beats.max(0.25);
        Self {
            segments: vec![MeterSegment {
                start: 0.0,
                end: f32::INFINITY,
                bar_beats,
                beat_unit: 1.0,
                beats: bar_beats.ceil().max(1.0) as u16,
                first_bar: 1,
            }],
        }
    }

    pub fn from_map(map: &crate::components::timeline::timeline_state::TimeSignatureMap) -> Self {
        use crate::components::timeline::timeline_state::{
            beats_per_bar_from_sig, denominator_unit_quarter_beats,
        };
        let mut points: Vec<(f64, u16, u16)> = map
            .points
            .iter()
            .map(|p| (p.beat.max(0.0), p.numerator.max(1), p.denominator))
            .collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        if points.is_empty() {
            return Self::constant(4.0);
        }
        let mut segments = Vec::with_capacity(points.len());
        let mut first_bar: i64 = 1;
        for (i, &(beat, numerator, denominator)) in points.iter().enumerate() {
            // The first meter reaches back to the song start: nothing precedes
            // it, and `bar_beat_at_beat` counts that stretch as its bar 1 too.
            let start = if i == 0 { 0.0 } else { beat as f32 };
            let end = points
                .get(i + 1)
                .map(|p| p.0 as f32)
                .unwrap_or(f32::INFINITY);
            let bar_beats = (beats_per_bar_from_sig(numerator, denominator) as f32).max(0.25);
            segments.push(MeterSegment {
                start,
                end,
                bar_beats,
                beat_unit: denominator_unit_quarter_beats(denominator) as f32,
                beats: numerator,
                first_bar,
            });
            if end.is_finite() {
                first_bar += ((end - start) / bar_beats).floor().max(0.0) as i64;
            }
        }
        Self { segments }
    }

    /// `bar.beat` of a project beat, numbered the way the ruler numbers bars
    /// (1-based, counting through every meter change). Before the song start
    /// reads as its first beat.
    pub fn bar_beat_label(&self, beat: f32) -> String {
        let beat = beat.max(0.0);
        let Some(seg) = self.segment_at(beat) else {
            return "1.1".to_string();
        };
        let into = (beat - seg.start).max(0.0);
        let bar = seg.first_bar + (into / seg.bar_beats).floor() as i64;
        let in_bar = into - (into / seg.bar_beats).floor() * seg.bar_beats;
        let counted = (in_bar / seg.beat_unit.max(0.0001)).floor() as i64 + 1;
        format!("{bar}.{}", counted.clamp(1, seg.beats.max(1) as i64))
    }

    /// The meter in force at project `beat` (the first one before the song
    /// start).
    fn segment_at(&self, beat: f32) -> Option<&MeterSegment> {
        self.segments
            .iter()
            .find(|seg| beat >= seg.start && beat < seg.end)
            .or(self.segments.last())
    }

    /// Length of a bar, in quarter beats, at project `beat`: what the "1 Bar"
    /// grid steps by there (`3/4` → 3, `6/8` → 3, `7/8` → 3.5).
    pub fn bar_beats_at(&self, beat: f32) -> f32 {
        self.segment_at(beat.max(0.0))
            .map_or(4.0, |seg| seg.bar_beats)
    }

    /// The bar line nearest project `beat` — the lines [`PianoRollViewport::grid_lines_in`]
    /// draws: bars count from each meter's start, and a meter change is a bar
    /// line of its own even when the previous meter's last bar is cut short.
    pub fn snap_to_bar(&self, beat: f32) -> f32 {
        MeterGrid::new(self, GridStep::Bar, 0.0).snap(f64::from(beat.max(0.0))) as f32
    }

    /// Segments overlapping `[start, end]`.
    fn overlapping(&self, start: f32, end: f32) -> impl Iterator<Item = &MeterSegment> {
        self.segments
            .iter()
            .filter(move |seg| seg.end > start && seg.start <= end)
    }
}

/// The editor's bars for the shared meter-aware grid ([`MeterGrid`]) — the
/// bars [`PianoRollViewport::grid_lines_in`] draws.
impl MeterBars for EditorMeter {
    fn bar_span(&self, beat: f64) -> (f64, f64) {
        let beat = beat.max(0.0) as f32;
        let Some(seg) = self.segment_at(beat) else {
            return (0.0, 4.0);
        };
        let bar = seg.bar_beats.max(0.25);
        let start = seg.start + ((beat - seg.start) / bar).floor() * bar;
        let end = (start + bar).min(seg.end);
        (f64::from(start), f64::from(end))
    }
}

/// Smallest bar step (1, 2, 4, …) that keeps bars at least `min_px` apart.
fn thinned_bar_step(bar_px: f32, min_px: f32) -> i64 {
    let mut step: i64 = 1;
    while (step as f32) * bar_px < min_px && step < 256 {
        step *= 2;
    }
    step
}

/// The shared transform of the MIDI editor, handed to surfaces that must stay
/// aligned with the piano roll (Solfege performance lanes, the Pitch tab)
/// without owning a second scroll position.
///
/// Both axes are here. The Pitch tab shares the *vertical* axis too, so a note
/// sits on the same screen row in both tabs and scrolling one scrolls the
/// other — there is exactly one coordinate transform for the whole editor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PianoRollViewport {
    /// Pixels per beat (Zoom X).
    pub ppb: f32,
    /// Horizontal scroll offset in pixels.
    pub scroll_x: f32,
    /// Pixels per semitone (Zoom Y).
    pub row_h: f32,
    /// Vertical scroll offset in pixels.
    pub scroll_y: f32,
    /// Width of the left piano-key gutter, so a lane can align its own header.
    pub key_lane_width: f32,
    /// Current snap step in beats; `0.0` when snapping is off.
    pub snap_step_beats: f32,
    /// Snapping is on with the "1 Bar" grid: snap to the meter's bar lines
    /// ([`Self::snap_in_clip`]), which `snap_step_beats` — the bar length at
    /// the clip start — cannot describe after a meter change.
    pub snap_to_bars: bool,
    /// Grid subdivision in beats, independent of the snap toggle — the grid
    /// stays visible when snapping is off.
    pub sub_step_beats: f32,
    /// Width of the note grid in pixels at the last paint.
    pub grid_width: f32,
    /// Height of the note grid in pixels at the last paint.
    pub grid_height: f32,
}

impl PianoRollViewport {
    #[inline]
    pub fn beat_to_x(&self, beat: f32) -> f32 {
        beat_to_local_x(beat, self.ppb, self.scroll_x)
    }

    #[inline]
    pub fn x_to_beat(&self, local_x: f32) -> f32 {
        local_x_to_beat(local_x, self.ppb, self.scroll_x)
    }

    /// Snap `beat` to the shared grid. Honors the piano roll's snap toggle, so
    /// a lane and the notes above it always land on the same divisions.
    #[inline]
    pub fn snap(&self, beat: f32) -> f32 {
        snap_beat_to_step(beat, self.snap_step_beats)
    }

    /// This transform on the song axis, for a surface that lays out the
    /// clip-local beats of a clip starting `origin` beats into the song (the
    /// Solfege lanes and Pitch tab): song beat `s` lands at the x of clip beat
    /// `s - origin`. Grid lines and ruler marks come from the song's meter
    /// through it, so the bar lines drawn there are the ones
    /// [`Self::snap_in_clip`] snaps to (#263).
    pub fn song_frame_for_clip(&self, origin: f32) -> Self {
        Self {
            scroll_x: self.scroll_x + origin * self.ppb,
            ..*self
        }
    }

    /// Snap clip-local `beat` of a clip that starts `origin` beats into the
    /// song to the grid the notes snap to. On the "1 Bar" grid that is the
    /// bar lines of `meter` — the ones drawn, through every meter change and
    /// in a clip that starts off a bar line (#263); a fixed step is
    /// [`Self::snap`]. Honors the snap toggle like [`Self::snap`].
    pub fn snap_in_clip(&self, beat: f32, meter: &EditorMeter, origin: f32) -> f32 {
        if !self.snap_to_bars {
            return self.snap(beat);
        }
        let grid = MeterGrid::new(meter, GridStep::Bar, f64::from(origin));
        (grid.snap(f64::from(beat)) as f32).max(0.0)
    }

    /// First and last beat visible in `width` pixels of lane.
    pub fn visible_beats(&self, width: f32) -> (f32, f32) {
        (self.x_to_beat(0.0), self.x_to_beat(width.max(1.0)))
    }

    /// Top edge of a pitch row. Matches the piano roll's own note placement.
    #[inline]
    pub fn pitch_row_top(&self, pitch: f32) -> f32 {
        (PITCH_COUNT - 1) as f32 * self.row_h - pitch * self.row_h - self.scroll_y
    }

    /// Centre line of a pitch row. This is where a *continuous* pitch of
    /// exactly `pitch` sits — the Pitch tab draws its trajectory against this,
    /// so a flat note lands on the middle of its own row.
    #[inline]
    pub fn pitch_center_y(&self, pitch: f32) -> f32 {
        self.pitch_row_top(pitch) + self.row_h * 0.5
    }

    /// Inverse of [`Self::pitch_center_y`], returning a *fractional* pitch —
    /// the pitch editor never quantizes the pointer to a semitone.
    #[inline]
    pub fn y_to_pitch_continuous(&self, local_y: f32) -> f32 {
        let row_h = self.row_h.max(0.0001);
        (PITCH_COUNT - 1) as f32 - (local_y + self.scroll_y - row_h * 0.5) / row_h
    }

    /// Inclusive semitone range covering `height` pixels, padded by one row so
    /// partially visible rows still paint.
    pub fn visible_pitches(&self, height: f32) -> (i32, i32) {
        let low = self.y_to_pitch_continuous(height.max(1.0)).floor() as i32 - 1;
        let high = self.y_to_pitch_continuous(0.0).ceil() as i32 + 1;
        (low.max(0), high.min(PITCH_COUNT - 1))
    }

    /// Bar/beat marks across `[start_beat, end_beat]` in one constant meter.
    /// See [`Self::ruler_marks_in`].
    pub fn ruler_marks(&self, start_beat: f32, end_beat: f32, bpb: f32) -> Vec<RulerMark> {
        self.ruler_marks_in(start_beat, end_beat, &EditorMeter::constant(bpb))
    }

    /// Bar/beat marks across `[start_beat, end_beat]`, following the meter.
    ///
    /// Labels each counted beat once a beat is 36 px wide, otherwise labels
    /// bar starts, doubling the bar step until labels are at least 56 px
    /// apart. The single source of ruler geometry for the whole MIDI editor.
    pub fn ruler_marks_in(
        &self,
        start_beat: f32,
        end_beat: f32,
        meter: &EditorMeter,
    ) -> Vec<RulerMark> {
        let ppb = self.ppb.max(0.0001);
        let start_beat = start_beat.max(0.0);
        let mut out = Vec::new();
        for seg in meter.overlapping(start_beat, end_beat) {
            let label_beats = seg.beat_unit * ppb >= 36.0;
            let bar_step = if label_beats {
                1
            } else {
                thinned_bar_step(seg.bar_beats * ppb, 56.0)
            };
            let from = ((start_beat.max(seg.start) - seg.start) / seg.bar_beats).floor() as i64;
            let to = ((end_beat.min(seg.end) - seg.start) / seg.bar_beats).ceil() as i64;
            for i in from.max(0)..=to.max(0) {
                let bar_start = seg.start + i as f32 * seg.bar_beats;
                if bar_start >= seg.end - 1.0e-4 || out.len() >= 2000 {
                    break;
                }
                let bar = seg.first_bar + i;
                if label_beats {
                    for k in 0..seg.beats {
                        let b = bar_start + f32::from(k) * seg.beat_unit;
                        if b >= seg.end - 1.0e-4 || b > end_beat + seg.bar_beats {
                            break;
                        }
                        out.push(RulerMark {
                            x: self.beat_to_x(b),
                            beat: b,
                            label: format!("{bar}.{}", k + 1),
                            on_bar: k == 0,
                        });
                    }
                } else if (bar - 1).rem_euclid(bar_step) == 0 {
                    out.push(RulerMark {
                        x: self.beat_to_x(bar_start),
                        beat: bar_start,
                        label: format!("{bar}"),
                        on_bar: true,
                    });
                }
            }
        }
        out
    }

    /// Vertical grid lines in one constant meter. See [`Self::grid_lines_in`].
    pub fn grid_lines(&self, start_beat: f32, end_beat: f32, bpb: f32) -> Vec<(f32, GridLineKind)> {
        self.grid_lines_in(start_beat, end_beat, &EditorMeter::constant(bpb))
    }

    /// Vertical grid lines across `[start_beat, end_beat]`, following the
    /// meter and thinned by zoom: bars always (every 2nd/4th… when they would
    /// sit closer than 18 px), counted beats from 10 px apart, and the grid
    /// subdivision from 7 px apart. Subdivisions count from each bar's start,
    /// so an odd meter's grid lands on its own downbeats.
    ///
    /// The single source of grid geometry for the whole MIDI editor.
    pub fn grid_lines_in(
        &self,
        start_beat: f32,
        end_beat: f32,
        meter: &EditorMeter,
    ) -> Vec<(f32, GridLineKind)> {
        const MAX_LINES: usize = 8000;
        let ppb = self.ppb.max(0.0001);
        let sub_step = self.sub_step_beats.max(1.0 / 32.0);
        let start_beat = start_beat.max(0.0);
        let mut out = Vec::new();
        for seg in meter.overlapping(start_beat, end_beat) {
            let bar_step = thinned_bar_step(seg.bar_beats * ppb, 18.0);
            let show_beats = seg.beat_unit * ppb >= 10.0;
            let show_subs = show_beats && sub_step * ppb >= 7.0 && ppb >= 24.0;
            let from = ((start_beat.max(seg.start) - seg.start) / seg.bar_beats).floor() as i64;
            let to = ((end_beat.min(seg.end) - seg.start) / seg.bar_beats).ceil() as i64;
            for i in from.max(0)..=to.max(0) {
                let bar_start = seg.start + i as f32 * seg.bar_beats;
                if bar_start >= seg.end - 1.0e-4 || out.len() >= MAX_LINES {
                    break;
                }
                let bar = seg.first_bar + i;
                if (bar - 1).rem_euclid(bar_step) == 0 {
                    out.push((self.beat_to_x(bar_start), GridLineKind::Bar));
                }
                if !show_beats {
                    continue;
                }
                let bar_end = (bar_start + seg.bar_beats).min(seg.end);
                for k in 1..seg.beats {
                    let b = bar_start + f32::from(k) * seg.beat_unit;
                    if b >= bar_end - 1.0e-4 {
                        break;
                    }
                    out.push((self.beat_to_x(b), GridLineKind::Beat));
                }
                if !show_subs {
                    continue;
                }
                let mut j = 1;
                loop {
                    let offset = j as f32 * sub_step;
                    let b = bar_start + offset;
                    if b >= bar_end - 1.0e-4 || out.len() >= MAX_LINES {
                        break;
                    }
                    if !is_multiple(offset, seg.beat_unit) {
                        out.push((self.beat_to_x(b), GridLineKind::Subdivision));
                    }
                    j += 1;
                }
            }
        }
        out
    }
}

fn local_x_to_beat(local_x: f32, pixels_per_beat: f32, scroll_x: f32) -> f32 {
    ((local_x + scroll_x) / pixels_per_beat.max(0.0001)).max(0.0)
}

fn beat_to_local_x(beat: f32, pixels_per_beat: f32, scroll_x: f32) -> f32 {
    beat * pixels_per_beat.max(0.0001) - scroll_x
}

/// Pointer travel below which a press on a note is a click, not a move.
const NOTE_CLICK_SLOP_PX: f32 = 3.0;

/// A pointer position in content space: the project beat (unclamped) and the
/// fractional pitch row under it. Unlike window or grid-local pixels this does
/// not change when the view scrolls or zooms, so a note drag measured between
/// two of these keeps the note under a cursor that is held still while
/// follow-playhead or the wheel moves the content.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DragGrab {
    beat: f32,
    row: f32,
}

impl DragGrab {
    fn at(local_x: f32, local_y: f32, scroll_x: f32, scroll_y: f32, ppb: f32, row_h: f32) -> Self {
        Self {
            beat: (local_x + scroll_x) / ppb.max(0.0001),
            row: (local_y + scroll_y) / row_h.max(0.0001),
        }
    }

    /// `(beats, rows)` from `self` to `now`; rows count up, as `dpitch` does.
    fn delta_to(self, now: DragGrab) -> (f32, i32) {
        (now.beat - self.beat, -((now.row - self.row).round() as i32))
    }
}

/// How far the *pointer* has travelled since a note press, measured in window
/// pixels from the press point — the click-vs-drag test (#242).
///
/// Content space ([`DragGrab`]) is the wrong ruler for this: follow-playhead
/// and the wheel move the content under a pointer that is held still, so a
/// still click measured there turned into a move or a resize. Pointer travel
/// only changes when the mouse moves. Once it passes [`NOTE_CLICK_SLOP_PX`]
/// the press is a drag for good, and from then on the drag is measured in
/// content space so the note follows the content under the cursor (#184).
#[derive(Debug, Clone, Copy, PartialEq)]
struct PointerTravel {
    /// Window position of the press.
    press: (f32, f32),
    /// The pointer has moved past the slop since the press.
    dragging: bool,
}

impl PointerTravel {
    fn new(press: (f32, f32)) -> Self {
        Self {
            press,
            dragging: false,
        }
    }

    /// Record a pointer position (window pixels). Returns whether the press
    /// is now a drag; that latches.
    fn track(&mut self, pointer: (f32, f32)) -> bool {
        let (dx, dy) = (pointer.0 - self.press.0, pointer.1 - self.press.1);
        self.dragging = self.dragging || dx.hypot(dy) >= NOTE_CLICK_SLOP_PX;
        self.dragging
    }
}

/// The live `(beats, rows)` of a Move/Resize drag: nothing until the pointer
/// itself has moved, however far the view scrolled under it; after that the
/// content-space delta from the grab to the content now under the pointer.
fn note_drag_delta(travel: PointerTravel, grab: DragGrab, now: DragGrab) -> (f32, i32) {
    if travel.dragging {
        grab.delta_to(now)
    } else {
        (0.0, 0)
    }
}

/// The live length of a Resize drag: the grabbed note's original length until
/// the pointer has moved — a press is not a request to re-snap an off-grid
/// end — then the snapped edge under the pointer.
fn resize_drag_duration(
    travel: PointerTravel,
    grab: DragGrab,
    now: DragGrab,
    anchor_start: f32,
    prev_dur: f32,
    snap: impl Fn(f32) -> f32,
) -> f32 {
    if !travel.dragging {
        return prev_dur;
    }
    let (d_beats, _) = note_drag_delta(travel, grab, now);
    let edge = snap(anchor_start + prev_dur + d_beats);
    (edge - anchor_start).max(MIN_NOTE_BEATS)
}

/// Whether releasing a note Move is a click — judged on pointer travel only.
/// A click commits nothing and records no undo step; it only applies a
/// deselect-on-click.
fn move_release_is_click(travel: PointerTravel) -> bool {
    !travel.dragging
}

fn snap_beat_to_step(beat: f32, step: f32) -> f32 {
    if step <= 0.0 {
        beat.max(0.0)
    } else {
        ((beat / step).round() * step).max(0.0)
    }
}

/// Adaptive grid step from Zoom X: coarser when zoomed out, finer when zoomed
/// in. Mirrors arrangement `get_grid_sub_beats` thresholds.
fn adaptive_step_beats(ppb: f32) -> f32 {
    let ppb = ppb.max(0.0001);
    if ppb < 8.0 {
        4.0
    } else if ppb < 16.0 {
        2.0
    } else if ppb < 32.0 {
        1.0
    } else if ppb < 64.0 {
        0.5
    } else if ppb < 128.0 {
        0.25
    } else if ppb < 256.0 {
        0.125
    } else {
        0.0625
    }
}

/// Grid step in beats for `grid_res` at Zoom X `ppb`, or `0.0` for `Free`.
///
/// The single source of the editor's grid resolution: the notes
/// ([`PianoRoll::step_beats`]) and the viewport handed to the Solfege lanes and
/// the Pitch tab ([`viewport_snap_step`]) both resolve through here, so a lane
/// breakpoint and the note directly above it cannot land on different
/// divisions.
///
/// `bar_beats` is the bar length the "1 Bar" grid steps by — the meter's, not
/// a fixed four quarters (see [`EditorMeter::bar_beats_at`]).
fn grid_step_beats(grid_res: GridRes, ppb: f32, bar_beats: f32) -> f32 {
    match grid_res {
        GridRes::Free => 0.0,
        GridRes::Adaptive => adaptive_step_beats(ppb),
        GridRes::Bar => bar_beats.max(MIN_NOTE_BEATS),
        other => other.fixed_beats(),
    }
}

/// Snap step published on [`PianoRollViewport`]. `0.0` when snapping is off or
/// the grid is `Free` — the same two conditions [`PianoRoll::snap_beats`]
/// treats as "do not snap".
fn viewport_snap_step(snap_on: bool, grid_res: GridRes, ppb: f32, bar_beats: f32) -> f32 {
    if !snap_on || grid_res.is_free() {
        return 0.0;
    }
    grid_step_beats(grid_res, ppb, bar_beats).max(MIN_NOTE_BEATS)
}

/// The note grid for `grid_res` at Zoom X `ppb` as the shared meter-aware
/// grid ([`MeterGrid`]), in the frame of a clip that starts `origin` beats
/// into the song. `None` for `Free`, which has no lines.
///
/// The one grid of the MIDI editor: notes snap through it, the controller
/// lanes snap and walk through it, and Duplicate measures through it — and
/// the arrangement's automation lanes use the same [`MeterGrid`] on the song
/// axis — so none of them can drift from the bar lines drawn after a meter
/// change.
fn note_grid(
    meter: &EditorMeter,
    grid_res: GridRes,
    ppb: f32,
    origin: f32,
) -> Option<MeterGrid<'_, EditorMeter>> {
    let step = match grid_res {
        GridRes::Free => return None,
        GridRes::Bar => GridStep::Bar,
        other => GridStep::Beats(f64::from(
            grid_step_beats(other, ppb, meter.bar_beats_at(origin)).max(MIN_NOTE_BEATS),
        )),
    };
    Some(MeterGrid::new(meter, step, f64::from(origin)))
}

/// How far Duplicate moves the copies of a selection spanning clip beats
/// `[start, end)`: [`MeterGrid::duplicate_offset`] on the note grid, or the
/// exact span when there is no grid (snapping off, `Free`, or Shift).
fn duplicate_offset(grid: Option<MeterGrid<'_, EditorMeter>>, start: f32, end: f32) -> f32 {
    let span = (end - start).max(MIN_NOTE_BEATS);
    match grid {
        None => span,
        // A length, not a position: snapping the span as if it were a clip
        // beat put "1 Bar" copies of a clip that starts off a bar line onto
        // the nearest bar line instead of one bar on (#237).
        Some(grid) => (grid.duplicate_offset(f64::from(start), f64::from(start + span)) as f32)
            .max(MIN_NOTE_BEATS),
    }
}

/// Length of a note drawn over clip beats `[lo, hi]`: whole grid steps from
/// `lo`, at least one, measured the way Duplicate measures
/// ([`duplicate_offset`]) — so a "1 Bar" note is as long as the bar the meter
/// makes it where the note starts, not the bar at the clip start (#262).
/// With no grid (snapping off, `Free`, or Shift) the drawn span as it is.
fn drawn_note_length(grid: Option<MeterGrid<'_, EditorMeter>>, lo: f32, hi: f32) -> f32 {
    duplicate_offset(grid, lo, hi)
}

/// Where a drum hit clicked at clip beat `beat` lands: the start of the grid
/// cell it was clicked in, the way a step sequencer fills a cell. On the
/// "1 Bar" grid the cell is the bar drawn there (#262). With no grid, where
/// it was clicked.
fn drum_cell_start(grid: Option<MeterGrid<'_, EditorMeter>>, beat: f32) -> f32 {
    match grid {
        Some(grid) => (grid.line_at_or_before(f64::from(beat)) as f32).max(0.0),
        None => beat.max(0.0),
    }
}

/// Where a note starting at clip beat `beat` goes when nudged one grid step
/// (`forward` or back). A fixed step moves it by the step, keeping its place
/// between lines. The "1 Bar" grid walks the bar lines drawn over the clip —
/// the next one after the note, or the one before it — so across a meter
/// change, or in a clip that starts off a bar line, the note lands on a bar
/// line instead of one clip-start bar from where it was (#262). With no grid
/// (`Free`) it moves by `free_step`.
fn nudged_start(
    grid: Option<MeterGrid<'_, EditorMeter>>,
    beat: f32,
    forward: bool,
    free_step: f32,
) -> f32 {
    let step = match grid.map(|grid| (grid, grid.step())) {
        Some((grid, GridStep::Bar)) => {
            let at = f64::from(beat);
            let line = grid.line_at_or_before(at);
            let target = if forward {
                grid.next_line(line)
            } else if at - line > 1.0e-4 {
                line
            } else {
                grid.line_at_or_before(line - 1.0e-3)
            };
            return (target as f32).max(0.0);
        }
        Some((_, GridStep::Beats(step))) => step as f32,
        None => free_step,
    };
    let delta = if forward { step } else { -step };
    (beat + delta).max(0.0)
}

/// A note's length after one fine step of the inspector's Length stepper
/// (`forward` lengthens). On the "1 Bar" grid the step is a quarter of the
/// bar drawn at the note's end — the bar it grows into, or the one it shrinks
/// back through — so in 3/4 it is 0.75, not a quarter of a 4/4 bar (#262).
/// Otherwise `fine_step`.
fn nudged_length(
    grid: Option<MeterGrid<'_, EditorMeter>>,
    start: f32,
    duration: f32,
    forward: bool,
    fine_step: f32,
) -> f32 {
    let step = match grid {
        Some(grid) if grid.step() == GridStep::Bar => {
            let end = f64::from(start + duration);
            let (lo, hi) = grid.cell(if forward { end } else { end - 1.0e-3 });
            ((hi - lo) as f32 * 0.25).max(MIN_NOTE_BEATS)
        }
        _ => fine_step.max(MIN_NOTE_BEATS),
    };
    let delta = if forward { step } else { -step };
    (duration + delta).max(MIN_NOTE_BEATS)
}

/// The clip beats a snapped controller stroke or ramp from `a` to `b` writes
/// a point on: `a`, every grid line between them, and `b`, ascending — the
/// note grid's lines, so after a meter change inside the clip the points
/// stay on the bar lines drawn instead of stepping on by the first bar's
/// length.
fn controller_grid_lines(grid: MeterGrid<'_, EditorMeter>, a: f32, b: f32) -> Vec<f32> {
    grid.lines_between(f64::from(a), f64::from(b))
        .into_iter()
        .map(|beat| beat as f32)
        .collect()
}

/// `lo`, then every `step` after it, ending on `hi`: the evenly spaced
/// samples a controller ramp writes when there is no grid (`Free`).
fn uniform_controller_samples(lo: f32, hi: f32, step: f32) -> Vec<f32> {
    let step = step.max(1.0e-3);
    let count = (((hi - lo).max(0.0) / step).round() as i32).clamp(0, 65_536);
    (0..=count)
        .map(|i| {
            if i > 0 && i == count {
                hi
            } else {
                (lo + step * i as f32).min(hi)
            }
        })
        .collect()
}

/// What Select All selects: every note on a channel the channel-view filter
/// shows. A hidden note is out of reach of every other gesture, so selecting
/// it would let Delete, Transpose or Cut change notes the user cannot see.
fn visible_note_ids(notes: &[MidiNoteState], view: MidiChannelMask) -> HashSet<u64> {
    notes
        .iter()
        .filter(|note| view.contains(note.channel))
        .map(|note| note.id)
        .collect()
}

fn restore_velocity_values(notes: &mut [MidiNoteState], snapshot: &[(u64, u8)]) {
    for note in notes {
        if let Some((_, velocity)) = snapshot.iter().find(|(id, _)| *id == note.id) {
            note.velocity = *velocity;
        }
    }
}

/// Piano-roll chrome theme (spec Part 7).
struct PianoRollTheme {
    key_lane_width: f32,
}

fn piano_roll_theme() -> PianoRollTheme {
    PianoRollTheme {
        // Room for a keyboard's worth of key and, under it, the controller
        // lane's selector with its name readable.
        key_lane_width: 84.0,
    }
}

fn key_lane_width() -> f32 {
    piano_roll_theme().key_lane_width
}
/// Height of the single unified controller lane (velocity / CC / pitch-bend /
/// pressure). Replaces the old stacked velocity + CC lanes — one lane at a time.
const LANE_H: f32 = 140.0;
/// Paint above the piano grid/body so the lane selector is never hidden by the
/// editor's scroll/clip containers.
const LANE_MENU_PRIORITY: usize = 100;
/// Bar/beat ruler header height — the same as the Audio Editor's and the
/// Solfege pitch tab's, so the editors read as one family.
const RULER_H: f32 = 22.0;
/// The editor's toolbar: a prominent-tier row holding default-tier controls.
const TOOLBAR_H: f32 = crate::theme::size::PROMINENT + crate::theme::space::TIGHT;
/// The status footer under the editor.
const FOOTER_H: f32 = crate::theme::size::ROW_DENSE;
/// The note inspector column on the right.
const INSPECTOR_W: f32 = 232.0;
/// A collapsed controller lane keeps a strip with its selector, so the lane
/// can be brought back from where it went.
const LANE_COLLAPSED_H: f32 = crate::theme::size::DEFAULT;
/// Black keys cover this share of the key lane; the rest of each white key
/// is where its name sits.
const BLACK_KEY_SHARE: f32 = 0.6;
/// Px on a note's right edge that starts a resize. Sized for a comfortable grab
/// at workstation density; a note narrower than [`NOTE_RESIZE_MIN_W`] shows no
/// handle so the move zone never disappears under it.
const RESIZE_ZONE: f32 = 8.0;
/// Narrowest note that still offers a resize handle: the handle plus at least
/// as much move zone beside it.
const NOTE_RESIZE_MIN_W: f32 = RESIZE_ZONE * 2.0 + 2.0;
/// Narrowest a note block is ever drawn — and hit-tested. Drawing, clicking,
/// and marquee selection all use it, so a very short note at low zoom cannot
/// present a clickable block the rubber band then misses.
const NOTE_MIN_W: f32 = 5.0;
/// Pixels of movement before an empty-grid press becomes a marquee drag.
const MARQUEE_DRAG_THRESHOLD: f32 = 4.0;
/// Default velocity for newly drawn/reset notes. Kept beside the editor gesture
/// constants so draw and reset use one value without adding velocity-owned data.
const DEFAULT_NOTE_VELOCITY: u8 = 100;

/// A copied note, stored with timing relative to the earliest note in the
/// selection so paste/duplicate can re-anchor the group at a new beat.
#[derive(Clone)]
struct ClipboardNote {
    pitch: u8,
    rel_start: f32,
    duration: f32,
    velocity: u8,
    muted: bool,
    channel: MidiChannel,
    articulation: Option<ArticulationId>,
    /// v4: the note's continuous pitch performance. Copied by value so a paste
    /// carries the expression, with fresh point ids so the copy is independent.
    pitch_curve: Option<PitchCurve>,
    /// v5: protocol-neutral per-note expression. Curves are copied by value;
    /// expression points have no transient editor identity to regenerate.
    expression: NoteExpression,
}

/// Internal clipboard format version. Bumped if [`ClipboardNote`] layout or
/// semantics change so a paste can reject data it doesn't understand instead of
/// mis-reading it. The clipboard is process-local today, but versioning keeps
/// the contract explicit for a future cross-process / serialized clipboard.
/// v2 added the per-note MIDI channel. v3 added the per-note articulation.
/// v4 added the per-note pitch curve; v5 added protocol-neutral expression.
const MIDI_CLIPBOARD_VERSION: u32 = 5;

/// Versioned clipboard payload — a version tag plus the copied notes.
#[derive(Clone)]
struct ClipboardPayload {
    version: u32,
    notes: Vec<ClipboardNote>,
}

thread_local! {
    /// Process-global MIDI note clipboard. Lives outside any single editor so
    /// copy in the docked piano roll can paste in the floating one (both run on
    /// the GPUI main thread). Holds relative timing — not real notes.
    static MIDI_NOTE_CLIPBOARD: std::cell::RefCell<ClipboardPayload> = const {
        std::cell::RefCell::new(ClipboardPayload {
            version: MIDI_CLIPBOARD_VERSION,
            notes: Vec::new(),
        })
    };
}

/// `true` when the MIDI-editor zoom diagnostics are enabled via
/// `FUTUREBOARD_MIDI_ZOOM_DEBUG`. Off by default — zoom logging must not be
/// always-on (per project debug-flag conventions).
fn midi_zoom_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIDI_ZOOM_DEBUG").is_some())
}

/// `true` when `beat` is (within tolerance) an integer multiple of `m`.
#[inline]
fn is_multiple(beat: f32, m: f32) -> bool {
    if m <= 0.0 {
        return false;
    }
    let r = (beat / m).round();
    (beat - r * m).abs() < 1.0e-3
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// `true` for the black keys of the chromatic octave. Shared so every pitch
/// surface tints the same rows.
pub fn is_black(pitch: i32) -> bool {
    matches!(pitch.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// Scientific pitch name for a semitone, shared with the Pitch tab's pitch
/// gutter so both surfaces label rows identically.
pub fn note_name(pitch: i32) -> String {
    format!(
        "{}{}",
        NOTE_NAMES[pitch.rem_euclid(12) as usize],
        pitch / 12 - 1
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PianoTool {
    Draw,
    Select,
    /// Draw a linear value ramp in the active controller lane.
    Line,
    /// Click/drag a note to delete it.
    Erase,
    /// Click a note to split it at the cursor beat.
    Split,
    /// Click a note to toggle its muted state.
    Mute,
}

/// What the single unified controller lane currently shows and edits. Replaces
/// the old always-on stacked velocity + CC lanes — exactly one is shown at a
/// time. `Velocity` edits note-owned velocity; `Controller` edits a controller
/// automation lane (CC / pitch-bend / pressure) by [`MidiControllerKind`];
/// `Articulations` edits the clip's direction articulation events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerLaneKind {
    Velocity,
    Controller(MidiControllerKind),
    Articulations,
}

/// Lane choices presented by the selector / cycled by the keyboard commands,
/// in display order. Custom CC numbers (not in this list) are reachable via the
/// selector's stepper and are preserved as data either way.
const LANE_CYCLE: [ControllerLaneKind; 10] = [
    ControllerLaneKind::Velocity,
    ControllerLaneKind::Controller(MidiControllerKind::CC(1)),
    ControllerLaneKind::Controller(MidiControllerKind::CC(7)),
    ControllerLaneKind::Controller(MidiControllerKind::CC(10)),
    ControllerLaneKind::Controller(MidiControllerKind::CC(11)),
    ControllerLaneKind::Controller(MidiControllerKind::CC(64)),
    ControllerLaneKind::Controller(MidiControllerKind::PitchBend),
    ControllerLaneKind::Controller(MidiControllerKind::ChannelPressure),
    ControllerLaneKind::Controller(MidiControllerKind::PolyPressure),
    ControllerLaneKind::Articulations,
];

/// Grid / snap resolution for the piano roll.
///
/// `Free` disables snapping (same as the Snap toolbar toggle off). `Adaptive`
/// picks a subdivision from the current Zoom X (`ppb`) so the grid stays usable
/// when zoomed out. Triplet variants use 2/3 of the straight-note step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridRes {
    Free,
    Adaptive,
    Bar,
    Whole,
    Half,
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
    SixtyFourth,
    QuarterTriplet,
    EighthTriplet,
    SixteenthTriplet,
}

#[derive(Debug, Clone)]
pub enum UiMidiPreviewCommand {
    NoteOn {
        track_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    NoteOff {
        track_id: String,
        channel: u8,
        pitch: u8,
    },
    AllNotesOff {
        track_id: String,
    },
    MidiPanic {
        track_id: String,
    },
}

impl UiMidiPreviewCommand {
    pub fn track_id(&self) -> &str {
        match self {
            Self::NoteOn { track_id, .. }
            | Self::NoteOff { track_id, .. }
            | Self::AllNotesOff { track_id }
            | Self::MidiPanic { track_id } => track_id,
        }
    }
}

impl GridRes {
    const ALL: [GridRes; 13] = [
        GridRes::Free,
        GridRes::Adaptive,
        GridRes::Bar,
        GridRes::Whole,
        GridRes::Half,
        GridRes::Quarter,
        GridRes::Eighth,
        GridRes::Sixteenth,
        GridRes::ThirtySecond,
        GridRes::SixtyFourth,
        GridRes::QuarterTriplet,
        GridRes::EighthTriplet,
        GridRes::SixteenthTriplet,
    ];

    /// Fixed step in beats for non-adaptive modes. `Adaptive` / `Free` return 0
    /// — callers must resolve via [`PianoRoll::step_beats`].
    fn fixed_beats(self) -> f32 {
        match self {
            GridRes::Free | GridRes::Adaptive => 0.0,
            GridRes::Bar | GridRes::Whole => 4.0,
            GridRes::Half => 2.0,
            GridRes::Quarter => 1.0,
            GridRes::Eighth => 0.5,
            GridRes::Sixteenth => 0.25,
            GridRes::ThirtySecond => 0.125,
            GridRes::SixtyFourth => 0.0625,
            // Triplet of a quarter / eighth / sixteenth note (3 in the time of 2).
            GridRes::QuarterTriplet => 2.0 / 3.0,
            GridRes::EighthTriplet => 1.0 / 3.0,
            GridRes::SixteenthTriplet => 1.0 / 6.0,
        }
    }

    fn beats(self) -> f32 {
        self.fixed_beats()
    }

    fn label(self) -> &'static str {
        match self {
            GridRes::Free => "Free",
            GridRes::Adaptive => "Adaptive",
            GridRes::Bar => "1 Bar",
            GridRes::Whole => "1/1",
            GridRes::Half => "1/2",
            GridRes::Quarter => "1/4",
            GridRes::Eighth => "1/8",
            GridRes::Sixteenth => "1/16",
            GridRes::ThirtySecond => "1/32",
            GridRes::SixtyFourth => "1/64",
            GridRes::QuarterTriplet => "1/4T",
            GridRes::EighthTriplet => "1/8T",
            GridRes::SixteenthTriplet => "1/16T",
        }
    }

    fn cycle(self) -> Self {
        match self {
            GridRes::Free => GridRes::Adaptive,
            GridRes::Adaptive => GridRes::Bar,
            GridRes::Bar => GridRes::Whole,
            GridRes::Whole => GridRes::Half,
            GridRes::Half => GridRes::Quarter,
            GridRes::Quarter => GridRes::Eighth,
            GridRes::Eighth => GridRes::Sixteenth,
            GridRes::Sixteenth => GridRes::ThirtySecond,
            GridRes::ThirtySecond => GridRes::SixtyFourth,
            GridRes::SixtyFourth => GridRes::QuarterTriplet,
            GridRes::QuarterTriplet => GridRes::EighthTriplet,
            GridRes::EighthTriplet => GridRes::SixteenthTriplet,
            GridRes::SixteenthTriplet => GridRes::Free,
        }
    }

    fn is_free(self) -> bool {
        matches!(self, GridRes::Free)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PianoSelectMenu {
    Grid,
    /// Root and scale kind together: roots as a grid above the kinds.
    Scale,
    /// Chord stacking for the Draw tool.
    Chord,
    Lane,
    /// Channel view filter (All Channels / Channel 1–16).
    Channel,
    /// Articulation palette for the lane's insert tool (which articulation a
    /// lane click inserts).
    Articulation,
    /// The tool windows: Strum, Humanize, Length, Velocity, Clean Up.
    Tools,
}

/// Which of the three unified-lane views is active. `Controller` shows/edits
/// the [`PianoRoll::active_cc`] controller lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PianoLaneView {
    Velocity,
    Controller,
    Articulations,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarqueeSelectionMode {
    Replace,
    Add,
    Toggle,
    Subtract,
}

impl MarqueeSelectionMode {
    fn from_modifiers(modifiers: &gpui::Modifiers) -> Self {
        if modifiers.alt {
            MarqueeSelectionMode::Subtract
        } else if modifiers.control || modifiers.platform {
            MarqueeSelectionMode::Toggle
        } else if modifiers.shift {
            MarqueeSelectionMode::Add
        } else {
            MarqueeSelectionMode::Replace
        }
    }

    fn label(self) -> &'static str {
        match self {
            MarqueeSelectionMode::Replace => "Replace",
            MarqueeSelectionMode::Add => "Add",
            MarqueeSelectionMode::Toggle => "Toggle",
            MarqueeSelectionMode::Subtract => "Subtract",
        }
    }
}

/// Velocity ramp shape. Only `Linear` is exposed today; keeping interpolation
/// behind this enum lets future curve tools share the same gesture/undo path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VelocityCurve {
    Linear,
    Exponential,
    Logarithmic,
    SCurve,
    ReverseSCurve,
}

impl VelocityCurve {
    fn sample(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Exponential => t * t,
            Self::Logarithmic => t.sqrt(),
            Self::SCurve => t * t * (3.0 - 2.0 * t),
            Self::ReverseSCurve => {
                let u = 1.0 - t;
                1.0 - u * u * (3.0 - 2.0 * u)
            }
        }
    }
}

#[derive(Debug, Clone)]
enum PianoDrag {
    None,
    /// Move the selected notes. `prev` snapshots each affected note's original
    /// (id, start, pitch); `ids` is the same set, hashed, so the per-note
    /// geometry lookup every frame is O(1) rather than a scan of `prev`.
    /// `dx_beats` / `dpitch` are the live deltas. `grab_pitch` is the original
    /// pitch of the note under the pointer — the anchor for the live audition
    /// preview while the pitch is dragged.
    ///
    /// `grab` is the press in content space and `pointer` the last pointer
    /// position: the deltas are re-derived from both whenever the view scrolls
    /// or zooms (see [`PianoRoll::refresh_drag_from_pointer`]), so the note
    /// stays under a still cursor while the content moves. `travel` decides
    /// click vs drag on pointer movement alone (see [`PointerTravel`]).
    Move {
        grab: DragGrab,
        pointer: (f32, f32),
        travel: PointerTravel,
        prev: Vec<(u64, f32, u8)>,
        ids: HashSet<u64>,
        dx_beats: f32,
        dpitch: i32,
        grab_pitch: u8,
        /// Original start of the grabbed note. Snap this one anchor and apply its
        /// resulting delta to every peer so off-grid spacing is preserved.
        anchor_start: f32,
        clone_on_commit: bool,
        /// Live Shift-held state, refreshed every mouse move: bypasses grid
        /// snapping for this drag without touching the persistent `snap_on`
        /// toggle. Checked continuously, not just at mouse-down.
        unsnap: bool,
        /// A Shift-press on a note that was already selected: the note leaves
        /// the selection if the press ends without a move, which keeps
        /// Shift-click a selection toggle while Shift-drag moves freely.
        deselect_on_click: Option<u64>,
    },
    /// Resize notes from a right-edge handle. If the grabbed note is part of a
    /// multi-selection, every selected note receives the same duration delta.
    /// `prev_durs` snapshots each affected note (keyed by id, so live geometry
    /// is an O(1) lookup per note) so live geometry and undo stay coherent
    /// without writing timeline state on mouse move. `grab` / `pointer` as in
    /// [`PianoDrag::Move`].
    Resize {
        id: u64,
        ids: Vec<u64>,
        grab: DragGrab,
        pointer: (f32, f32),
        travel: PointerTravel,
        prev_dur: f32,
        prev_durs: HashMap<u64, f32>,
        /// Original start of the grabbed note; right-edge snapping is performed
        /// against `anchor_start + duration`, not duration in isolation.
        anchor_start: f32,
        delta_dur: f32,
        new_dur: f32,
        /// Live Shift-held state, refreshed every mouse move (see
        /// `PianoDrag::Move::unsnap`).
        unsnap: bool,
    },
    /// Time-stretch the selection from a handle at one end of its span on the
    /// ruler, or Alt on a selected note's end. `spec` holds the span at
    /// mouse-down and the live factor; the notes are drawn through it and
    /// written once, on release, as one undo step.
    Stretch {
        ids: HashSet<u64>,
        start_x: f32,
        spec: StretchSpec,
        /// Live Shift-held state (see `PianoDrag::Move::unsnap`).
        unsnap: bool,
    },
    /// Drag a velocity bar. Values are always derived from `prev`, never from
    /// the prior mouse-move frame. Alt switches to absolute assignment and Shift
    /// scales the relative pointer delta for fine adjustment.
    Velocity {
        clip_id: String,
        prev: Vec<(u64, u8)>,
        anchor_orig: u8,
        start_mouse_y: f32,
        absolute: bool,
        fine: bool,
    },
    /// Absolute velocity painting from the lane background. `original_notes` is
    /// sorted by start beat for swept-interval hit testing; `touched` prevents a
    /// note from being duplicated in the eventual undo snapshot.
    VelocityPaint {
        clip_id: String,
        original_notes: Vec<MidiNoteState>,
        touched: HashSet<u64>,
        last_x: f32,
        last_y: f32,
    },
    /// Linear velocity ramp over selected notes (when any are selected), or all
    /// visible notes whose starts fall in the dragged beat range.
    VelocityLine {
        clip_id: String,
        original_notes: Vec<MidiNoteState>,
        affected: HashSet<u64>,
        anchor_beat: f32,
        anchor_value: u8,
        current_beat: f32,
        current_value: u8,
        curve: VelocityCurve,
        unsnap: bool,
    },
    /// Range/lasso selection in the velocity lane. It updates the same note-id
    /// selection used by the piano grid; velocity has no separate selection.
    VelocitySelect {
        clip_id: String,
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
        mode: MarqueeSelectionMode,
        dragging: bool,
    },
    /// Middle-mouse grab-pan of the note grid (scroll_x / scroll_y). Mutually
    /// exclusive with note editing — started only from Middle button down.
    Pan {
        last_x: f32,
        last_y: f32,
    },
    /// Rectangular marquee selection on the note grid (local grid px).
    MarqueeSelect {
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
        mode: MarqueeSelectionMode,
        /// `true` once the pointer moves past [`MARQUEE_DRAG_THRESHOLD`].
        dragging: bool,
    },
    /// Left-drag note creation preview (committed on mouse-up).
    DrawNote {
        pitch: u8,
        start_beat: f32,
        end_beat: f32,
        /// Live Shift-held state, refreshed every mouse move (see
        /// `PianoDrag::Move::unsnap`).
        unsnap: bool,
        /// Channel the new note is created with — the active channel-view
        /// filter's single channel if narrowed, otherwise the track/clip
        /// default. Fixed at mouse-down; drawing never changes a note's channel.
        channel: MidiChannel,
    },
    /// Right-drag erase — ids collected until mouse-up.
    EraseNotes {
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
        erased: HashSet<u64>,
    },
    /// Paint (left) or erase (right) on the active CC lane. The lane's pre-drag
    /// points are snapshotted in `cc_edit_prev`; one undo entry on release.
    ///
    /// `last` is the previous cursor position in lane-local pixels. A mouse move
    /// can jump many pixels, so the stroke is interpolated from `last` to the
    /// new position instead of stamping a single sample per event — that is what
    /// makes a fast freehand drag read as a continuous curve rather than a row
    /// of disconnected dots. `unsnap` mirrors the live Alt state so the grid can
    /// be released mid-stroke.
    CcPaint {
        erase: bool,
        last: Option<(f32, f32)>,
        unsnap: bool,
    },
    /// Drag one or more selected CC points. `prev` snapshots `(id, beat, value)`
    /// at drag start; each point moves by the cursor's Δbeat/Δvalue from where
    /// the press landed (`anchor_*`, unsnapped), so grabbing a handle
    /// off-centre does not make it jump and relative offsets are preserved.
    /// One undo entry on release.
    CcMove {
        ids: Vec<u64>,
        prev: Vec<(u64, f32, f32)>,
        anchor_beat: f32,
        anchor_value: f32,
        unsnap: bool,
    },
    /// Shift+drag a straight ramp across the active CC lane. Replaces points in
    /// the spanned beat range with an evenly-spaced line from the gesture anchor
    /// to the cursor. Pre-drag points live in `cc_edit_prev`; one undo on release.
    CcLine {
        anchor_beat: f32,
        anchor_value: f32,
        unsnap: bool,
    },
    /// Marquee selection for points in the active controller lane.
    CcSelect {
        clip_id: String,
        kind: MidiControllerKind,
        start_x: f32,
        start_y: f32,
        current_x: f32,
        current_y: f32,
        mode: MarqueeSelectionMode,
        dragging: bool,
    },
    /// Drag a direction articulation event (by transient id) to a new beat.
    /// Pre-drag events are snapshotted in `art_edit_prev`; one undo on release.
    ArtMove {
        id: u64,
    },
    /// Dragging the bar/beat ruler to seek the project playhead.
    RulerSeek,
}

impl PianoDrag {
    /// `true` for a gesture that carries note ids and is pruned against the
    /// clip's notes every frame (see `PianoRoll::prune_transient_state`).
    fn holds_note_ids(&self) -> bool {
        matches!(
            self,
            PianoDrag::Move { .. }
                | PianoDrag::Resize { .. }
                | PianoDrag::Stretch { .. }
                | PianoDrag::Velocity { .. }
                | PianoDrag::VelocityPaint { .. }
                | PianoDrag::VelocityLine { .. }
                | PianoDrag::EraseNotes { .. }
        )
    }
}

pub struct PianoRoll {
    timeline: Entity<Timeline>,
    /// When `true`, commit logs use `[MIDI Editor]` (floating window instance).
    pub midi_editor_sink: bool,
    /// Project beat the edited clip starts at, refreshed each render.
    ///
    /// Cached rather than read from the timeline on demand: every coordinate
    /// conversion needs it, including the ones inside mouse handlers that have
    /// no `Context` to read an entity through, and an origin that changed
    /// mid-gesture would move the notes out from under the pointer.
    edit_origin_beats: f32,
    /// The clips on screen, refreshed each render.
    scope: scope::EditorScope,
    /// Where the playhead is this frame. Shared with the overlay entity below
    /// so the line can move without rebuilding the editor around it.
    playhead_frame: playhead::PianoRollPlayheadFrameCell,
    /// The playhead's own entity, created on first render because it needs no
    /// geometry until there is some.
    playhead_overlay: Option<Entity<playhead::PianoRollPlayheadOverlay>>,
    /// Docked editor only: opens the floating MIDI editor window.
    on_pop_out: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    /// Floating editor only: the window's own actions, shown in the editor's
    /// footer so the window needs no second status row under it.
    on_export_midi: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    on_dock: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    /// Whether the note inspector column is shown.
    inspector_open: bool,
    /// The MIDI Tools column on the left (quantize, transpose, chords…).
    tools: tools_panel::ToolsPanelState,
    /// Settings of the tool windows (Strum, Humanize…), kept here so a tool
    /// reopens as it was left.
    advanced: advanced_tools::AdvancedToolSettings,
    /// The tool window that is open and the notes it is previewing on.
    tool_session: Option<advanced_tools::ToolSession>,
    on_midi_preview:
        Option<std::sync::Arc<dyn Fn(UiMidiPreviewCommand, &mut gpui::App) + Send + Sync>>,
    tool: PianoTool,
    /// Horizontal zoom: pixels per beat (Zoom X).
    ppb: f32,
    /// Vertical zoom: pixels per semitone / note-row height (Zoom Y).
    /// Independent of [`Self::ppb`]. Reset restores [`DEFAULT_ROW_H`].
    row_h: f32,
    /// Drum editor mode: the pitch axis is a list of drum lanes rather than
    /// 128 keys, notes are hits, and the Draw tool toggles a hit per step.
    drum_mode: bool,
    /// Which lanes drum mode lists.
    drum_lanes: DrumLanes,
    /// The lanes, top to bottom, refreshed each render from the edited clip
    /// (see [`Self::refresh_drum_rows`]). Every drum-mode row mapping reads it.
    drum_rows: Vec<u8>,
    /// Lane names from the track's own instrument (the Drum Sampler's pads),
    /// over the General MIDI names.
    drum_names: std::collections::HashMap<u8, SharedString>,
    snap_on: bool,
    grid_res: GridRes,
    /// Quantize strength resolution, independent of the visual grid.
    quantize_res: GridRes,
    selection: HashSet<u64>,
    scroll_x: f32,
    scroll_y: f32,
    /// The user scrolled the view sideways during this playback, so the
    /// editor stops following the playhead until the transport stops —
    /// playback must not drag the view away from what they went to look at.
    follow_paused: bool,
    drag: PianoDrag,
    /// Selection snapshot taken when a marquee gesture begins (for modifier modes).
    selection_before_marquee: HashSet<u64>,
    /// Notes highlighted during an erase drag.
    erase_preview_ids: HashSet<u64>,
    /// When true (Quantize button hovered), the grid shows ghost outlines at the
    /// positions the affected notes would snap to.
    quantize_preview: bool,
    /// Last clip-local beat the pointer was over the note grid. Used as the
    /// anchor for paste-at-mouse (`Ctrl/Cmd+Shift+V`) and status feedback.
    hover_beat: Option<f32>,
    /// Last pitch row the pointer was over in the note grid, for compact status.
    hover_pitch: Option<u8>,
    /// Detailed note hover text: pitch, start, length, velocity.
    hover_note_status: Option<String>,
    /// Live value text shown while dragging velocity/CC values.
    drag_value_status: Option<String>,
    /// Last clip id we ran [`Self::fit_piano_roll_to_notes`] for.
    fitted_clip_id: Option<String>,
    grid_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Bounds of the bar/beat ruler header; used to seek from window-space drag.
    ruler_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The controller kind shown/edited when the unified lane is NOT in velocity
    /// mode. Also remembered while velocity is shown, so switching back restores
    /// the last controller. Switching the lane never touches hidden lane data.
    active_cc: MidiControllerKind,
    /// Which view the unified lane shows: note velocities, the `active_cc`
    /// controller lane, or the articulation lane. Default: velocity.
    lane_view: PianoLaneView,
    /// Selected direction articulation event (transient id) in the lane.
    selected_articulation: Option<u64>,
    /// Articulation the lane's insert click places. Changed via the lane's
    /// palette dropdown.
    insert_articulation: ArticulationId,
    /// Clip articulation events snapshotted when a lane gesture begins
    /// (undo prev), mirroring `cc_edit_prev`.
    art_edit_prev: Option<Vec<MidiArticulationEvent>>,
    /// `false` collapses the controller lane entirely (grid uses the full
    /// height). Toggled from the selector / commands.
    lane_visible: bool,
    /// Toolbar dropdown open state. This is transient UI state only; musical
    /// scale/root/lock data live in `pitch_ctx`.
    open_select_menu: Option<PianoSelectMenu>,
    /// CC number bound to the selector's "Custom CC" stepper (0..=127).
    custom_cc: u8,
    /// Bounds of the CC strip, captured at paint for cursor → beat/value mapping.
    cc_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The window's device scale, recorded each render so the controller
    /// lane's GPU painter rasterises at physical resolution.
    window_scale: f32,
    /// The project's meter, refreshed each render; drives the grid, the
    /// ruler and the lane grids.
    meter: EditorMeter,
    /// Lane points snapshotted when a CC paint/erase gesture begins (undo prev).
    cc_edit_prev: Option<Vec<MidiControllerPoint>>,
    /// Clip/controller captured with `cc_edit_prev`; cancellation and commit must
    /// never resolve against a different clip or lane selected mid-gesture.
    cc_edit_target: Option<(String, MidiControllerKind)>,
    /// Selected CC point ids in the active controller lane (multi-edit).
    cc_selection: HashSet<u64>,
    /// Point selection captured when a CC marquee starts.
    cc_selection_before_marquee: HashSet<u64>,
    /// CC point a Select-tool press would grab, so the target is visible
    /// before the click rather than discovered by it.
    cc_hover_point: Option<u64>,
    /// Right-click CC curve context menu (local strip px of the click).
    open_cc_curve_menu: Option<(f32, f32)>,
    /// Right-click velocity transform menu (local lane px).
    open_velocity_menu: Option<(f32, f32)>,
    /// Right-click note menu: grid-local px of the click. Articulation, mute,
    /// velocity and delete for the current selection.
    open_note_menu: Option<(f32, f32)>,
    /// Last clip the editor rendered — used to emit the `open_editor` debug log
    /// exactly once when the edited clip changes (not every frame).
    last_editing_clip: Option<String>,
    active_preview_note: Option<(String, u8, u8)>,
    /// Pitch currently sounding from the left key lane (the audition note); also
    /// drives the pressed-key highlight. `None` while no key is being auditioned
    /// (including mid-drag when the cursor has left the lane).
    key_lane_pressed_pitch: Option<u8>,
    /// `true` between a key mouse-down and the matching mouse-up — i.e. the user
    /// is scrubbing the piano-key lane. Kept separate from
    /// [`Self::key_lane_pressed_pitch`] so a drag that wanders off the lane (note
    /// off, no sounding pitch) still resumes auditioning when it returns.
    piano_key_drag_active: bool,
    /// Bounds of the left key-lane keys area, captured at paint so a window-space
    /// cursor can be hit-tested + mapped to a pitch with the same math the grid
    /// uses. `None` until the first frame lays the lane out.
    key_lane_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    focus: FocusHandle,
    focus_lost_subscription: Option<Subscription>,
    /// Scale/root + constrain toggle for scale-aware pitch editing. Off
    /// (`constrain: false`) preserves raw chromatic drag/draw behavior.
    pitch_ctx: PitchTransformContext,
    /// Draw-tool chord stacking. Independent of Lock: a triad/7th still uses
    /// the active scale even when chromatic dragging is allowed.
    chord_kind: ChordInsertKind,
    /// Channel view/edit filter: `ALL` (default) renders and allows editing
    /// every note unchanged; narrowed to a single channel via the toolbar
    /// dropdown, notes on other channels are hidden (and therefore not
    /// reachable by mouse gestures — no separate editability check needed).
    channel_view: MidiChannelMask,
}

/// CC curve generators available from the controller-lane context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VelocityOperation {
    SetDefault,
    Add,
    Subtract,
    Scale,
    Compress,
    Expand,
    Randomize,
    Humanize,
    Crescendo,
    Decrescendo,
    Reverse,
    ResetDefault,
}

impl VelocityOperation {
    const ALL: [Self; 12] = [
        Self::SetDefault,
        Self::Add,
        Self::Subtract,
        Self::Scale,
        Self::Compress,
        Self::Expand,
        Self::Randomize,
        Self::Humanize,
        Self::Crescendo,
        Self::Decrescendo,
        Self::Reverse,
        Self::ResetDefault,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::SetDefault => "Set Velocity · 100",
            Self::Add => "Add · +5",
            Self::Subtract => "Subtract · -5",
            Self::Scale => "Scale · 110%",
            Self::Compress => "Compress · 25%",
            Self::Expand => "Expand · 25%",
            Self::Randomize => "Randomize",
            Self::Humanize => "Humanize · ±5",
            Self::Crescendo => "Crescendo",
            Self::Decrescendo => "Decrescendo",
            Self::Reverse => "Reverse",
            Self::ResetDefault => "Reset to Default",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CcCurveKind {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    SCurve,
    Exponential,
    Logarithmic,
    Ramp,
    Flat,
    Triangle,
    Saw,
    Square,
    Random,
    Humanize,
}

impl CcCurveKind {
    const ALL: [CcCurveKind; 14] = [
        CcCurveKind::Linear,
        CcCurveKind::EaseIn,
        CcCurveKind::EaseOut,
        CcCurveKind::EaseInOut,
        CcCurveKind::SCurve,
        CcCurveKind::Exponential,
        CcCurveKind::Logarithmic,
        CcCurveKind::Ramp,
        CcCurveKind::Flat,
        CcCurveKind::Triangle,
        CcCurveKind::Saw,
        CcCurveKind::Square,
        CcCurveKind::Random,
        CcCurveKind::Humanize,
    ];

    fn label(self) -> &'static str {
        match self {
            CcCurveKind::Linear => "Linear",
            CcCurveKind::EaseIn => "Ease In",
            CcCurveKind::EaseOut => "Ease Out",
            CcCurveKind::EaseInOut => "Ease In Out",
            CcCurveKind::SCurve => "S Curve",
            CcCurveKind::Exponential => "Exponential",
            CcCurveKind::Logarithmic => "Logarithmic",
            CcCurveKind::Ramp => "Ramp",
            CcCurveKind::Flat => "Flat",
            CcCurveKind::Triangle => "Triangle",
            CcCurveKind::Saw => "Saw",
            CcCurveKind::Square => "Square",
            CcCurveKind::Random => "Random",
            CcCurveKind::Humanize => "Humanize",
        }
    }

    /// Map normalized time `t` in [0, 1] to a value in [0, 1] for shaped ramps.
    /// Oscillator shapes ignore `from`/`to` endpoints and fill their own range.
    fn sample(self, t: f32, from: f32, to: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            CcCurveKind::Linear | CcCurveKind::Ramp => from + (to - from) * t,
            CcCurveKind::EaseIn => from + (to - from) * (t * t),
            CcCurveKind::EaseOut => {
                let u = 1.0 - (1.0 - t) * (1.0 - t);
                from + (to - from) * u
            }
            CcCurveKind::EaseInOut => {
                let u = if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                };
                from + (to - from) * u
            }
            CcCurveKind::SCurve => {
                // Smoothstep (Hermite).
                let u = t * t * (3.0 - 2.0 * t);
                from + (to - from) * u
            }
            CcCurveKind::Exponential => {
                let u = if t <= 0.0 {
                    0.0
                } else {
                    ((t * 4.0).exp() - 1.0) / ((4.0_f32).exp() - 1.0)
                };
                from + (to - from) * u
            }
            CcCurveKind::Logarithmic => {
                let u = if t <= 0.0 {
                    0.0
                } else {
                    (1.0 + t * 9.0).ln() / (10.0_f32).ln()
                };
                from + (to - from) * u
            }
            CcCurveKind::Flat => from,
            CcCurveKind::Triangle => {
                if t < 0.5 {
                    t * 2.0
                } else {
                    2.0 - t * 2.0
                }
            }
            CcCurveKind::Saw => t,
            CcCurveKind::Square => {
                if t < 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            CcCurveKind::Random | CcCurveKind::Humanize => {
                // Deterministic-ish hash of t so re-applying the same span is stable
                // within a session without pulling RNG into the hot path.
                let bits = (t * 10000.0) as u32;
                let h = bits.wrapping_mul(2654435761);
                (h >> 24) as f32 / 255.0
            }
        }
        .clamp(0.0, 1.0)
    }
}

impl PianoRoll {
    pub fn new(timeline: Entity<Timeline>, cx: &mut Context<Self>) -> Self {
        Self {
            timeline,
            midi_editor_sink: false,
            edit_origin_beats: 0.0,
            scope: scope::EditorScope::default(),
            playhead_frame: std::rc::Rc::new(std::cell::Cell::new(
                playhead::PianoRollPlayheadFrame::default(),
            )),
            playhead_overlay: None,
            on_pop_out: None,
            on_export_midi: None,
            on_dock: None,
            inspector_open: true,
            tools: tools_panel::ToolsPanelState::default(),
            advanced: advanced_tools::AdvancedToolSettings::default(),
            tool_session: None,
            on_midi_preview: None,
            tool: PianoTool::Draw,
            ppb: 80.0,
            row_h: DEFAULT_ROW_H,
            drum_mode: false,
            drum_lanes: DrumLanes::Kit,
            drum_rows: Vec::new(),
            drum_names: std::collections::HashMap::new(),
            snap_on: true,
            grid_res: GridRes::Sixteenth,
            quantize_res: GridRes::Sixteenth,
            selection: HashSet::new(),
            scroll_x: 0.0,
            scroll_y: 0.0,
            follow_paused: false,
            drag: PianoDrag::None,
            selection_before_marquee: HashSet::new(),
            erase_preview_ids: HashSet::new(),
            quantize_preview: false,
            hover_beat: None,
            hover_pitch: None,
            hover_note_status: None,
            drag_value_status: None,
            fitted_clip_id: None,
            grid_bounds: Rc::new(Cell::new(None)),
            ruler_bounds: Rc::new(Cell::new(None)),
            active_cc: MidiControllerKind::CC(1),
            lane_view: PianoLaneView::Velocity,
            selected_articulation: None,
            insert_articulation: ArticulationId::Staccato,
            art_edit_prev: None,
            lane_visible: true,
            open_select_menu: None,
            custom_cc: 74,
            cc_bounds: Rc::new(Cell::new(None)),
            window_scale: 1.0,
            meter: EditorMeter::constant(4.0),
            cc_edit_prev: None,
            cc_edit_target: None,
            cc_selection: HashSet::new(),
            cc_selection_before_marquee: HashSet::new(),
            cc_hover_point: None,
            open_cc_curve_menu: None,
            open_velocity_menu: None,
            open_note_menu: None,
            last_editing_clip: None,
            active_preview_note: None,
            key_lane_pressed_pitch: None,
            piano_key_drag_active: false,
            key_lane_bounds: Rc::new(Cell::new(None)),
            focus: cx.focus_handle(),
            focus_lost_subscription: None,
            pitch_ctx: PitchTransformContext::default(),
            chord_kind: ChordInsertKind::Off,
            channel_view: MidiChannelMask::ALL,
        }
    }

    /// Set the scale guide, exactly as picking root and scale in the header
    /// does: a non-chromatic scale also constrains note gestures to it.
    pub fn set_scale(&mut self, root: ScaleRoot, kind: ScaleKind, cx: &mut Context<Self>) {
        self.pitch_ctx.scale.root = root;
        self.pitch_ctx.scale.kind = kind;
        self.pitch_ctx.constrain = kind != ScaleKind::Chromatic;
        cx.notify();
    }

    pub fn set_pop_out_handler(
        &mut self,
        handler: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    ) {
        self.on_pop_out = handler;
    }

    /// The floating window's actions — export the clip, and put the editor
    /// back in the bottom panel — shown in the editor's own footer.
    pub fn set_window_actions(
        &mut self,
        export_midi: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
        dock: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    ) {
        self.on_export_midi = export_midi;
        self.on_dock = dock;
    }

    pub fn set_midi_preview_handler(
        &mut self,
        handler: Option<std::sync::Arc<dyn Fn(UiMidiPreviewCommand, &mut gpui::App) + Send + Sync>>,
    ) {
        self.on_midi_preview = handler;
    }

    fn prune_transient_state(&mut self, cx: &Context<Self>, clip_id: Option<&str>) {
        let Some(clip_id) = clip_id else {
            self.selection.clear();
            self.selection_before_marquee.clear();
            self.erase_preview_ids.clear();
            self.drag = PianoDrag::None;
            self.cc_edit_prev = None;
            self.cc_edit_target = None;
            self.cc_selection_before_marquee.clear();
            self.art_edit_prev = None;
            self.selected_articulation = None;
            self.open_note_menu = None;
            self.hover_beat = None;
            self.hover_pitch = None;
            self.hover_note_status = None;
            self.drag_value_status = None;
            self.fitted_clip_id = None;
            return;
        };

        // Pruning runs every frame, and building this set is O(notes in clip)
        // plus an allocation. With nothing selected, hovering, or queued for
        // erase there is nothing to prune, and the three `retain`s below would
        // walk empty collections — so do not pay for the set at all. That is
        // the common case: the editor sits idle with no selection far more
        // often than it holds one.
        //
        // A gesture holding note ids needs the set as well, selection or not:
        // pruning it against an empty set reads as "every note is gone" and
        // cancels the gesture — a velocity paint or line with nothing selected
        // stopped after its first frame and never reached its undo commit.
        let needs_note_ids = !self.selection.is_empty()
            || !self.selection_before_marquee.is_empty()
            || !self.erase_preview_ids.is_empty()
            || self.drag.holds_note_ids();
        let valid_note_ids: HashSet<u64> = if needs_note_ids {
            self.timeline
                .read(cx)
                .state
                .midi_clip_notes(clip_id)
                .map(|notes| notes.iter().map(|note| note.id).collect())
                .unwrap_or_default()
        } else {
            HashSet::new()
        };

        self.selection.retain(|id| valid_note_ids.contains(id));
        if self.selection.is_empty() {
            // The note menu acts on the selection; an emptied selection (the
            // notes were deleted, or the clip changed under it) leaves it
            // nothing to act on.
            self.open_note_menu = None;
        }
        self.selection_before_marquee
            .retain(|id| valid_note_ids.contains(id));
        self.erase_preview_ids
            .retain(|id| valid_note_ids.contains(id));
        if let Some(selected) = self.selected_articulation {
            let still_exists = self
                .timeline
                .read(cx)
                .state
                .midi_clip_articulations(clip_id)
                .is_some_and(|events| events.iter().any(|e| e.id == selected));
            if !still_exists {
                self.selected_articulation = None;
            }
        }

        let drag_is_stale = match &mut self.drag {
            PianoDrag::None
            | PianoDrag::MarqueeSelect { .. }
            | PianoDrag::VelocitySelect { .. }
            | PianoDrag::DrawNote { .. }
            | PianoDrag::CcPaint { .. }
            | PianoDrag::CcMove { .. }
            | PianoDrag::CcLine { .. }
            | PianoDrag::CcSelect { .. }
            | PianoDrag::ArtMove { .. }
            | PianoDrag::RulerSeek
            | PianoDrag::Pan { .. } => false,
            PianoDrag::Move { prev, ids, .. } => {
                prev.retain(|(id, _, _)| valid_note_ids.contains(id));
                ids.retain(|id| valid_note_ids.contains(id));
                prev.is_empty()
            }
            PianoDrag::Resize { id, .. } => !valid_note_ids.contains(id),
            PianoDrag::Stretch { ids, .. } => {
                ids.retain(|id| valid_note_ids.contains(id));
                ids.is_empty()
            }
            PianoDrag::Velocity { prev, .. } => {
                prev.retain(|(id, _)| valid_note_ids.contains(id));
                prev.is_empty()
            }
            PianoDrag::VelocityPaint { original_notes, .. }
            | PianoDrag::VelocityLine { original_notes, .. } => {
                original_notes.retain(|note| valid_note_ids.contains(&note.id));
                original_notes.is_empty()
            }
            PianoDrag::EraseNotes { erased, .. } => {
                erased.retain(|id| valid_note_ids.contains(id));
                false
            }
        };
        if drag_is_stale {
            self.drag = PianoDrag::None;
        }
    }

    pub fn selected_note_count(&self) -> usize {
        self.selection.len()
    }

    /// `true` when this editor's grid currently holds keyboard focus. Used by
    /// `StudioLayout` to route Ctrl+A/C/V/X/Delete to the MIDI editor (its own
    /// `on_key_down`) instead of the timeline clip commands.
    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    pub fn handle_key_event(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_key(event, window, cx);
    }

    pub fn grid_label(&self) -> &'static str {
        self.grid_res.label()
    }

    /// What the pointer is over, or what the gesture in flight is doing — the
    /// footer's live readout. The counts sit beside it, separately.
    fn pointer_status(&self) -> String {
        // Where the pointer is on the song, the way the ruler says it — not
        // a beat count from the clip's start, which reads negative left of it.
        let pointer = match (self.hover_pitch, self.hover_beat) {
            (Some(pitch), Some(beat)) => format!(
                "{} · {}",
                note_name(pitch as i32),
                self.meter.bar_beat_label(self.edit_origin() + beat)
            ),
            _ => String::new(),
        };
        match &self.drag {
            PianoDrag::Velocity { prev, .. } => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| format!("Vel drag · {} note{}", prev.len(), plural(prev.len()))),
            PianoDrag::VelocityPaint { touched, .. } => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| format!("Velocity paint · {} notes", touched.len())),
            PianoDrag::VelocityLine { affected, .. } => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| format!("Velocity line · {} notes", affected.len())),
            PianoDrag::VelocitySelect { mode, dragging, .. } => {
                if *dragging {
                    format!("Velocity select · {}", mode.label())
                } else {
                    "Velocity select".to_string()
                }
            }
            PianoDrag::DrawNote {
                pitch,
                start_beat,
                end_beat,
                unsnap,
                ..
            } => {
                let (lo, hi) = normalize_range(*start_beat, *end_beat);
                format!(
                    "Draw {} · {:.2}+{:.2}",
                    note_name(*pitch as i32),
                    lo,
                    self.drawn_note_length(lo, hi, *unsnap)
                )
            }
            PianoDrag::Move {
                dx_beats, dpitch, ..
            } => format!("Move Δ{:.2} beat Δ{} st", dx_beats, dpitch),
            PianoDrag::Resize { new_dur, .. } => format!("Length {:.2}", new_dur),
            PianoDrag::Stretch { ids, spec, .. } => stretch::stretch_status(spec, ids.len()),
            PianoDrag::CcPaint { erase, .. } => {
                self.drag_value_status.clone().unwrap_or_else(|| {
                    if *erase {
                        "CC erase".to_string()
                    } else {
                        "CC draw".to_string()
                    }
                })
            }
            PianoDrag::CcMove { .. } => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| "CC move".to_string()),
            PianoDrag::CcLine { .. } => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| "CC line".to_string()),
            PianoDrag::CcSelect { mode, dragging, .. } => {
                if *dragging {
                    format!("CC select · {}", mode.label())
                } else {
                    "CC select".to_string()
                }
            }
            PianoDrag::ArtMove { .. } => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| "Articulation move".to_string()),
            PianoDrag::RulerSeek => self
                .drag_value_status
                .clone()
                .unwrap_or_else(|| "Seek".to_string()),
            PianoDrag::EraseNotes { erased, .. } => {
                format!("Erase {} note{}", erased.len(), plural(erased.len()))
            }
            PianoDrag::MarqueeSelect { mode, dragging, .. } => {
                if *dragging {
                    format!("Select · {}", mode.label())
                } else {
                    "Select".to_string()
                }
            }
            PianoDrag::Pan { .. } => "Pan".to_string(),
            PianoDrag::None => self.hover_note_status.clone().unwrap_or(pointer),
        }
    }

    // ── Unified controller lane selection ────────────────────────────────
    /// What the single bottom lane currently shows/edits.
    fn current_lane(&self) -> ControllerLaneKind {
        match self.lane_view {
            PianoLaneView::Velocity => ControllerLaneKind::Velocity,
            PianoLaneView::Controller => ControllerLaneKind::Controller(self.active_cc),
            PianoLaneView::Articulations => ControllerLaneKind::Articulations,
        }
    }

    /// Switch which controller the unified lane shows. Only changes what is
    /// displayed/edited — hidden lane data (velocity stays on notes, CC points
    /// stay in their lanes, articulation events stay on the clip) is never
    /// touched. Always makes the lane visible.
    fn set_lane(&mut self, kind: ControllerLaneKind, cx: &mut Context<Self>) {
        match kind {
            ControllerLaneKind::Velocity => self.lane_view = PianoLaneView::Velocity,
            ControllerLaneKind::Controller(k) => {
                self.lane_view = PianoLaneView::Controller;
                self.active_cc = k;
            }
            ControllerLaneKind::Articulations => self.lane_view = PianoLaneView::Articulations,
        }
        self.lane_visible = true;
        self.open_select_menu = None;
        self.selected_articulation = None;
        // Geometry of the active lane may differ; force a fresh bounds capture.
        self.cc_bounds.set(None);
        cx.notify();
    }

    /// Step through [`LANE_CYCLE`] (Next/Previous controller lane commands).
    fn cycle_lane(&mut self, dir: i32, cx: &mut Context<Self>) {
        let cur = self.current_lane();
        let n = LANE_CYCLE.len() as i32;
        let idx = LANE_CYCLE.iter().position(|k| *k == cur).unwrap_or(0) as i32;
        let next = (((idx + dir) % n) + n) % n;
        self.set_lane(LANE_CYCLE[next as usize], cx);
    }

    fn toggle_lane_visible(&mut self, cx: &mut Context<Self>) {
        self.lane_visible = !self.lane_visible;
        self.open_select_menu = None;
        cx.notify();
    }

    /// Display name of the active lane (header + selector button).
    fn lane_name(&self) -> String {
        match self.current_lane() {
            ControllerLaneKind::Velocity => "Velocity".to_string(),
            ControllerLaneKind::Controller(k) => cc_kind_label(k),
            ControllerLaneKind::Articulations => "Articulations".to_string(),
        }
    }

    /// Value-range caption for the active lane header.
    fn lane_range(&self) -> &'static str {
        match self.current_lane() {
            ControllerLaneKind::Velocity => "1–127",
            ControllerLaneKind::Controller(MidiControllerKind::PitchBend) => "-8192..8191",
            ControllerLaneKind::Controller(_) => "0–127",
            ControllerLaneKind::Articulations => "Direction",
        }
    }

    /// Menu / command-bar actions for the MIDI editor (shared menu IDs).
    pub fn run_menu_command(&mut self, command_id: &str, cx: &mut Context<Self>) {
        match command_id {
            "midi:select-all" | "midi.select_all" => {
                let Some(clip_id) = self.editing_clip_id(cx) else {
                    return;
                };
                if self.lane_view == PianoLaneView::Controller {
                    self.cc_selection = self
                        .timeline
                        .read(cx)
                        .state
                        .controller_lane_points(&clip_id, self.active_cc)
                        .map(|points| points.iter().map(|point| point.id).collect())
                        .unwrap_or_default();
                } else {
                    self.selection = self
                        .timeline
                        .read(cx)
                        .state
                        .midi_clip_notes(&clip_id)
                        .map(|notes| visible_note_ids(notes, self.channel_view))
                        .unwrap_or_default();
                }
                cx.notify();
            }
            "midi:delete-selected" | "midi.delete" => {
                if self.lane_view == PianoLaneView::Controller {
                    self.delete_selected_cc_points(cx)
                } else {
                    self.delete_selection(cx)
                }
            }
            "midi:duplicate-selected" | "midi.duplicate" => {
                if self.lane_view == PianoLaneView::Controller {
                    self.duplicate_selected_cc_points(cx)
                } else {
                    self.duplicate_selection(false, cx)
                }
            }
            "midi:quantize" | "midi.quantize" => self.quantize_selection(cx),
            "midi:nudge-left" => self.nudge_selected_start(false, self.step_beats(), cx),
            "midi:nudge-right" => self.nudge_selected_start(true, self.step_beats(), cx),
            "midi:transpose-up" | "midi.transpose_up" => self.transpose_selection(1, cx),
            "midi:transpose-down" | "midi.transpose_down" => self.transpose_selection(-1, cx),
            "midi:transpose-octave-up" => self.transpose_selection(12, cx),
            "midi:transpose-octave-down" => self.transpose_selection(-12, cx),
            "midi:velocity-increase" | "midi.velocity_increase" => {
                self.nudge_selected_velocity(1, cx)
            }
            "midi:velocity-decrease" | "midi.velocity_decrease" => {
                self.nudge_selected_velocity(-1, cx)
            }
            "midi:toggle-snap" | "midi.toggle_snap" => {
                self.snap_on = !self.snap_on;
                cx.notify();
            }
            "midi:tool-select" | "midi.tool.select" => {
                self.cancel_active_gesture(cx);
                self.tool = PianoTool::Select;
                cx.notify();
            }
            "midi:tool-draw" | "midi.tool.draw" => {
                self.cancel_active_gesture(cx);
                self.tool = PianoTool::Draw;
                cx.notify();
            }
            "midi:tool-line" | "midi.tool.line" => {
                self.cancel_active_gesture(cx);
                self.tool = PianoTool::Line;
                cx.notify();
            }
            "midi:fit-notes" => {
                if let Some(cid) = self.editing_clip_id(cx) {
                    self.fit_piano_roll_to_notes(cx, &cid);
                    cx.notify();
                }
            }
            "midi:scroll-to-c4" => {
                self.scroll_to_pitch(60);
                cx.notify();
            }
            "midi:reset-pitch-zoom" => {
                // Restore default note-row height (Zoom Y) without touching Zoom X.
                self.reset_row_h_zoom(cx);
                self.scroll_to_pitch(60);
                cx.notify();
            }
            "midi:lane-next" => self.cycle_lane(1, cx),
            "midi:lane-prev" => self.cycle_lane(-1, cx),
            "midi:lane-velocity" => self.set_lane(ControllerLaneKind::Velocity, cx),
            "midi:lane-cc" => self.set_lane(ControllerLaneKind::Controller(self.active_cc), cx),
            "midi:lane-articulations" => self.set_lane(ControllerLaneKind::Articulations, cx),
            "midi:lane-toggle" => self.toggle_lane_visible(cx),
            // Selected-note articulation assignment (menu / context commands).
            "midi:articulation-none" => self.set_selection_articulation(None, cx),
            "midi:articulation-sustain" => {
                self.set_selection_articulation(Some(ArticulationId::Sustain), cx)
            }
            "midi:articulation-staccato" => {
                self.set_selection_articulation(Some(ArticulationId::Staccato), cx)
            }
            "midi:articulation-staccatissimo" => {
                self.set_selection_articulation(Some(ArticulationId::Staccatissimo), cx)
            }
            "midi:articulation-legato" => {
                self.set_selection_articulation(Some(ArticulationId::Legato), cx)
            }
            "midi:articulation-tenuto" => {
                self.set_selection_articulation(Some(ArticulationId::Tenuto), cx)
            }
            "midi:articulation-accent" => {
                self.set_selection_articulation(Some(ArticulationId::Accent), cx)
            }
            "midi:articulation-marcato" => {
                self.set_selection_articulation(Some(ArticulationId::Marcato), cx)
            }
            // Insert the palette articulation as a direction event at the
            // playhead (clip-local, snapped).
            "midi:articulation-insert" => {
                if let Some(clip_id) = self.editing_clip_id(cx) {
                    let beat = self.playhead_paste_anchor(cx, &clip_id);
                    self.insert_articulation_at(&clip_id, beat, cx);
                }
            }
            _ => {}
        }
    }

    // ── Editing target ───────────────────────────────────────────────────
    /// The selected clip id, but only if it is a MIDI clip.
    fn editing_clip_id(&self, cx: &Context<Self>) -> Option<String> {
        let tl = self.timeline.read(cx);
        let cid = tl.state.selection.selected_clip_ids.first()?.clone();
        tl.state.midi_clip_notes(&cid).map(|_| cid)
    }

    /// `true` when a note on `channel` should render / be reachable by editor
    /// gestures under the current channel-view filter.
    fn channel_visible(&self, channel: MidiChannel) -> bool {
        self.channel_view.contains(channel)
    }

    /// Channel newly drawn notes should be created with: the channel-view
    /// filter's single channel when narrowed (so a drawn note is never
    /// immediately hidden by its own view filter), otherwise the editing
    /// track's default note channel.
    fn active_note_channel(&self, cx: &Context<Self>) -> MidiChannel {
        if let Some(channel) =
            MidiChannel::all().find(|ch| MidiChannelMask::single(*ch) == self.channel_view)
        {
            return channel;
        }
        let tl = self.timeline.read(cx);
        tl.state
            .selection
            .selected_clip_ids
            .first()
            .and_then(|clip_id| tl.state.find_clip(clip_id))
            .map(|(track, _)| track.routing.default_note_channel())
            .unwrap_or_default()
    }

    fn preview_target(&self, cx: &Context<Self>) -> Option<(String, u8)> {
        use crate::components::timeline::timeline_state::{ClipType, TrackType};
        let tl = self.timeline.read(cx);
        let channel_for = |track: &crate::components::timeline::timeline_state::TrackState| {
            track
                .routing
                .midi_channel
                .map(|ch| ch.saturating_sub(1).min(15))
                .unwrap_or(0)
        };
        // Preview must reach whichever plugin instance actually plays this
        // track's notes: a MIDI track routed to an Instrument via
        // `TrackOutputRouting::Instrument` previews through that target, not
        // through itself (it owns no plugin). Keeps live audition in sync
        // with the same redirect the playback engine snapshot applies.
        let resolved_target = |track: &crate::components::timeline::timeline_state::TrackState| {
            tl.state
                .effective_instrument_track_id(&track.id)
                .map(|target_id| (target_id, channel_for(track)))
        };
        // Once a MIDI clip/track is actually selected, trust that as the
        // preview target rather than falling through to an unrelated track —
        // an unrouted MIDI track should stay silent, not surprise-preview
        // through whichever other track happens to own a plugin.
        if let Some(clip_id) = tl.state.selection.selected_clip_ids.first() {
            if let Some((track, clip)) = tl.state.find_clip(clip_id) {
                if matches!(clip.clip_type, ClipType::Midi { .. }) {
                    return resolved_target(track);
                }
            }
        }
        if let Some(track_id) = tl.state.selection.selected_track_id.as_deref() {
            if let Some(track) = tl.state.find_track(track_id) {
                if matches!(track.track_type, TrackType::Instrument | TrackType::Midi) {
                    return resolved_target(track);
                }
            }
        }
        tl.state
            .tracks
            .iter()
            .find(|track| {
                matches!(track.track_type, TrackType::Instrument | TrackType::Midi)
                    && track.instrument_insert().is_some()
            })
            .map(|track| (track.id.clone(), channel_for(track)))
    }

    fn begin_preview_note(
        &mut self,
        pitch: u8,
        velocity: u8,
        reason: &str,
        cx: &mut Context<Self>,
    ) {
        self.end_preview_note("replace", cx);
        let Some((track_id, channel)) = self.preview_target(cx) else {
            if midi_debug_enabled() {
                eprintln!(
                    "[MidiEditor] sending PreviewNoteOn skipped pitch={} reason={} no_midi_track",
                    pitch, reason
                );
            }
            return;
        };
        if midi_debug_enabled() {
            eprintln!(
                "[MidiEditor] sending PreviewNoteOn track_id={} pitch={} channel={} reason={}",
                track_id, pitch, channel, reason
            );
        }
        if let Some(handler) = self.on_midi_preview.clone() {
            handler(
                UiMidiPreviewCommand::NoteOn {
                    track_id: track_id.clone(),
                    channel,
                    pitch,
                    velocity,
                },
                cx,
            );
            self.active_preview_note = Some((track_id, channel, pitch));
        }
    }

    fn end_preview_note(&mut self, reason: &str, cx: &mut Context<Self>) {
        let Some((track_id, channel, pitch)) = self.active_preview_note.take() else {
            return;
        };
        if midi_debug_enabled() {
            eprintln!(
                "[MidiEditor] sending PreviewNoteOff track_id={} pitch={} channel={} reason={}",
                track_id, pitch, channel, reason
            );
        }
        if let Some(handler) = self.on_midi_preview.clone() {
            handler(
                UiMidiPreviewCommand::NoteOff {
                    track_id,
                    channel,
                    pitch,
                },
                cx,
            );
        }
    }

    pub fn preview_all_notes_off(&mut self, reason: &str, cx: &mut Context<Self>) {
        let target = self
            .active_preview_note
            .as_ref()
            .map(|(track_id, _, _)| track_id.clone())
            .or_else(|| self.preview_target(cx).map(|(track_id, _)| track_id));
        self.active_preview_note = None;
        // Any all-notes-off (escape, focus loss, clip change, editor close,
        // destructive edit) must also end a piano-key scrub so the pressed key
        // clears and a stale drag can't keep re-triggering.
        self.key_lane_pressed_pitch = None;
        self.piano_key_drag_active = false;
        let Some(track_id) = target else {
            return;
        };
        if midi_debug_enabled() {
            eprintln!(
                "[MidiEditor] sending PreviewAllNotesOff track_id={} reason={}",
                track_id, reason
            );
        }
        if let Some(handler) = self.on_midi_preview.clone() {
            handler(UiMidiPreviewCommand::AllNotesOff { track_id }, cx);
        }
    }

    pub fn midi_panic(&mut self, reason: &str, cx: &mut Context<Self>) {
        let target = self
            .active_preview_note
            .as_ref()
            .map(|(track_id, _, _)| track_id.clone())
            .or_else(|| self.preview_target(cx).map(|(track_id, _)| track_id));
        self.active_preview_note = None;
        self.key_lane_pressed_pitch = None;
        self.piano_key_drag_active = false;
        let Some(track_id) = target else {
            return;
        };
        if midi_debug_enabled() {
            eprintln!(
                "[MidiEditor] sending MidiPanic track_id={} reason={}",
                track_id, reason
            );
        }
        if let Some(handler) = self.on_midi_preview.clone() {
            handler(UiMidiPreviewCommand::MidiPanic { track_id }, cx);
        }
    }

    /// Stop any sounding preview/audition note AND panic the track before a
    /// destructive edit (delete / erase / cut) removes note data. The engine's
    /// `AllNotesOff` handler resolves the track instrument and sends explicit
    /// note-offs for tracked preview notes plus CC64/CC123/CC120/CC121, so a
    /// note that was sounding when its data is destroyed cannot get stuck.
    fn cleanup_midi_before_destructive_edit(&mut self, reason: &str, cx: &mut Context<Self>) {
        self.preview_all_notes_off(reason, cx);
    }

    /// Bar length at the edited clip's start. Only a scalar summary for
    /// what cannot follow the meter (the published viewport step, the stretch
    /// handle's step): snapping, nudging, drawing, drum hits and quantize go
    /// through the meter-aware [`Self::edit_grid`] instead.
    fn bar_beats(&self) -> f32 {
        self.meter.bar_beats_at(self.edit_origin())
    }

    fn step_beats(&self) -> f32 {
        grid_step_beats(self.grid_res, self.ppb, self.bar_beats())
    }

    fn snap_beats(&self, beats: f32) -> f32 {
        if !self.snap_on || self.grid_res.is_free() {
            return beats.max(0.0);
        }
        self.snap_to_grid(beats)
    }

    /// Snap a clip beat to the current grid whatever the snap toggle says —
    /// for a gesture that asks for the grid explicitly (Alt on the CC lane's
    /// freehand brush). `Free` has no grid and returns the beat as it is.
    fn snap_to_grid(&self, beats: f32) -> f32 {
        // Bars follow the meter, measured on the song's axis, so a note lands
        // on the bar line drawn under it in 3/4, 6/8 or across a meter
        // change; fixed steps count from the clip start. Both round (not
        // floor) so drag jitter near a grid line settles stably instead of
        // snapping one step early under the cursor.
        match self.edit_grid() {
            Some(grid) => (grid.snap(f64::from(beats)) as f32).max(0.0),
            None => beats.max(0.0),
        }
    }

    /// The note grid of the edited clip, whatever the snap toggle says (see
    /// [`note_grid`]): notes, controller lanes and Duplicate all go through
    /// it. `None` for `Free`.
    fn edit_grid(&self) -> Option<MeterGrid<'_, EditorMeter>> {
        note_grid(&self.meter, self.grid_res, self.ppb, self.edit_origin())
    }

    /// [`Self::edit_grid`] when a gesture snaps: snapping on, a grid with
    /// lines, and no Shift (`unsnap`). `None` otherwise.
    fn snapping_grid(&self, unsnap: bool) -> Option<MeterGrid<'_, EditorMeter>> {
        if unsnap || !self.snap_on {
            return None;
        }
        self.edit_grid()
    }

    /// Length of the note a draw from clip beat `lo` to `hi` creates — the
    /// commit, its preview and the status line all read it (see
    /// [`drawn_note_length`]).
    pub(super) fn drawn_note_length(&self, lo: f32, hi: f32, unsnap: bool) -> f32 {
        drawn_note_length(self.snapping_grid(unsnap), lo, hi)
    }

    /// Same as [`Self::snap_beats`], but `unsnap` (live Shift state of the
    /// current drag) bypasses the grid regardless of the persistent `snap_on`
    /// toggle. Centralizes the Shift-to-unsnap behavior for note move/resize/draw.
    fn snap_beats_live(&self, beats: f32, unsnap: bool) -> f32 {
        if unsnap {
            return beats.max(0.0);
        }
        self.snap_beats(beats)
    }

    // ── Coordinate helpers ────────────────────────────────────────────────
    //
    // Two frames, and the names say which is which. The axis is project beats —
    // what the ruler counts, and where the other clips on the track sit. Notes,
    // CC points and articulations are stored against their own clip's start, so
    // they go through the clip pair, which is the project pair plus the edited
    // clip's origin.
    //
    // Before this the roll had one frame and called it `beat`, which worked
    // exactly as long as the editor could only ever show one clip.

    /// Project beat under a local x.
    fn x_to_project_beat(&self, local_x: f32) -> f32 {
        local_x_to_beat(local_x, self.ppb, self.scroll_x)
    }

    /// Local x for a project beat.
    fn project_beat_to_x(&self, beat: f32) -> f32 {
        beat_to_local_x(beat, self.ppb, self.scroll_x)
    }

    /// Where the edited clip starts, in project beats.
    fn edit_origin(&self) -> f32 {
        self.edit_origin_beats
    }

    /// Beat within the edited clip under a local x — the frame a note's
    /// `start` is measured in.
    fn x_to_clip_beat(&self, local_x: f32) -> f32 {
        self.x_to_project_beat(local_x) - self.edit_origin()
    }

    /// Local x for a beat within the edited clip.
    fn clip_beat_to_x(&self, beat: f32) -> f32 {
        self.project_beat_to_x(beat + self.edit_origin())
    }
    fn y_to_pitch(&self, local_y: f32) -> u8 {
        if self.drum_mode {
            if let Some(pitch) =
                drum_map::drum_row_at(&self.drum_rows, local_y, self.scroll_y, self.note_row_h())
            {
                return pitch;
            }
        }
        local_y_to_pitch(local_y, self.scroll_y, self.row_h)
    }
    fn pitch_to_y(&self, pitch: u8) -> f32 {
        if self.drum_mode {
            // A note with no lane is drawn nowhere; `drum_rows` gives every
            // played note one, so this only hides what was never played.
            return match drum_map::drum_row_index(&self.drum_rows, pitch) {
                Some(row) => row as f32 * self.note_row_h() - self.scroll_y,
                None => -1.0e6,
            };
        }
        (PITCH_CNT - 1 - pitch as i32) as f32 * self.row_h - self.scroll_y
    }

    /// How many rows the pitch axis has: 128 keys, or the drum lanes.
    fn pitch_row_count(&self) -> usize {
        if self.drum_mode && !self.drum_rows.is_empty() {
            self.drum_rows.len()
        } else {
            PITCH_CNT as usize
        }
    }

    /// The row index (from the top) of `pitch`, fractional pitches included
    /// in piano mode.
    fn pitch_row_of(&self, pitch: f32) -> f32 {
        if self.drum_mode {
            let nearest = pitch.round().clamp(0.0, 127.0) as u8;
            if let Some(row) = drum_map::drum_row_index(&self.drum_rows, nearest) {
                return row as f32;
            }
        }
        (PITCH_CNT - 1) as f32 - pitch
    }

    /// `pitch` moved by a drag of `dpitch` (rows up, positive). The piano
    /// moves semitones under the scale lock; drum mode moves whole lanes.
    fn moved_pitch(&self, pitch: u8, dpitch: i32) -> u8 {
        if self.drum_mode {
            return drum_map::drum_move(&self.drum_rows, pitch, -dpitch);
        }
        let raw_pitch = (pitch as i32 + dpitch).clamp(0, 127) as u8;
        self.pitch_ctx.constrain_pitch(raw_pitch)
    }

    /// What a row is called in the lane list and the hover status: the
    /// track's instrument's name for it, the General MIDI name, or the note.
    fn drum_lane_name(&self, pitch: u8) -> SharedString {
        if let Some(name) = self.drum_names.get(&pitch) {
            return name.clone();
        }
        drum_map::gm_drum_name(pitch)
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(note_name(pitch as i32)))
    }

    /// Width of the key column: the keyboard, or the drum lane list, which is
    /// wider so a sound's name fits beside its note.
    pub(super) fn key_column_w(&self) -> f32 {
        if self.drum_mode {
            key_lane_width().max(DRUM_LANE_W)
        } else {
            key_lane_width()
        }
    }

    pub fn drum_mode(&self) -> bool {
        self.drum_mode
    }

    /// Switch between the piano roll and the drum editor. The view keeps the
    /// same notes; the row under the middle of the view is kept in view.
    pub fn set_drum_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.drum_mode == on {
            return;
        }
        self.drum_mode = on;
        self.refresh_drum_rows(cx);
        // Lanes are worth a taller row than semitones, and start at the top.
        if on {
            self.row_h = self.row_h.max(DRUM_ROW_H);
            self.scroll_y = 0.0;
        } else {
            self.scroll_to_pitch(60);
        }
        cx.notify();
    }

    pub fn set_drum_lanes(&mut self, lanes: DrumLanes, cx: &mut Context<Self>) {
        if self.drum_lanes == lanes {
            return;
        }
        self.drum_lanes = lanes;
        self.refresh_drum_rows(cx);
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll_y());
        cx.notify();
    }

    /// Rebuild the lanes from the edited clip: every note it plays has one.
    /// Names come from the track's Drum Sampler pads where it has one. Cheap
    /// enough per render: one pass over the clip's notes, only in drum mode.
    pub(super) fn refresh_drum_rows(&mut self, cx: &Context<Self>) {
        if !self.drum_mode {
            return;
        }
        let used: Vec<u8> = self
            .editing_clip_id(cx)
            .and_then(|clip_id| {
                self.timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(&clip_id)
                    .map(|notes| notes.iter().map(|note| note.pitch).collect())
            })
            .unwrap_or_default();
        self.drum_rows = drum_map::drum_rows(self.drum_lanes, &used);
        self.drum_names = self.drum_sampler_names(cx);
    }

    /// Pad names of the built-in Drum Sampler on the edited track, by note:
    /// each pad's sample, which is what the user loaded onto that note.
    fn drum_sampler_names(
        &self,
        cx: &Context<Self>,
    ) -> std::collections::HashMap<u8, SharedString> {
        let mut names = std::collections::HashMap::new();
        #[cfg(feature = "builtin-plugin-editor")]
        {
            let state = &self.timeline.read(cx).state;
            let Some(track) = self
                .editing_clip_id(cx)
                .and_then(|clip_id| state.find_clip(&clip_id).map(|(track, _)| track))
            else {
                return names;
            };
            for insert in track.inserts.iter().filter(|insert| !insert.is_empty()) {
                let Some(params) =
                    crate::components::builtin_plugin_editor::builtin_drum_sampler_params(
                        &insert.id,
                    )
                else {
                    continue;
                };
                for pad in params.pads.iter() {
                    if let Some(sample) = pad.sample_name.as_ref().filter(|name| !name.is_empty()) {
                        let label = std::path::Path::new(sample)
                            .file_stem()
                            .map(|stem| stem.to_string_lossy().into_owned())
                            .unwrap_or_else(|| sample.clone());
                        names
                            .entry(pad.note)
                            .or_insert_with(|| SharedString::from(label));
                    }
                }
            }
        }
        #[cfg(not(feature = "builtin-plugin-editor"))]
        let _ = cx;
        names
    }

    /// Current Zoom Y (px per semitone). Prefer this over the `DEFAULT_ROW_H`
    /// constant in render/hit-test paths so vertical zoom stays coherent.
    #[inline]
    fn note_row_h(&self) -> f32 {
        self.row_h.max(0.0001)
    }

    fn total_pitch_h(&self) -> f32 {
        self.pitch_row_count() as f32 * self.note_row_h()
    }

    fn point_to_beat_pitch(&self, local_x: f32, local_y: f32) -> (f32, u8) {
        (self.x_to_clip_beat(local_x), self.y_to_pitch(local_y))
    }

    fn rects_intersect(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
    }

    fn normalized_marquee_rect(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        view_w: f32,
        view_h: f32,
    ) -> (f32, f32, f32, f32) {
        let left = x0.min(x1).max(0.0);
        let top = y0.min(y1).max(0.0);
        let right = x0.max(x1).min(view_w);
        let bottom = y0.max(y1).min(view_h);
        (left, top, right, bottom)
    }

    fn apply_marquee_mode(
        before: &HashSet<u64>,
        hits: &HashSet<u64>,
        mode: MarqueeSelectionMode,
    ) -> HashSet<u64> {
        match mode {
            MarqueeSelectionMode::Replace => hits.clone(),
            MarqueeSelectionMode::Add => before.union(hits).copied().collect(),
            MarqueeSelectionMode::Toggle => before.symmetric_difference(hits).copied().collect(),
            MarqueeSelectionMode::Subtract => before.difference(hits).copied().collect(),
        }
    }

    fn begin_marquee_select(
        &mut self,
        lx: f32,
        ly: f32,
        mode: MarqueeSelectionMode,
        cx: &mut Context<Self>,
    ) {
        self.selection_before_marquee = self.selection.clone();
        self.drag = PianoDrag::MarqueeSelect {
            start_x: lx,
            start_y: ly,
            current_x: lx,
            current_y: ly,
            mode,
            dragging: false,
        };
        cx.notify();
    }

    fn update_marquee_select(&mut self, lx: f32, ly: f32, clip_id: &str, cx: &mut Context<Self>) {
        let (view_w, view_h) = self.grid_view_size();
        let clamped_x = lx.clamp(0.0, view_w);
        let clamped_y = ly.clamp(0.0, view_h);

        if let PianoDrag::MarqueeSelect {
            current_x,
            current_y,
            ..
        } = &mut self.drag
        {
            *current_x = clamped_x;
            *current_y = clamped_y;
        } else {
            return;
        }

        let (start_x, start_y, current_x, current_y, mode, was_dragging) = match &self.drag {
            PianoDrag::MarqueeSelect {
                start_x,
                start_y,
                current_x,
                current_y,
                mode,
                dragging,
            } => (*start_x, *start_y, *current_x, *current_y, *mode, *dragging),
            _ => return,
        };

        if !was_dragging {
            let dx = current_x - start_x;
            let dy = current_y - start_y;
            if (dx * dx + dy * dy).sqrt() < MARQUEE_DRAG_THRESHOLD {
                return;
            }
            if let PianoDrag::MarqueeSelect { dragging, .. } = &mut self.drag {
                *dragging = true;
            }
            if midi_debug_enabled() {
                eprintln!("[midi] marquee_start mode={}", mode.label());
            }
        }

        let marquee =
            Self::normalized_marquee_rect(start_x, start_y, current_x, current_y, view_w, view_h);
        let hits = self.marquee_hits(cx, clip_id, marquee);
        self.selection = Self::apply_marquee_mode(&self.selection_before_marquee, &hits, mode);

        if midi_debug_enabled() {
            let (min_beat, max_pitch) = self.point_to_beat_pitch(marquee.0, marquee.1);
            let (max_beat, min_pitch) = self.point_to_beat_pitch(marquee.2, marquee.3);
            let (min_pitch, max_pitch) = (min_pitch.min(max_pitch), min_pitch.max(max_pitch));
            let (min_beat, max_beat) = (min_beat.min(max_beat), min_beat.max(max_beat));
            eprintln!(
                "[midi] marquee_update beats={:.3}..{:.3} pitch={}..{} hits={}",
                min_beat,
                max_beat,
                min_pitch,
                max_pitch,
                hits.len()
            );
            eprintln!("[midi] marquee_mode mode={}", mode.label());
        }

        cx.notify();
    }

    fn commit_marquee_select(&mut self, cx: &mut Context<Self>) {
        let drag = std::mem::replace(&mut self.drag, PianoDrag::None);
        let PianoDrag::MarqueeSelect { dragging, mode, .. } = drag else {
            return;
        };

        if dragging {
            if midi_debug_enabled() {
                eprintln!("[midi] marquee_commit selected={}", self.selection.len());
            }
        } else if mode == MarqueeSelectionMode::Replace && !self.selection.is_empty() {
            // Click on empty grid without drag — clear selection.
            self.selection.clear();
            cx.notify();
        }

        self.selection_before_marquee.clear();
    }

    fn cancel_marquee_select(&mut self, cx: &mut Context<Self>) {
        if matches!(self.drag, PianoDrag::MarqueeSelect { .. }) {
            self.selection = self.selection_before_marquee.clone();
            self.selection_before_marquee.clear();
            self.drag = PianoDrag::None;
            cx.notify();
        }
    }

    fn cancel_active_gesture(&mut self, cx: &mut Context<Self>) {
        if self.piano_key_drag_active || self.key_lane_pressed_pitch.is_some() {
            eprintln!(
                "[PianoKeyPreview] cancel active={:?}",
                self.key_lane_pressed_pitch
            );
        }
        self.preview_all_notes_off("cancel", cx);
        if matches!(self.drag, PianoDrag::MarqueeSelect { .. }) {
            self.cancel_marquee_select(cx);
            return;
        }
        if matches!(self.drag, PianoDrag::VelocitySelect { .. }) {
            self.selection = self.selection_before_marquee.clone();
            self.selection_before_marquee.clear();
            self.drag = PianoDrag::None;
            cx.notify();
            return;
        }
        if matches!(self.drag, PianoDrag::CcSelect { .. }) {
            self.cc_selection = self.cc_selection_before_marquee.clone();
            self.cc_selection_before_marquee.clear();
            self.drag = PianoDrag::None;
            cx.notify();
            return;
        }
        if matches!(self.drag, PianoDrag::None) {
            return;
        }

        // Live value gestures write silently for responsive rendering. Restore
        // their drag-start snapshots without creating history or dirty state.
        match &self.drag {
            PianoDrag::Velocity { clip_id, prev, .. } => {
                let clip_id = clip_id.clone();
                let prev = prev.clone();
                self.with_timeline_silent(cx, |tl, _| {
                    if let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) {
                        restore_velocity_values(notes, &prev);
                    }
                });
            }
            PianoDrag::VelocityPaint {
                clip_id,
                original_notes,
                ..
            }
            | PianoDrag::VelocityLine {
                clip_id,
                original_notes,
                ..
            } => {
                let clip_id = clip_id.clone();
                let originals: Vec<(u64, u8)> = original_notes
                    .iter()
                    .map(|note| (note.id, note.velocity))
                    .collect();
                self.with_timeline_silent(cx, |tl, _| {
                    if let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) {
                        restore_velocity_values(notes, &originals);
                    }
                });
            }
            PianoDrag::CcPaint { .. } | PianoDrag::CcMove { .. } | PianoDrag::CcLine { .. } => {
                if let (Some(prev), Some((clip_id, kind))) =
                    (self.cc_edit_prev.take(), self.cc_edit_target.take())
                {
                    self.with_timeline_silent(cx, |tl, _| {
                        tl.state.set_controller_lane_points(&clip_id, kind, prev);
                    });
                }
            }
            PianoDrag::ArtMove { .. } => {
                if let (Some(prev), Some(clip_id)) =
                    (self.art_edit_prev.take(), self.editing_clip_id(cx))
                {
                    self.with_timeline_silent(cx, |tl, _| {
                        tl.state.set_midi_articulations(&clip_id, prev);
                    });
                }
            }
            _ => {}
        }
        self.drag = PianoDrag::None;
        self.erase_preview_ids.clear();
        self.cc_edit_prev = None;
        self.cc_edit_target = None;
        self.art_edit_prev = None;
        self.drag_value_status = None;
        cx.notify();
    }

    /// Resolve the local (grid-relative) cursor position from a window-space
    /// point using the bounds captured during paint. `None` until the first
    /// frame has laid the grid out.
    fn grid_local(&self, window_pos: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.grid_bounds.get()?;
        let ox: f32 = bounds.origin.x.into();
        let oy: f32 = bounds.origin.y.into();
        let x: f32 = window_pos.x.into();
        let y: f32 = window_pos.y.into();
        Some((x - ox, y - oy))
    }

    /// Content-space position of a window point for a note drag (see
    /// [`DragGrab`]). Before the grid's first paint there is no transform; the
    /// origin is returned, and the drag's delta stays zero until there is one.
    fn drag_grab_at(&self, window: (f32, f32)) -> DragGrab {
        let Some((lx, ly)) = self.grid_local(point(px(window.0), px(window.1))) else {
            return DragGrab {
                beat: 0.0,
                row: 0.0,
            };
        };
        DragGrab::at(
            lx,
            ly,
            self.scroll_x,
            self.scroll_y,
            self.ppb,
            self.note_row_h(),
        )
    }

    /// Re-derive a Move/Resize drag's live deltas from its last pointer
    /// position and the current view. Runs on every pointer move and on every
    /// render, so a scroll or zoom that moves the content under a still
    /// cursor (follow-playhead, the wheel) moves the dragged notes with the
    /// cursor rather than with the content. O(1): no per-note work.
    fn refresh_drag_from_pointer(&mut self) {
        let (grab, pointer, travel) = match &self.drag {
            PianoDrag::Move {
                grab,
                pointer,
                travel,
                ..
            }
            | PianoDrag::Resize {
                grab,
                pointer,
                travel,
                ..
            } => (*grab, *pointer, *travel),
            _ => return,
        };
        // Zero until the pointer itself has moved (#242): a scroll under a
        // still pointer is not a drag.
        let now = self.drag_grab_at(pointer);
        let (d_beats, d_rows) = note_drag_delta(travel, grab, now);
        let resized = match &self.drag {
            PianoDrag::Resize {
                prev_dur,
                anchor_start,
                unsnap,
                ..
            } => Some(resize_drag_duration(
                travel,
                grab,
                now,
                *anchor_start,
                *prev_dur,
                |edge| self.snap_beats_live(edge, *unsnap),
            )),
            _ => None,
        };
        match &mut self.drag {
            PianoDrag::Move {
                dx_beats, dpitch, ..
            } => {
                // The raw beat delta; snapping is applied to the grabbed
                // anchor in `display_note` / commit.
                *dx_beats = d_beats;
                *dpitch = d_rows;
            }
            PianoDrag::Resize {
                prev_dur,
                delta_dur,
                new_dur,
                ..
            } => {
                if let Some(d) = resized {
                    *delta_dur = d - *prev_dur;
                    *new_dur = d;
                }
            }
            _ => {}
        }
    }

    fn ruler_local(&self, window_pos: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.ruler_bounds.get()?;
        let ox: f32 = bounds.origin.x.into();
        let oy: f32 = bounds.origin.y.into();
        let x: f32 = window_pos.x.into();
        let y: f32 = window_pos.y.into();
        Some((x - ox, y - oy))
    }

    fn seek_ruler_at(&mut self, lx: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let (clip_start, clip_len) = self.clip_meta(cx, &clip_id);
        let rel_beat = self.x_to_clip_beat(lx).clamp(0.0, clip_len.max(0.0));
        let project_beat = clip_start + rel_beat;
        self.timeline
            .update(cx, |tl, tcx| tl.seek_to_beat(project_beat, tcx));
        self.hover_beat = Some(rel_beat);
        self.drag_value_status = Some(format!("Seek {:.2}", project_beat));
        cx.notify();
    }

    /// Pitch under a window-space point **iff** it is inside the left piano-key
    /// lane, else `None` (the cursor has left the lane → the scrub note must
    /// stop). Uses the exact same [`Self::y_to_pitch`] mapping as the note grid,
    /// so the keys and the grid rows can never drift apart — there is no
    /// second copy of the pitch math.
    fn key_lane_pitch_at(&self, window_pos: gpui::Point<Pixels>) -> Option<u8> {
        let bounds = self.key_lane_bounds.get()?;
        let ox: f32 = bounds.origin.x.into();
        let oy: f32 = bounds.origin.y.into();
        let w: f32 = bounds.size.width.into();
        let h: f32 = bounds.size.height.into();
        let local_x = f32::from(window_pos.x) - ox;
        let local_y = f32::from(window_pos.y) - oy;
        if local_x < 0.0 || local_x > w || local_y < 0.0 || local_y > h {
            return None;
        }
        Some(self.key_pitch_at_local(local_x, local_y, w))
    }

    /// The key under a point in the key lane, as it is drawn: black keys are
    /// [`BLACK_KEY_SHARE`] of the lane wide, and to their right a black row
    /// belongs to the white keys either side of it — its upper half to the key
    /// above, its lower half to the one below.
    fn key_pitch_at_local(&self, local_x: f32, local_y: f32, lane_w: f32) -> u8 {
        let pitch = self.y_to_pitch(local_y);
        if self.drum_mode {
            // A lane is the whole row: there are no black keys to split.
            return pitch;
        }
        keyboard_key_at(
            pitch,
            local_y - self.pitch_to_y(pitch),
            self.note_row_h(),
            local_x,
            lane_w,
        )
    }

    /// `true` when the edited clip sits on a Solfege track. The Solfege editor
    /// wraps this piano roll and supplies its own stacked performance lanes, so
    /// the built-in single unified lane stands down there to avoid showing the
    /// same parameter twice and to keep the grid the majority of the viewport.
    pub(super) fn editing_solfege_track(&self, cx: &Context<Self>) -> bool {
        let tl = self.timeline.read(cx);
        tl.state
            .selection
            .selected_clip_ids
            .first()
            .and_then(|clip_id| tl.state.find_clip(clip_id))
            .is_some_and(|(track, _)| track.solfege.is_some())
    }

    /// Whether the built-in unified controller lane is on screen this frame.
    ///
    /// The Solfege editor replaces the velocity and controller views with its
    /// own stacked performance lanes, so those stand down there. Articulations
    /// are the exception: direction articulations have no Solfege lane, and
    /// suppressing them left the whole `MidiArticulationEvent` model with no
    /// lane on exactly the tracks that perform articulations.
    pub(super) fn unified_lane_visible(&self, cx: &Context<Self>) -> bool {
        self.lane_visible
            && (self.lane_view == PianoLaneView::Articulations || !self.editing_solfege_track(cx))
    }

    /// Articulations the editing track can play. A Solfege track answers from
    /// its instrument capability table (spec: "available articulations must
    /// come from the loaded instrument"); every other track keeps the built-in
    /// set.
    pub(super) fn available_articulations(&self, cx: &Context<Self>) -> Vec<ArticulationId> {
        let tl = self.timeline.read(cx);
        tl.state
            .selection
            .selected_clip_ids
            .first()
            .and_then(|clip_id| tl.state.find_clip(clip_id))
            .and_then(|(track, _)| track.solfege.as_ref())
            .map(|solfege| solfege.capabilities().articulations)
            .unwrap_or_else(|| ArticulationId::ALL.to_vec())
    }

    /// Read-only snapshot of the shared MIDI timeline viewport.
    ///
    /// The piano roll is the single horizontal-scroll owner for the whole
    /// Solfege editor: the performance lane stack and the Pitch tab render
    /// through this transform instead of keeping their own scroll, which is
    /// what keeps every surface sample-aligned with the notes.
    pub fn viewport(&self) -> PianoRollViewport {
        let (grid_width, grid_height) = self.grid_view_size();
        PianoRollViewport {
            ppb: self.ppb.max(0.0001),
            scroll_x: self.scroll_x.max(0.0),
            row_h: self.note_row_h(),
            scroll_y: self.scroll_y.max(0.0),
            key_lane_width: self.key_column_w(),
            snap_step_beats: viewport_snap_step(
                self.snap_on,
                self.grid_res,
                self.ppb,
                self.bar_beats(),
            ),
            snap_to_bars: self.snap_on && self.grid_res == GridRes::Bar,
            sub_step_beats: self.step_beats().max(MIN_NOTE_BEATS),
            grid_width,
            grid_height,
        }
    }

    /// Scroll the shared viewport horizontally by `dx` pixels, clamped exactly
    /// as the piano roll's own drag-scroll does.
    pub fn scroll_viewport_by(&mut self, dx: f32, cx: &Context<Self>) {
        let max_scroll_x = self.max_scroll_x(cx);
        self.scroll_x = (self.scroll_x - dx).clamp(0.0, max_scroll_x);
    }

    /// Scroll the shared viewport vertically by `dy` pixels.
    ///
    /// `view_h` is the height of the surface doing the scrolling, because the
    /// scroll limit depends on it. The Pitch tab must pass its own canvas
    /// height: the piano roll is not even mounted while that tab is on screen,
    /// so its captured grid bounds are stale, and clamping against them would
    /// leave the lowest pitches unreachable (or over-scroll past the last row)
    /// depending on what the MIDI tab last painted.
    pub fn scroll_viewport_vertically_by(&mut self, dy: f32, view_h: f32) {
        self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll_y_for(view_h));
    }

    /// Change Zoom Y, keeping the pitch under `anchor_y` fixed.
    ///
    /// `view_h` and `anchor_y` belong to the surface being zoomed, for the same
    /// reason as [`Self::scroll_viewport_vertically_by`]. Anchoring on the
    /// cursor rather than the view centre is also what keeps Ctrl+wheel from
    /// drifting the view sideways in pitch.
    pub fn zoom_viewport_vertically(&mut self, factor: f32, view_h: f32, anchor_y: f32) {
        let anchor_y = anchor_y.clamp(0.0, view_h.max(1.0));
        if self.drum_mode {
            // Lanes are rows, not pitches: keep the same row fraction under
            // the cursor.
            let anchor_row = (anchor_y + self.scroll_y) / self.note_row_h().max(0.0001);
            let new_row_h =
                (self.note_row_h() * factor).clamp(PIANO_ROLL_MIN_ROW_H, PIANO_ROLL_MAX_ROW_H);
            self.row_h = new_row_h;
            self.scroll_y =
                (anchor_row * new_row_h - anchor_y).clamp(0.0, self.max_scroll_y_for(view_h));
            return;
        }
        let anchor_pitch = local_y_to_pitch_f32(anchor_y, self.scroll_y, self.note_row_h());
        let new_row_h =
            (self.note_row_h() * factor).clamp(PIANO_ROLL_MIN_ROW_H, PIANO_ROLL_MAX_ROW_H);
        self.row_h = new_row_h;
        // Re-solve scroll_y so the same pitch stays under the cursor.
        let target_top = (PITCH_CNT - 1) as f32 * new_row_h - anchor_pitch * new_row_h;
        self.scroll_y =
            (target_top + new_row_h * 0.5 - anchor_y).clamp(0.0, self.max_scroll_y_for(view_h));
    }

    /// Zoom Y and scroll so `[min_pitch, max_pitch]` fills `view_h` with the
    /// same edge padding [`Self::fit_piano_roll_to_notes`] uses horizontally.
    ///
    /// `view_h` belongs to the calling surface, for the same reason as
    /// [`Self::scroll_viewport_vertically_by`] — this is what backs the Pitch
    /// tab's own Fit button, and the piano roll is not mounted while that tab
    /// is showing.
    pub fn fit_viewport_vertically(&mut self, view_h: f32, min_pitch: f32, max_pitch: f32) {
        if view_h <= 1.0 {
            return;
        }
        // Measured in rows: one per semitone, or one per drum lane.
        let span = (self.pitch_row_of(min_pitch) - self.pitch_row_of(max_pitch))
            .abs()
            .max(1.0);
        let fit_h = (view_h - PIANO_ROLL_FIT_PAD_PX * 2.0).max(24.0);
        let new_row_h = (fit_h / span).clamp(PIANO_ROLL_MIN_ROW_H, PIANO_ROLL_MAX_ROW_H);
        self.row_h = new_row_h;
        let anchor_row = (self.pitch_row_of(min_pitch) + self.pitch_row_of(max_pitch)) * 0.5;
        let target_top = anchor_row * new_row_h;
        self.scroll_y =
            (target_top + new_row_h * 0.5 - view_h * 0.5).clamp(0.0, self.max_scroll_y_for(view_h));
    }

    fn grid_view_size(&self) -> (f32, f32) {
        match self.grid_bounds.get() {
            Some(b) => (
                f32::from(b.size.width).max(1.0),
                f32::from(b.size.height).max(1.0),
            ),
            None => (600.0, 200.0),
        }
    }

    fn max_scroll_y(&self) -> f32 {
        let (_, h) = self.grid_view_size();
        self.max_scroll_y_for(h)
    }

    /// Scroll limit for a surface `view_h` pixels tall.
    fn max_scroll_y_for(&self, view_h: f32) -> f32 {
        (self.total_pitch_h() - view_h.max(1.0)).max(0.0)
    }

    /// How far the view can scroll, in project pixels.
    ///
    /// The whole track, not the edited clip: the editor shows every clip on it,
    /// and bounding the scroll to one of them would make the rest unreachable.
    /// A little air past the last clip, so its final bar is not pinned to the
    /// right edge with nothing after it.
    fn max_scroll_x(&self, _cx: &Context<Self>) -> f32 {
        let (view_w, _) = self.grid_view_size();
        let Some((_, end)) = self.scope.extent() else {
            return 0.0;
        };
        const TRAILING_BEATS: f32 = 4.0;
        ((end + TRAILING_BEATS) * self.ppb - view_w).max(0.0)
    }

    fn clip_meta(&self, cx: &Context<Self>, clip_id: &str) -> (f32, f32) {
        let tl = self.timeline.read(cx);
        for track in &tl.state.tracks {
            for clip in &track.clips {
                if clip.id == clip_id {
                    return (clip.start_beat, clip.duration_beats);
                }
            }
        }
        (0.0, 4.0)
    }

    /// Scroll the pitch axis so `pitch` is vertically centered in the view.
    fn scroll_to_pitch(&mut self, pitch: u8) {
        let (_, view_h) = self.grid_view_size();
        let row_h = self.note_row_h();
        let target = self.pitch_row_of(pitch as f32) * row_h - view_h * 0.5 + row_h * 0.5;
        self.scroll_y = target.clamp(0.0, self.max_scroll_y());
    }

    /// Scroll/zoom the grid so selected notes (or all notes) are visible.
    fn fit_piano_roll_to_notes(&mut self, cx: &Context<Self>, clip_id: &str) {
        let (view_w, view_h) = self.grid_view_size();
        if view_w <= 1.0 || view_h <= 1.0 {
            return;
        }

        let (notes, selected): (Vec<MidiNoteState>, HashSet<u64>) = {
            let tl = self.timeline.read(cx);
            let notes = tl
                .state
                .midi_clip_notes(clip_id)
                .cloned()
                .unwrap_or_default();
            (notes, self.selection.clone())
        };

        let target_notes: Vec<&MidiNoteState> = if !selected.is_empty() {
            notes.iter().filter(|n| selected.contains(&n.id)).collect()
        } else {
            notes.iter().collect()
        };

        let (min_p, max_p) = if target_notes.is_empty() {
            (60u8, 60u8)
        } else {
            let lo = target_notes.iter().map(|n| n.pitch).min().unwrap_or(60);
            let hi = target_notes.iter().map(|n| n.pitch).max().unwrap_or(60);
            if self.drum_mode {
                // Every played note is a lane; a padded pitch may not be.
                (lo, hi)
            } else {
                (lo.saturating_sub(6), hi.saturating_add(6))
            }
        };

        let row_h = self.note_row_h();
        // The middle row between the extremes: in drum mode the lanes are
        // not one semitone apart, so it is found by row, not by pitch.
        let mid_row = (self.pitch_row_of(min_p as f32) + self.pitch_row_of(max_p as f32)) * 0.5;
        let target_scroll = if self.drum_mode {
            // A kit reads from its first played lane down — the kick and
            // snare at the top, not centred out of view.
            (self.pitch_row_of(min_p as f32) - 1.0).max(0.0) * row_h
        } else {
            mid_row * row_h - view_h * 0.5 + row_h * 0.5
        };
        self.scroll_y = target_scroll.clamp(0.0, self.max_scroll_y());

        let (_, clip_len) = self.clip_meta(cx, clip_id);
        if !target_notes.is_empty() {
            let min_start = target_notes
                .iter()
                .map(|n| n.start)
                .fold(f32::INFINITY, f32::min)
                .max(0.0);
            let max_end = target_notes
                .iter()
                .map(|n| n.start + n.duration)
                .fold(0.0_f32, f32::max)
                .min(clip_len.max(0.0));
            let span = (max_end - min_start).max(1.0);
            let fit_w = (view_w - PIANO_ROLL_FIT_PAD_PX * 2.0).max(96.0);
            self.ppb = (fit_w / span).clamp(PIANO_ROLL_MIN_PPB, PIANO_ROLL_MAX_PPB);
            // The axis is project beats, so a clip-local note start has to be
            // placed by the clip's origin before it can be scrolled to.
            self.scroll_x =
                ((self.edit_origin() + min_start) * self.ppb - PIANO_ROLL_FIT_PAD_PX).max(0.0);
        } else {
            let fit_w = (view_w - PIANO_ROLL_FIT_PAD_PX * 2.0).max(96.0);
            self.ppb = (fit_w / clip_len.max(1.0)).clamp(PIANO_ROLL_MIN_PPB, PIANO_ROLL_MAX_PPB);
            self.scroll_x = (self.edit_origin() * self.ppb - PIANO_ROLL_FIT_PAD_PX).max(0.0);
        }

        if midi_debug_enabled() {
            eprintln!(
                "[midi] piano_roll fit clip={} pitch={}..{} notes={}",
                clip_id,
                min_p,
                max_p,
                target_notes.len()
            );
        }
    }

    // ── Mutations through the timeline ────────────────────────────────────
    /// Full snapshots of the given note ids in a clip (undo prev/next state).
    fn snapshot_notes(&self, cx: &Context<Self>, clip_id: &str, ids: &[u64]) -> Vec<MidiNoteState> {
        // A set, not `ids.contains`: select-all on a dense clip made this
        // notes × selection on every commit.
        let ids: std::collections::HashSet<u64> = ids.iter().copied().collect();
        self.timeline
            .read(cx)
            .state
            .midi_clip_notes(clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| ids.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Apply an in-place note transform (move / resize / quantize / transpose /
    /// nudge / bulk velocity) and record it as one undoable `EditMidiNotes`
    /// command. Captures full prev/next snapshots of `ids`; a no-op
    /// (`prev == next`) is dropped without dirtying the project.
    fn commit_note_transform<F>(&mut self, cx: &mut Context<Self>, ids: &[u64], mutate: F)
    where
        F: FnOnce(&mut TimelineState, &str),
    {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        if ids.is_empty() {
            return;
        }
        let prev = self.snapshot_notes(cx, &clip_id, ids);
        self.timeline.update(cx, |tl, tcx| {
            mutate(&mut tl.state, &clip_id);
            tcx.notify();
        });
        let next = self.snapshot_notes(cx, &clip_id, ids);
        self.push_note_edit(cx, clip_id, prev, next);
    }

    /// Record an `EditMidiNotes` command for an already-applied transform,
    /// skipping no-ops. Logs the commit for the floating-editor debug sink.
    fn push_note_edit(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: String,
        prev: Vec<MidiNoteState>,
        next: Vec<MidiNoteState>,
    ) {
        if prev == next {
            return;
        }
        // A note dragged past the edge of its clip belongs to whichever clip it
        // landed in. Handled here because this is the one place every move and
        // resize passes through, so there is no gesture that can quietly leave
        // a note outside the clip that owns it.
        if self.migrate_notes_that_left_the_clip(cx, &clip_id, &prev, &next) {
            return;
        }
        self.timeline.update(cx, |tl, tcx| {
            tl.record_executed_command(
                EditCommand::EditMidiNotes {
                    clip_id,
                    prev,
                    next,
                },
                tcx,
            );
        });
        if self.midi_editor_sink {
            crate::components::midi_editor_window::midi_editor_debug("edit command committed");
        }
    }

    /// Reset one or all note-owned expression lanes as a single undoable
    /// operation. The existing full-note snapshot command is intentionally
    /// reused so expression edits cannot bypass the normal dirty/undo path.
    pub(super) fn reset_selected_expression(
        &mut self,
        lane: Option<NoteExpressionLane>,
        cx: &mut Context<Self>,
    ) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids: Vec<u64> = self.selection.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let prev = self.snapshot_notes(cx, &clip_id, &ids);
        self.timeline.update(cx, |tl, tcx| {
            if let Some(notes) = tl.state.midi_clip_notes_mut(&clip_id) {
                for note in notes.iter_mut() {
                    if !ids.contains(&note.id) {
                        continue;
                    }
                    if let Some(lane) = lane {
                        match lane {
                            NoteExpressionLane::Pitch => note.expression.pitch.points.clear(),
                            NoteExpressionLane::Pressure => note.expression.pressure.points.clear(),
                            NoteExpressionLane::Timbre => note.expression.timbre.points.clear(),
                        }
                    } else {
                        note.expression = Default::default();
                        note.release_velocity = None;
                    }
                }
            }
            tcx.notify();
        });
        let next = self.snapshot_notes(cx, &clip_id, &ids);
        self.push_note_edit(cx, clip_id, prev, next);
        cx.notify();
    }

    /// Hand any note that left the edited clip to the clip it landed in.
    ///
    /// Returns `true` when it recorded the move, in which case the caller must
    /// not also record an `EditMidiNotes` — the move command already carries
    /// both clips' full before and after.
    ///
    /// Notes that land on no clip at all are left where they are rather than
    /// deleted. Dragging into the gap between two clips is a miss, not an
    /// instruction to discard the note, and the user can drag it back.
    fn migrate_notes_that_left_the_clip(
        &mut self,
        cx: &mut Context<Self>,
        clip_id: &str,
        prev: &[MidiNoteState],
        next: &[MidiNoteState],
    ) -> bool {
        let Some(source) = self.scope.span(clip_id).cloned() else {
            return false;
        };

        // Which notes ended up somewhere else, and where.
        let mut moves: Vec<(MidiNoteState, String, f32)> = Vec::new();
        for note in next {
            let project_start = source.to_project(note.start);
            let Some(owner) = self.scope.owner_at(project_start) else {
                continue;
            };
            if owner.clip_id == source.clip_id {
                continue;
            }
            moves.push((note.clone(), owner.clip_id.clone(), owner.start_beat));
        }
        if moves.is_empty() {
            return false;
        }

        let moved_ids: std::collections::HashSet<u64> =
            moves.iter().map(|(note, _, _)| note.id).collect();

        // One destination per gesture: a multi-note drag that straddles two
        // neighbours would need one command per destination, and splitting a
        // single drag across several undo entries is worse than keeping the
        // stragglers put. The first destination wins and the rest stay.
        let (_, to_clip_id, to_origin) = moves[0].clone();

        let (from_prev, to_prev) = {
            let tl = self.timeline.read(cx);
            (
                tl.state
                    .midi_clip_notes(clip_id)
                    .cloned()
                    .unwrap_or_default(),
                tl.state
                    .midi_clip_notes(&to_clip_id)
                    .cloned()
                    .unwrap_or_default(),
            )
        };

        let from_next: Vec<MidiNoteState> = next
            .iter()
            .filter(|note| !moved_ids.contains(&note.id))
            .cloned()
            .collect();

        let mut to_next = to_prev.clone();
        for (note, destination, origin) in &moves {
            if destination != &to_clip_id {
                continue;
            }
            let mut moved = note.clone();
            // Rebased into the destination's frame: the note keeps the place on
            // the timeline the user dropped it at, which is the whole point of
            // the gesture.
            moved.start = source.to_project(note.start) - origin;
            to_next.push(moved);
        }

        // Anything bound for a second destination stays where it was.
        let stayed: Vec<MidiNoteState> = moves
            .iter()
            .filter(|(_, destination, _)| destination != &to_clip_id)
            .filter_map(|(note, _, _)| prev.iter().find(|p| p.id == note.id).cloned())
            .collect();
        let from_next: Vec<MidiNoteState> = from_next.into_iter().chain(stayed).collect();

        let _ = to_origin;
        if midi_debug_enabled() {
            eprintln!(
                "[MidiEditor] migrate {} note(s) {clip_id} -> {to_clip_id}",
                to_next.len().saturating_sub(to_prev.len())
            );
        }

        let command = EditCommand::MoveMidiNotesBetweenClips {
            from_clip_id: clip_id.to_string(),
            to_clip_id,
            from_prev,
            from_next,
            to_prev,
            to_next,
        };
        self.run_edit_command(command, cx);
        // The moved notes now live under new ids' clip; a selection pointing
        // into the old one would delete the wrong thing.
        self.selection.clear();
        true
    }

    /// Mutate the timeline for a *live* gesture (drag in progress): repaint but
    /// do NOT mark the project dirty, so we don't rebuild the engine snapshot on
    /// every mouse-move. The owning gesture marks dirty once on release.
    fn with_timeline_silent<R>(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Timeline, &mut Context<Timeline>) -> R,
    ) -> R {
        self.timeline.update(cx, |tl, tcx| {
            let r = f(tl, tcx);
            tcx.notify();
            r
        })
    }

    fn run_edit_command(&mut self, cmd: EditCommand, cx: &mut Context<Self>) {
        self.timeline.update(cx, |tl, tcx| {
            tl.run_edit_command(cmd, tcx);
        });
        if self.midi_editor_sink {
            crate::components::midi_editor_window::midi_editor_debug("edit command committed");
        }
    }

    fn note_at_grid(&self, cx: &Context<Self>, clip_id: &str, lx: f32, ly: f32) -> Option<u64> {
        let (view_w, view_h) = self.grid_view_size();
        let rect = (
            (lx - 2.0).max(0.0),
            (ly - 2.0).max(0.0),
            (lx + 2.0).min(view_w),
            (ly + 2.0).min(view_h),
        );
        self.collect_notes_in_rect(cx, clip_id, rect)
            .into_iter()
            .next()
    }

    fn collect_notes_in_rect(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
        rect: (f32, f32, f32, f32),
    ) -> HashSet<u64> {
        let tl = self.timeline.read(cx);
        let Some(notes) = tl.state.midi_clip_notes(clip_id) else {
            return HashSet::new();
        };
        notes
            .iter()
            .filter(|n| self.channel_visible(n.channel))
            .filter(|n| Self::rects_intersect(rect, self.note_to_rect(&self.display_note(n))))
            .map(|n| n.id)
            .collect()
    }

    // ── Mouse handlers ─────────────────────────────────────────────────────
    // Notes are interactive elements that handle their own select/move/resize/
    // delete (and stop propagation), so the grid surface only deals with empty
    // space: create a note (Draw tool) or clear the selection (Select tool).
    /// Resolve the clips on screen and the edited clip's origin.
    ///
    /// Called before anything that measures a beat — including
    /// `fit_piano_roll_to_notes`, which places its scroll through the origin
    /// and was previously fitting a new clip against the previous one's.
    pub(super) fn refresh_scope(&mut self, cx: &Context<Self>, clip_id: &str) {
        self.scope = scope::EditorScope::for_editing_clip(&self.timeline.read(cx).state, clip_id);
        self.edit_origin_beats = self
            .scope
            .editing()
            .map(|span| span.start_beat)
            .unwrap_or(0.0);
    }

    /// Make the clip under `lx` the one being edited, if it is not already.
    ///
    /// This is what turns the neighbours from a picture into the rest of the
    /// editor. The alternative — teaching all forty edit sites to address a
    /// clip resolved per gesture — would be the same behaviour with forty
    /// chances to address the wrong one.
    ///
    /// The note selection is dropped on the way across, because note ids belong
    /// to their clip: keeping them would leave a selection pointing at notes
    /// that are no longer on screen and would be deleted by the next Delete.
    ///
    /// Returns `true` when it moved, and the caller starts the gesture on the
    /// next press: the scope and the origin are only refreshed by a render, so
    /// continuing here would measure the new clip against the old frame.
    pub(super) fn retarget_to_clip_under(&mut self, lx: f32, cx: &mut Context<Self>) -> bool {
        let project_beat = self.x_to_project_beat(lx);
        let Some(owner) = self.scope.owner_at(project_beat) else {
            return false;
        };
        if owner.editable {
            return false;
        }
        let clip_id = owner.clip_id.clone();
        if midi_debug_enabled() {
            eprintln!("[MidiEditor] retarget to clip {clip_id} at beat {project_beat:.3}");
        }
        self.selection.clear();
        self.drag = PianoDrag::None;
        self.timeline.update(cx, |tl, tcx| {
            tl.state.select_clip(&clip_id);
            tcx.notify();
        });
        cx.notify();
        true
    }

    fn on_grid_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _scope = crate::perf::PerfScope::enter("PianoRoll::grid_down");
        cx.stop_propagation();
        // Any grid interaction dismisses the lane selector dropdown and the
        // note context menu.
        self.open_select_menu = None;
        self.open_note_menu = None;
        window.focus(&self.focus, cx);
        let Some((lx, ly)) = self.grid_local(event.position) else {
            // Bounds not captured yet (first frame) — ignore to avoid creating
            // a note at the wrong coordinate.
            return;
        };
        // Pressing inside a neighbouring clip moves the editor there rather
        // than doing nothing, which is what makes every clip on the track
        // reachable without going back to the arrangement to select it.
        if self.retarget_to_clip_under(lx, cx) {
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };

        let marquee_modifier = (event.modifiers.shift && self.tool != PianoTool::Draw)
            || event.modifiers.control
            || event.modifiers.platform
            || event.modifiers.alt;

        // A held modifier always means marquee select, whatever the active tool.
        if marquee_modifier {
            let mode = MarqueeSelectionMode::from_modifiers(&event.modifiers);
            self.begin_marquee_select(lx, ly, mode, cx);
            return;
        }

        match self.tool {
            PianoTool::Draw => {
                let pitch = if self.drum_mode {
                    // Lanes are notes of the kit; the scale lock has no say.
                    self.y_to_pitch(ly)
                } else {
                    self.pitch_ctx.constrain_pitch(self.y_to_pitch(ly))
                };
                let unsnap = event.modifiers.shift;
                let start = if self.drum_mode {
                    // A hit lands at the start of the step it was clicked in,
                    // the way a step sequencer fills a cell.
                    self.drum_step_start(self.x_to_clip_beat(lx), unsnap)
                } else {
                    self.snap_beats_live(self.x_to_clip_beat(lx), unsnap)
                };
                if midi_debug_enabled() {
                    if let Some((track_id, channel)) = self.preview_target(cx) {
                        eprintln!(
                            "[MidiEditor] draw_start pitch={} velocity=100 track_id={} channel={}",
                            pitch, track_id, channel
                        );
                    }
                }
                self.begin_preview_note(pitch, 100, "draw_start", cx);
                let channel = self.active_note_channel(cx);
                self.drag = PianoDrag::DrawNote {
                    pitch,
                    start_beat: start,
                    end_beat: start,
                    unsnap,
                    channel,
                };
                cx.notify();
            }
            PianoTool::Select => {
                self.begin_marquee_select(lx, ly, MarqueeSelectionMode::Replace, cx);
            }
            PianoTool::Erase => {
                // Begin an erase drag from empty space (sweeps notes like the
                // right-drag erase).
                let mut erased = HashSet::new();
                if let Some(id) = self.note_at_grid(cx, &clip_id, lx, ly) {
                    erased.insert(id);
                }
                self.erase_preview_ids = erased.clone();
                self.drag = PianoDrag::EraseNotes {
                    start_x: lx,
                    start_y: ly,
                    current_x: lx,
                    current_y: ly,
                    erased,
                };
                cx.notify();
            }
            // Line/Split/Mute act on lane values or notes only.
            PianoTool::Line | PianoTool::Split | PianoTool::Mute => {}
        }
    }

    fn on_grid_right_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        self.open_note_menu = None;
        let Some((lx, ly)) = self.grid_local(event.position) else {
            return;
        };
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let mut erased = HashSet::new();
        if let Some(id) = self.note_at_grid(cx, &clip_id, lx, ly) {
            erased.insert(id);
        }
        self.erase_preview_ids = erased.clone();
        self.drag = PianoDrag::EraseNotes {
            start_x: lx,
            start_y: ly,
            current_x: lx,
            current_y: ly,
            erased,
        };
        cx.notify();
    }

    /// Right-click on a note opens its context menu (articulation, mute,
    /// velocity, delete) rather than erasing it. Erasing was a destructive
    /// surprise with no confirmation; the erase sweep still lives on
    /// right-drag over empty grid, on the Erase tool, and on Delete.
    fn note_right_down(
        &mut self,
        id: u64,
        lx: f32,
        ly: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing_clip_id(cx).is_none() {
            return;
        }
        // The menu is dismissed with Escape, which the grid's own key handler
        // owns — so the press has to take focus like every other grid gesture.
        window.focus(&self.focus, cx);
        // Right-clicking outside the current selection retargets it, so the
        // menu always acts on what the user pointed at.
        if !self.selection.contains(&id) {
            self.selection = HashSet::from([id]);
        }
        self.open_velocity_menu = None;
        self.open_select_menu = None;
        if self.open_native_note_menu(window, cx) {
            cx.notify();
            return;
        }
        self.open_note_menu = Some((lx, ly));
        cx.notify();
    }
    fn note_mouse_down(
        &mut self,
        id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // In drum mode the Draw tool toggles: a click on a hit removes it, as
        // a click on an empty step adds one. Shift and Ctrl still select.
        if self.drum_mode
            && self.tool == PianoTool::Draw
            && !event.modifiers.shift
            && !event.modifiers.control
            && !event.modifiers.platform
        {
            self.erase_note(id, cx);
            return;
        }
        // Tool-specific note actions take precedence over select/move.
        match self.tool {
            PianoTool::Erase => {
                self.erase_note(id, cx);
                return;
            }
            PianoTool::Mute => {
                self.mute_note(id, cx);
                return;
            }
            PianoTool::Split => {
                if let Some((lx, _)) = self.grid_local(event.position) {
                    let beat = self.x_to_clip_beat(lx);
                    self.split_note(id, beat, cx);
                }
                return;
            }
            PianoTool::Draw | PianoTool::Select | PianoTool::Line => {}
        }
        let shift = event.modifiers.shift;
        let ctrl = event.modifiers.control || event.modifiers.platform;
        let clone_on_commit = event.modifiers.alt;
        if ctrl {
            // Toggle this note in/out of the selection — no drag.
            if self.selection.contains(&id) {
                self.selection.remove(&id);
            } else {
                self.selection.insert(id);
            }
            cx.notify();
            return;
        }
        // Shift adds the note to the selection and starts a free move: the
        // drag ignores the grid for as long as Shift is held. A Shift-click
        // that never moves still toggles the note, on release.
        let mut deselect_on_click = None;
        if shift {
            if self.selection.contains(&id) {
                deselect_on_click = Some(id);
            } else {
                self.selection.insert(id);
            }
        } else if !self.selection.contains(&id) {
            self.selection = HashSet::from([id]);
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        // Anchor pitch/velocity of the grabbed note for the live audition.
        let (grab_pitch, grab_vel, anchor_start) = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|notes| notes.iter().find(|n| n.id == id))
            .map(|n| (n.pitch, n.velocity, n.start))
            .unwrap_or((60, DEFAULT_NOTE_VELOCITY, 0.0));
        let prev = self.snapshot_selection(cx, &clip_id);
        let ids = prev.iter().map(|(id, _, _)| *id).collect();
        let pointer = (event.position.x.into(), event.position.y.into());
        self.drag = PianoDrag::Move {
            grab: self.drag_grab_at(pointer),
            pointer,
            travel: PointerTravel::new(pointer),
            prev,
            ids,
            dx_beats: 0.0,
            dpitch: 0,
            grab_pitch,
            anchor_start,
            clone_on_commit,
            unsnap: shift,
            deselect_on_click,
        };
        // Audition the grabbed pitch immediately; on_move switches it as the
        // drag changes pitch, on_up / cancel stops it.
        self.begin_preview_note(grab_pitch, grab_vel, "note_move_start", cx);
        cx.notify();
    }

    /// Right-edge handle mouse-down: begin a resize drag.
    fn begin_resize_drag(
        &mut self,
        id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let multi = self.selection.len() > 1 && self.selection.contains(&id);
        // Alt on the end of a note in a selection stretches the selection
        // instead of lengthening every note by the same amount.
        if multi && event.modifiers.alt {
            self.begin_stretch_drag(StretchAnchor::Start, event, window, cx);
            return;
        }
        if !multi {
            self.selection = HashSet::from([id]);
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let selected = self.selection.clone();
        let prev_durs: HashMap<u64, f32> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| selected.contains(&n.id))
                    .map(|n| (n.id, n.duration))
                    .collect()
            })
            .unwrap_or_default();
        let (anchor_start, prev_dur) = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|notes| notes.iter().find(|note| note.id == id))
            .map(|note| (note.start, note.duration))
            .unwrap_or((0.0, self.step_beats()));
        let ids: Vec<u64> = prev_durs.keys().copied().collect();
        let pointer = (event.position.x.into(), event.position.y.into());
        self.drag = PianoDrag::Resize {
            id,
            ids,
            grab: self.drag_grab_at(pointer),
            pointer,
            travel: PointerTravel::new(pointer),
            prev_dur,
            prev_durs,
            anchor_start,
            delta_dur: 0.0,
            new_dur: prev_dur,
            unsnap: event.modifiers.shift,
        };
        cx.notify();
    }

    /// Velocity bar mouse-down: begin a snapshot-based drag. Ctrl/Cmd toggles
    /// note selection; Shift adds an unselected note, while Shift-dragging an
    /// already selected note performs fine relative adjustment. Alt is absolute.
    fn begin_velocity_drag(
        &mut self,
        id: u64,
        orig_vel: u8,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_velocity_menu = None;
        window.focus(&self.focus, cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let toggle = event.modifiers.control || event.modifiers.platform;
        if toggle {
            if self.selection.contains(&id) {
                self.selection.remove(&id);
            } else {
                self.selection.insert(id);
            }
            cx.notify();
            return;
        }
        if event.modifiers.shift && !self.selection.contains(&id) {
            self.selection.insert(id);
            cx.notify();
            return;
        }
        if event.click_count >= 2 {
            if !self.selection.contains(&id) {
                self.selection = HashSet::from([id]);
            }
            let ids = self.selected_note_ids();
            self.commit_note_transform(cx, &ids, |state, cid| {
                for note_id in &ids {
                    state.set_midi_note_velocity(cid, *note_id, DEFAULT_NOTE_VELOCITY);
                }
            });
            self.drag_value_status = Some(format!(
                "Velocity: {} · {} note{}",
                DEFAULT_NOTE_VELOCITY,
                ids.len(),
                plural(ids.len())
            ));
            cx.notify();
            return;
        }
        if !self.selection.contains(&id) {
            self.selection = HashSet::from([id]);
        }
        let selected = &self.selection;
        let prev: Vec<(u64, u8)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| selected.contains(&n.id))
                    .map(|n| (n.id, n.velocity))
                    .collect()
            })
            .unwrap_or_else(|| vec![(id, orig_vel)]);
        self.drag_value_status = Some(if prev.len() == 1 {
            format!("Velocity: {orig_vel} · Δ+0")
        } else {
            format!("Anchor {orig_vel} · Δ+0 · {} notes", prev.len())
        });
        self.drag = PianoDrag::Velocity {
            clip_id,
            prev,
            anchor_orig: orig_vel,
            start_mouse_y: event.position.y.into(),
            absolute: event.modifiers.alt,
            fine: event.modifiers.shift,
        };
        cx.notify();
    }

    fn velocity_from_window_y(&self, position: gpui::Point<Pixels>) -> u8 {
        let local_y = self.cc_local(position).map(|(_, y)| y).unwrap_or_else(|| {
            self.cc_bounds
                .get()
                .map(|bounds| {
                    let origin_y: f32 = bounds.origin.y.into();
                    let y: f32 = position.y.into();
                    y - origin_y
                })
                .unwrap_or(LANE_H * 0.5)
        });
        let (_, lane_h) = self.cc_view_size();
        let usable_h = (lane_h - 8.0).max(1.0);
        let norm = (1.0 - ((local_y - 2.0) / usable_h)).clamp(0.0, 1.0);
        (1.0 + norm * 126.0).round().clamp(1.0, 127.0) as u8
    }

    fn apply_velocity_absolute(
        &mut self,
        clip_id: &str,
        prev: &[(u64, u8)],
        value: u8,
        cx: &mut Context<Self>,
    ) {
        if prev.is_empty() {
            return;
        }
        self.drag_value_status = Some(format!(
            "Velocity: {value} · absolute · {} note{}",
            prev.len(),
            plural(prev.len())
        ));
        self.with_timeline_silent(cx, |tl, _| {
            for (id, _) in prev {
                tl.state.set_midi_note_velocity(clip_id, *id, value);
            }
        });
    }

    fn apply_velocity_relative(
        &mut self,
        clip_id: &str,
        prev: &[(u64, u8)],
        anchor_orig: u8,
        delta: i32,
        fine: bool,
        cx: &mut Context<Self>,
    ) {
        if prev.is_empty() {
            return;
        }
        let anchor = relative_velocity(anchor_orig, delta);
        self.drag_value_status = Some(format!(
            "Anchor {anchor} · Δ{delta:+}{} · {} note{}",
            if fine { " fine" } else { "" },
            prev.len(),
            plural(prev.len())
        ));
        self.with_timeline_silent(cx, |tl, _| {
            for (id, orig) in prev {
                tl.state
                    .set_midi_note_velocity(clip_id, *id, relative_velocity(*orig, delta));
            }
        });
    }

    fn velocity_note_at_x(&self, cx: &Context<Self>, clip_id: &str, lx: f32) -> Option<(u64, u8)> {
        let tl = self.timeline.read(cx);
        let notes = tl.state.midi_clip_notes(clip_id)?;
        notes
            .iter()
            .filter(|note| self.channel_visible(note.channel))
            .filter_map(|note| {
                let d = self.display_note(note);
                let x = self.clip_beat_to_x(d.start);
                let distance = if lx < x {
                    x - lx
                } else if lx > x + 8.0 {
                    lx - (x + 8.0)
                } else {
                    0.0
                };
                (distance <= 14.0).then_some((distance, d.id, d.velocity))
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, id, velocity)| (id, velocity))
    }

    fn begin_velocity_lane_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.open_velocity_menu = None;
        window.focus(&self.focus, cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let Some((lx, ly)) = self.cc_local(event.position) else {
            return;
        };
        let selection_modifier =
            event.modifiers.shift || event.modifiers.control || event.modifiers.platform;
        if self.tool == PianoTool::Line {
            self.begin_velocity_line(clip_id, lx, ly, event.modifiers.shift, cx);
        } else if self.tool == PianoTool::Select || selection_modifier {
            let mode = MarqueeSelectionMode::from_modifiers(&event.modifiers);
            self.selection_before_marquee = self.selection.clone();
            self.drag = PianoDrag::VelocitySelect {
                clip_id,
                start_x: lx,
                start_y: ly,
                current_x: lx,
                current_y: ly,
                mode,
                dragging: false,
            };
            cx.notify();
        } else {
            self.begin_velocity_paint(clip_id, lx, ly, cx);
        }
    }

    fn velocity_from_local_y(&self, local_y: f32) -> u8 {
        let (_, lane_h) = self.cc_view_size();
        let usable_h = (lane_h - 8.0).max(1.0);
        let norm = (1.0 - ((local_y - 2.0) / usable_h)).clamp(0.0, 1.0);
        clamp_velocity((1.0 + norm * 126.0).round() as i32)
    }

    fn velocity_gesture_notes(&self, cx: &Context<Self>, clip_id: &str) -> Vec<MidiNoteState> {
        let mut notes: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|note| self.channel_visible(note.channel))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        notes.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        notes
    }

    fn begin_velocity_paint(&mut self, clip_id: String, lx: f32, ly: f32, cx: &mut Context<Self>) {
        let original_notes = self.velocity_gesture_notes(cx, &clip_id);
        self.drag = PianoDrag::VelocityPaint {
            clip_id,
            original_notes,
            touched: HashSet::new(),
            last_x: lx,
            last_y: ly,
        };
        self.paint_velocity_segment(lx, ly, lx, ly, cx);
        cx.notify();
    }

    fn paint_velocity_segment(
        &mut self,
        from_x: f32,
        from_y: f32,
        to_x: f32,
        to_y: f32,
        cx: &mut Context<Self>,
    ) {
        let lo_beat = self.x_to_clip_beat(from_x.min(to_x) - 4.0);
        let hi_beat = self.x_to_clip_beat(from_x.max(to_x) + 4.0);
        let (clip_id, updates): (String, Vec<(u64, u8)>) = match &self.drag {
            PianoDrag::VelocityPaint {
                clip_id,
                original_notes,
                ..
            } => {
                let start = original_notes.partition_point(|note| note.start < lo_beat);
                let end = original_notes.partition_point(|note| note.start <= hi_beat);
                let dx = to_x - from_x;
                let updates = original_notes[start..end]
                    .iter()
                    .map(|note| {
                        let note_x = self.clip_beat_to_x(note.start);
                        let t = if dx.abs() <= 1.0e-6 {
                            1.0
                        } else {
                            ((note_x - from_x) / dx).clamp(0.0, 1.0)
                        };
                        let y = from_y + (to_y - from_y) * t;
                        (note.id, self.velocity_from_local_y(y))
                    })
                    .collect();
                (clip_id.clone(), updates)
            }
            _ => return,
        };
        if updates.is_empty() {
            return;
        }
        if let PianoDrag::VelocityPaint {
            touched,
            last_x,
            last_y,
            ..
        } = &mut self.drag
        {
            touched.extend(updates.iter().map(|(id, _)| *id));
            *last_x = to_x;
            *last_y = to_y;
        }
        let value = updates.last().map(|(_, value)| *value).unwrap_or(1);
        self.drag_value_status = Some(format!(
            "Paint {value} · {} note{}",
            updates.len(),
            plural(updates.len())
        ));
        self.with_timeline_silent(cx, |tl, _| {
            for (id, value) in updates {
                tl.state.set_midi_note_velocity(&clip_id, id, value);
            }
        });
    }

    fn begin_velocity_line(
        &mut self,
        clip_id: String,
        lx: f32,
        ly: f32,
        unsnap: bool,
        cx: &mut Context<Self>,
    ) {
        let anchor_beat = self.snap_beats_live(self.x_to_clip_beat(lx), unsnap);
        let anchor_value = self.velocity_from_local_y(ly);
        let original_notes = self.velocity_gesture_notes(cx, &clip_id);
        self.drag = PianoDrag::VelocityLine {
            clip_id,
            original_notes,
            affected: HashSet::new(),
            anchor_beat,
            anchor_value,
            current_beat: anchor_beat,
            current_value: anchor_value,
            curve: VelocityCurve::Linear,
            unsnap,
        };
        self.update_velocity_line(lx, ly, cx);
        cx.notify();
    }

    fn update_velocity_line(&mut self, lx: f32, ly: f32, cx: &mut Context<Self>) {
        let unsnap = match &self.drag {
            PianoDrag::VelocityLine { unsnap, .. } => *unsnap,
            _ => false,
        };
        let cursor_beat = self.snap_beats_live(self.x_to_clip_beat(lx), unsnap);
        let cursor_value = self.velocity_from_local_y(ly);
        let selection = self.selection.clone();
        let (clip_id, updates, affected): (String, Vec<(u64, u8)>, HashSet<u64>) = match &self.drag
        {
            PianoDrag::VelocityLine {
                clip_id,
                original_notes,
                affected,
                anchor_beat,
                anchor_value,
                curve,
                ..
            } => {
                let (lo, hi, from, to) = if *anchor_beat <= cursor_beat {
                    (*anchor_beat, cursor_beat, *anchor_value, cursor_value)
                } else {
                    (cursor_beat, *anchor_beat, cursor_value, *anchor_value)
                };
                let span = (hi - lo).max(1.0e-6);
                let mut next_affected = affected.clone();
                let mut values = Vec::new();
                for note in original_notes {
                    let in_scope = note.start >= lo - 1.0e-4
                        && note.start <= hi + 1.0e-4
                        && (selection.is_empty() || selection.contains(&note.id));
                    if in_scope {
                        let t = (note.start - lo) / span;
                        values.push((note.id, interpolate_velocity(from, to, t, *curve)));
                        next_affected.insert(note.id);
                    } else if affected.contains(&note.id) {
                        values.push((note.id, note.velocity));
                    }
                }
                (clip_id.clone(), values, next_affected)
            }
            _ => return,
        };
        if let PianoDrag::VelocityLine {
            current_beat,
            current_value,
            affected: target_affected,
            ..
        } = &mut self.drag
        {
            *current_beat = cursor_beat;
            *current_value = cursor_value;
            *target_affected = affected;
        }
        self.drag_value_status = Some(format!(
            "Linear {}→{} · {:.2}..{:.2}",
            match &self.drag {
                PianoDrag::VelocityLine { anchor_value, .. } => *anchor_value,
                _ => cursor_value,
            },
            cursor_value,
            match &self.drag {
                PianoDrag::VelocityLine { anchor_beat, .. } => *anchor_beat,
                _ => cursor_beat,
            },
            cursor_beat
        ));
        self.with_timeline_silent(cx, |tl, _| {
            for (id, value) in updates {
                tl.state.set_midi_note_velocity(&clip_id, id, value);
            }
        });
    }

    fn update_velocity_select(&mut self, lx: f32, ly: f32, cx: &mut Context<Self>) {
        let (clip_id, start_x, start_y, mode, was_dragging) = match &self.drag {
            PianoDrag::VelocitySelect {
                clip_id,
                start_x,
                start_y,
                mode,
                dragging,
                ..
            } => (clip_id.clone(), *start_x, *start_y, *mode, *dragging),
            _ => return,
        };
        let dx = lx - start_x;
        let dy = ly - start_y;
        let dragging = was_dragging || (dx * dx + dy * dy).sqrt() >= MARQUEE_DRAG_THRESHOLD;
        if let PianoDrag::VelocitySelect {
            current_x,
            current_y,
            dragging: state_dragging,
            ..
        } = &mut self.drag
        {
            *current_x = lx;
            *current_y = ly;
            *state_dragging = dragging;
        }
        if !dragging {
            return;
        }
        let (view_w, lane_h) = self.cc_view_size();
        let rect = Self::normalized_marquee_rect(start_x, start_y, lx, ly, view_w, lane_h);
        let hits: HashSet<u64> = self
            .velocity_gesture_notes(cx, &clip_id)
            .into_iter()
            .filter(|note| {
                let x = self.clip_beat_to_x(note.start);
                let bar_h = (((note.velocity as f32 - 1.0) / 126.0) * (lane_h - 8.0)).max(1.0);
                Self::rects_intersect(rect, (x, lane_h - bar_h - 2.0, x + 8.0, lane_h - 2.0))
            })
            .map(|note| note.id)
            .collect();
        self.selection = Self::apply_marquee_mode(&self.selection_before_marquee, &hits, mode);
        cx.notify();
    }

    fn open_velocity_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let Some((lx, ly)) = self.cc_local(event.position) else {
            return;
        };
        if let Some((id, _)) = self.velocity_note_at_x(cx, &clip_id, lx) {
            if !self.selection.contains(&id) {
                self.selection = HashSet::from([id]);
            }
        }
        if self.selection.is_empty() {
            self.open_velocity_menu = None;
            return;
        }
        if self.open_native_velocity_menu(window, cx) {
            cx.notify();
            return;
        }
        self.open_velocity_menu = Some((lx, ly));
        cx.notify();
    }

    fn apply_velocity_operation(&mut self, operation: VelocityOperation, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let mut notes: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|note| self.selection.contains(&note.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if notes.is_empty() {
            return;
        }
        notes.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mean = notes.iter().map(|note| note.velocity as f32).sum::<f32>() / notes.len() as f32;
        let min_velocity = notes.iter().map(|note| note.velocity).min().unwrap_or(1);
        let max_velocity = notes.iter().map(|note| note.velocity).max().unwrap_or(127);
        let reversed: Vec<u8> = notes.iter().rev().map(|note| note.velocity).collect();
        let count = notes.len();
        let updates: Vec<(u64, u8)> = notes
            .iter()
            .enumerate()
            .map(|(index, note)| {
                let hash = note.id.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17);
                let t = if count <= 1 {
                    0.0
                } else {
                    index as f32 / (count - 1) as f32
                };
                let value = match operation {
                    VelocityOperation::SetDefault | VelocityOperation::ResetDefault => {
                        DEFAULT_NOTE_VELOCITY
                    }
                    VelocityOperation::Add => relative_velocity(note.velocity, 5),
                    VelocityOperation::Subtract => relative_velocity(note.velocity, -5),
                    VelocityOperation::Scale => {
                        clamp_velocity((note.velocity as f32 * 1.1).round() as i32)
                    }
                    VelocityOperation::Compress => {
                        clamp_velocity((mean + (note.velocity as f32 - mean) * 0.75).round() as i32)
                    }
                    VelocityOperation::Expand => {
                        clamp_velocity((mean + (note.velocity as f32 - mean) * 1.25).round() as i32)
                    }
                    VelocityOperation::Randomize => 1 + (hash % 127) as u8,
                    VelocityOperation::Humanize => {
                        relative_velocity(note.velocity, (hash % 11) as i32 - 5)
                    }
                    VelocityOperation::Crescendo => {
                        interpolate_velocity(min_velocity, max_velocity, t, VelocityCurve::Linear)
                    }
                    VelocityOperation::Decrescendo => {
                        interpolate_velocity(max_velocity, min_velocity, t, VelocityCurve::Linear)
                    }
                    VelocityOperation::Reverse => reversed[index],
                };
                (note.id, value)
            })
            .collect();
        let ids: Vec<u64> = updates.iter().map(|(id, _)| *id).collect();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            for (id, velocity) in updates {
                state.set_midi_note_velocity(cid, id, velocity);
            }
        });
        self.open_velocity_menu = None;
        cx.notify();
    }

    fn snapshot_selection(&self, cx: &Context<Self>, clip_id: &str) -> Vec<(u64, f32, u8)> {
        let tl = self.timeline.read(cx);
        tl.state
            .midi_clip_notes(clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| self.selection.contains(&n.id))
                    .map(|n| (n.id, n.start, n.pitch))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn on_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Piano-key lane drag-scrub: audition whichever key is under the cursor.
        // Driven from this root-level move handler (not a per-key one) so fast
        // vertical drags keep tracking even as the cursor crosses key edges. It
        // is mutually exclusive with grid drags — it only starts from a key
        // mouse-down — so handle it first and return.
        if self.piano_key_drag_active {
            match self.key_lane_pitch_at(event.position) {
                Some(raw_pitch) => {
                    let pitch = self.pitch_ctx.constrain_pitch(raw_pitch);
                    // Debounce: only (re)trigger when the pitch actually changes.
                    if self.key_lane_pressed_pitch != Some(pitch) {
                        if midi_debug_enabled() {
                            eprintln!(
                                "[PianoKeyPreview] move old={:?} new={}",
                                self.key_lane_pressed_pitch, pitch
                            );
                        }
                        // `begin_preview_note` sends note-off for the previous
                        // pitch before note-on for the new one (no stuck notes).
                        self.begin_preview_note(pitch, 100, "piano_key_drag", cx);
                        self.key_lane_pressed_pitch = Some(pitch);
                        cx.notify();
                    }
                }
                None => {
                    // Cursor left the lane: stop the current note but keep the
                    // drag active so returning to the lane resumes auditioning.
                    if self.key_lane_pressed_pitch.take().is_some() {
                        if midi_debug_enabled() {
                            eprintln!("[PianoKeyPreview] off note=outside_lane");
                        }
                        self.end_preview_note("piano_key_drag_out", cx);
                        cx.notify();
                    }
                }
            }
            return;
        }
        if matches!(self.drag, PianoDrag::RulerSeek) {
            if let Some((lx, _)) = self.ruler_local(event.position) {
                self.seek_ruler_at(lx, cx);
            }
            return;
        }
        // Track the grid beat under the pointer so paste-at-mouse has an anchor.
        // Cheap field write, no repaint.
        if let Some((lx, ly)) = self.grid_local(event.position) {
            self.hover_beat = Some(self.x_to_clip_beat(lx));
            self.hover_pitch = Some(self.y_to_pitch(ly));
        }
        match self.drag {
            PianoDrag::VelocityPaint { last_x, last_y, .. } => {
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.paint_velocity_segment(last_x, last_y, lx, ly, cx);
                }
                return;
            }
            PianoDrag::VelocityLine { .. } => {
                if let PianoDrag::VelocityLine { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.shift;
                }
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.update_velocity_line(lx, ly, cx);
                }
                return;
            }
            PianoDrag::VelocitySelect { .. } => {
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.update_velocity_select(lx, ly, cx);
                }
                return;
            }
            // The CC lane follows its drags from a window-level listener
            // (`PianoRoll::on_cc_pointer_move`) so a stroke keeps tracking
            // past the editor's edge; handling them here too would apply
            // every move twice.
            PianoDrag::CcSelect { .. }
            | PianoDrag::CcPaint { .. }
            | PianoDrag::CcMove { .. }
            | PianoDrag::CcLine { .. } => return,
            PianoDrag::ArtMove { id } => {
                if let Some((lx, _ly)) = self.cc_local(event.position) {
                    self.articulation_move_to(id, lx, cx);
                }
                return;
            }
            _ => {}
        }
        if event.pressed_button == Some(MouseButton::Right) {
            let Some((lx, ly)) = self.grid_local(event.position) else {
                return;
            };
            let Some(clip_id) = self.editing_clip_id(cx) else {
                return;
            };
            if let PianoDrag::EraseNotes {
                current_x,
                current_y,
                ..
            } = &mut self.drag
            {
                *current_x = lx;
                *current_y = ly;
            }
            let (start_x, start_y, cur_x, cur_y) = match &self.drag {
                PianoDrag::EraseNotes {
                    start_x,
                    start_y,
                    current_x,
                    current_y,
                    ..
                } => (*start_x, *start_y, *current_x, *current_y),
                _ => return,
            };
            let (view_w, view_h) = self.grid_view_size();
            let rect =
                Self::normalized_marquee_rect(start_x, start_y, cur_x, cur_y, view_w, view_h);
            let hits = self.collect_notes_in_rect(cx, &clip_id, rect);
            if let PianoDrag::EraseNotes { erased, .. } = &mut self.drag {
                for id in hits {
                    erased.insert(id);
                }
                self.erase_preview_ids = erased.clone();
                cx.notify();
            }
            return;
        }
        // Middle-mouse grab-pan: update scroll from pointer delta. Handled
        // before the Left-only early return so note editing never sees Middle.
        if matches!(self.drag, PianoDrag::Pan { .. }) {
            if event.pressed_button != Some(MouseButton::Middle) {
                self.drag = PianoDrag::None;
                cx.notify();
                return;
            }
            let cur_x: f32 = event.position.x.into();
            let cur_y: f32 = event.position.y.into();
            let max_scroll_x = self.max_scroll_x(cx);
            let max_scroll_y = self.max_scroll_y();
            if let PianoDrag::Pan { last_x, last_y } = &mut self.drag {
                let dx = cur_x - *last_x;
                let dy = cur_y - *last_y;
                *last_x = cur_x;
                *last_y = cur_y;
                let before_x = self.scroll_x;
                self.scroll_x = (self.scroll_x - dx).clamp(0.0, max_scroll_x);
                self.scroll_y = (self.scroll_y - dy).clamp(0.0, max_scroll_y);
                self.note_user_scrolled_x(before_x, cx);
            }
            cx.notify();
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if matches!(self.drag, PianoDrag::DrawNote { .. }) {
            if let PianoDrag::DrawNote { unsnap, .. } = &mut self.drag {
                *unsnap = event.modifiers.shift;
            }
            // A drum hit is one step long wherever the pointer goes.
            if self.drum_mode {
                return;
            }
            if let Some((lx, _)) = self.grid_local(event.position) {
                let live_unsnap = matches!(self.drag, PianoDrag::DrawNote { unsnap: true, .. });
                let beat = self.snap_beats_live(self.x_to_clip_beat(lx), live_unsnap);
                if let PianoDrag::DrawNote { end_beat, .. } = &mut self.drag {
                    *end_beat = beat;
                    cx.notify();
                }
            }
            return;
        }
        if let PianoDrag::MarqueeSelect { .. } = &self.drag {
            let Some((lx, ly)) = self.grid_local(event.position) else {
                return;
            };
            let Some(clip_id) = self.editing_clip_id(cx) else {
                return;
            };
            self.update_marquee_select(lx, ly, &clip_id, cx);
            return;
        }
        if matches!(self.drag, PianoDrag::Move { .. }) {
            if let PianoDrag::Move {
                pointer,
                travel,
                unsnap,
                ..
            } = &mut self.drag
            {
                *pointer = (event.position.x.into(), event.position.y.into());
                travel.track(*pointer);
                *unsnap = event.modifiers.shift;
            }
            self.refresh_drag_from_pointer();
            let audition_pitch = match &self.drag {
                PianoDrag::Move {
                    grab_pitch, dpitch, ..
                } => Some((*grab_pitch, *dpitch)),
                _ => None,
            };
            // Switch the live audition note when the dragged pitch changes; a
            // horizontal-only (timing) move never retriggers.
            let audition_pitch =
                audition_pitch.map(|(grab, dpitch)| self.moved_pitch(grab, dpitch));
            if let Some(pitch) = audition_pitch {
                let changed = self
                    .active_preview_note
                    .as_ref()
                    .map(|(_, _, p)| *p != pitch)
                    .unwrap_or(true);
                if changed {
                    self.begin_preview_note(pitch, 100, "note_move_pitch", cx);
                }
            }
            cx.notify();
            return;
        }
        if matches!(self.drag, PianoDrag::Resize { .. }) {
            if let PianoDrag::Resize {
                pointer,
                travel,
                unsnap,
                ..
            } = &mut self.drag
            {
                *pointer = (event.position.x.into(), event.position.y.into());
                travel.track(*pointer);
                *unsnap = event.modifiers.shift;
            }
            self.refresh_drag_from_pointer();
            cx.notify();
            return;
        }
        let ppb = self.ppb.max(0.0001);
        let snap_enabled = self.snap_on && !self.grid_res.is_free();
        let snap_step = self.step_beats();
        match &mut self.drag {
            PianoDrag::None => {}
            PianoDrag::Move { .. } | PianoDrag::Resize { .. } => {}
            PianoDrag::Stretch {
                start_x,
                spec,
                unsnap,
                ..
            } => {
                *unsnap = event.modifiers.shift;
                let cur_x: f32 = event.position.x.into();
                let step = (snap_enabled && !*unsnap).then_some(snap_step);
                spec.factor = stretch::drag_factor(spec, cur_x - *start_x, ppb, step);
                cx.notify();
            }
            PianoDrag::Velocity {
                clip_id,
                prev,
                anchor_orig,
                start_mouse_y,
                absolute,
                fine,
            } => {
                let clip_id = clip_id.clone();
                let prev = prev.clone();
                let anchor_orig = *anchor_orig;
                let start_mouse_y = *start_mouse_y;
                *absolute = event.modifiers.alt;
                *fine = event.modifiers.shift;
                let absolute = *absolute;
                let fine = *fine;
                let current_y: f32 = event.position.y.into();
                if absolute {
                    let value = self.velocity_from_window_y(event.position);
                    self.apply_velocity_absolute(&clip_id, &prev, value, cx);
                } else {
                    let (_, lane_h) = self.cc_view_size();
                    let delta = velocity_drag_delta(start_mouse_y, current_y, lane_h, fine);
                    self.apply_velocity_relative(&clip_id, &prev, anchor_orig, delta, fine, cx);
                }
            }
            PianoDrag::MarqueeSelect { .. } | PianoDrag::VelocitySelect { .. } => {}
            PianoDrag::VelocityPaint { .. } | PianoDrag::VelocityLine { .. } => {}
            PianoDrag::DrawNote { .. } | PianoDrag::EraseNotes { .. } => {}
            PianoDrag::CcPaint { .. }
            | PianoDrag::CcMove { .. }
            | PianoDrag::CcLine { .. }
            | PianoDrag::CcSelect { .. }
            | PianoDrag::ArtMove { .. } => {}
            PianoDrag::RulerSeek | PianoDrag::Pan { .. } => {}
        }
    }

    fn commit_draw_note(&mut self, drag: PianoDrag, cx: &mut Context<Self>) {
        let PianoDrag::DrawNote {
            pitch,
            start_beat,
            end_beat,
            unsnap,
            channel,
        } = drag
        else {
            return;
        };
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let (lo, hi) = normalize_range(start_beat, end_beat);
        let duration = self.drawn_note_length(lo, hi, unsnap);
        // Do not clamp the note into the current clip length — a note drawn past
        // the clip end auto-expands the clip (see `CreateMidiNote::execute`).
        // `MidiNoteState::new` clamps start >= 0, pitch 0..=127, dur >= MIN.
        let pitches = self.chord_draw_pitches(pitch);
        let notes: Vec<MidiNoteState> = pitches
            .into_iter()
            .map(|chord_pitch| {
                let mut chord_note =
                    MidiNoteState::new(chord_pitch, lo, duration, DEFAULT_NOTE_VELOCITY);
                chord_note.channel = channel;
                chord_note
            })
            .collect();
        let ids: HashSet<u64> = notes.iter().map(|n| n.id).collect();
        match notes.len() {
            0 => {}
            1 => {
                let note = notes.into_iter().next().expect("len 1");
                self.run_edit_command(EditCommand::CreateMidiNote { clip_id, note }, cx);
            }
            _ => {
                self.run_edit_command(EditCommand::CreateMidiNotes { clip_id, notes }, cx);
            }
        }
        self.selection = ids;
        cx.notify();
    }

    fn commit_erase_notes(&mut self, drag: PianoDrag, cx: &mut Context<Self>) {
        let PianoDrag::EraseNotes { erased, .. } = drag else {
            return;
        };
        self.erase_preview_ids.clear();
        if erased.is_empty() {
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let notes: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| erased.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if notes.is_empty() {
            return;
        }
        self.cleanup_midi_before_destructive_edit("note_erase_sweep", cx);
        self.run_edit_command(EditCommand::DeleteMidiNotes { clip_id, notes }, cx);
        self.selection.retain(|id| !erased.contains(id));
        cx.notify();
    }

    fn on_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.end_preview_note("mouse_up", cx);
        // End a piano-key scrub. Fires here for release anywhere — including
        // outside the lane/window — because this is wired to both `on_mouse_up`
        // and `on_mouse_up_out` on the root. A key-lane drag never edits clip
        // notes, so just clear its state (the note-off was sent above).
        if self.piano_key_drag_active || self.key_lane_pressed_pitch.is_some() {
            if midi_debug_enabled() {
                eprintln!(
                    "[PianoKeyPreview] off note={:?} reason=mouse_up",
                    self.key_lane_pressed_pitch
                );
            }
            self.piano_key_drag_active = false;
            self.key_lane_pressed_pitch = None;
            cx.notify();
            return;
        }
        if self.end_cc_drag(cx) {
            return;
        }
        if matches!(self.drag, PianoDrag::ArtMove { .. }) {
            self.drag = PianoDrag::None;
            self.drag_value_status = None;
            self.commit_articulation_edit(cx);
            return;
        }
        if matches!(self.drag, PianoDrag::RulerSeek) {
            self.drag = PianoDrag::None;
            self.drag_value_status = None;
            cx.notify();
            return;
        }
        if matches!(self.drag, PianoDrag::MarqueeSelect { .. }) {
            self.commit_marquee_select(cx);
            return;
        }
        if matches!(self.drag, PianoDrag::VelocitySelect { .. }) {
            let drag = std::mem::replace(&mut self.drag, PianoDrag::None);
            if let PianoDrag::VelocitySelect { mode, dragging, .. } = drag {
                if !dragging && mode == MarqueeSelectionMode::Replace {
                    self.selection.clear();
                }
            }
            self.selection_before_marquee.clear();
            cx.notify();
            return;
        }
        if matches!(self.drag, PianoDrag::Pan { .. }) {
            self.drag = PianoDrag::None;
            cx.notify();
            return;
        }
        // A scroll since the last move or paint still moves a Move/Resize
        // drag with the cursor: commit where the cursor points now.
        self.refresh_drag_from_pointer();
        let drag = std::mem::replace(&mut self.drag, PianoDrag::None);
        self.drag_value_status = None;
        if matches!(drag, PianoDrag::DrawNote { .. }) {
            self.commit_draw_note(drag, cx);
            return;
        }
        if matches!(drag, PianoDrag::EraseNotes { .. }) {
            self.commit_erase_notes(drag, cx);
            return;
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        match drag {
            PianoDrag::None => return,
            PianoDrag::Move {
                prev,
                travel,
                dx_beats,
                dpitch,
                anchor_start,
                clone_on_commit,
                unsnap,
                deselect_on_click,
                ..
            } => {
                // A press whose pointer moved less than a few pixels is a
                // click, however far follow-playhead or the wheel scrolled
                // the view meanwhile (#242): it commits nothing and records
                // no undo step. With Shift held the drag is unsnapped, and a
                // hand's jitter would otherwise nudge the notes off the grid.
                if move_release_is_click(travel) {
                    if let Some(id) = deselect_on_click {
                        self.selection.remove(&id);
                        cx.notify();
                    }
                    return;
                }
                let pitch_ctx = self.pitch_ctx;
                let snapped_anchor = self.snap_beats_live(anchor_start + dx_beats, unsnap);
                let effective_delta = snapped_anchor - anchor_start;
                let updates: Vec<(u64, f32, u8)> = prev
                    .iter()
                    .map(|(id, start, pitch)| {
                        let new_start = (*start + effective_delta).max(0.0);
                        let _ = pitch_ctx;
                        (*id, new_start, self.moved_pitch(*pitch, dpitch))
                    })
                    .collect();
                // Skip a commit (and the dirty flag) if snapping landed every
                // note back on its original position.
                let changed = updates
                    .iter()
                    .zip(prev.iter())
                    .any(|((_, ns, np), (_, os, op))| (ns - os).abs() > 1e-4 || np != op);
                if !changed {
                    return;
                }
                if clone_on_commit {
                    let source_notes: std::collections::HashMap<u64, MidiNoteState> = self
                        .timeline
                        .read(cx)
                        .state
                        .midi_clip_notes(&clip_id)
                        .map(|notes| notes.iter().map(|n| (n.id, n.clone())).collect())
                        .unwrap_or_default();
                    let notes: Vec<MidiNoteState> = updates
                        .iter()
                        .filter_map(|(id, start, pitch)| {
                            let source = source_notes.get(id)?;
                            let mut note = MidiNoteState::new(
                                *pitch,
                                *start,
                                source.duration,
                                source.velocity,
                            );
                            note.muted = source.muted;
                            note.channel = source.channel;
                            note.articulation = source.articulation;
                            note.pitch_curve = source
                                .pitch_curve
                                .as_ref()
                                .map(PitchCurve::cloned_with_new_ids);
                            note.expression = source.expression.clone();
                            Some(note)
                        })
                        .collect();
                    if notes.is_empty() {
                        return;
                    }
                    let new_ids: Vec<u64> = notes.iter().map(|note| note.id).collect();
                    self.run_edit_command(EditCommand::CreateMidiNotes { clip_id, notes }, cx);
                    self.selection = new_ids.into_iter().collect();
                    cx.notify();
                    return;
                }
                let ids: Vec<u64> = updates.iter().map(|(id, _, _)| *id).collect();
                self.commit_note_transform(cx, &ids, move |state, cid| {
                    state.move_midi_notes(cid, &updates)
                });
            }
            PianoDrag::Resize {
                id,
                ids,
                travel,
                new_dur,
                prev_dur,
                prev_durs,
                delta_dur,
                ..
            } => {
                // As for Move: only a real pointer drag resizes (#242).
                if !travel.dragging || (new_dur - prev_dur).abs() < 0.0001 {
                    return;
                }
                let target_ids = if ids.is_empty() { vec![id] } else { ids };
                self.commit_note_transform(cx, &target_ids, move |state, cid| {
                    for (note_id, duration) in prev_durs {
                        let next = (duration + delta_dur).max(MIN_NOTE_BEATS);
                        state.resize_midi_note(cid, note_id, next);
                    }
                });
            }
            PianoDrag::Stretch { ids, spec, .. } => {
                self.commit_stretch(ids, spec, cx);
            }
            PianoDrag::Velocity {
                clip_id: velocity_clip_id,
                prev: orig,
                ..
            } => {
                // Velocity was applied live (silent). Reconstruct the pre-drag
                // state from the per-note original velocities and record one
                // undoable edit covering every affected note.
                let ids: Vec<u64> = orig.iter().map(|(id, _)| *id).collect();
                let next = self.snapshot_notes(cx, &velocity_clip_id, &ids);
                let original: std::collections::HashMap<u64, _> = orig.iter().copied().collect();
                let prev: Vec<MidiNoteState> = next
                    .iter()
                    .map(|n| {
                        let mut p = n.clone();
                        if let Some(v) = original.get(&n.id) {
                            p.velocity = *v;
                        }
                        p
                    })
                    .collect();
                self.push_note_edit(cx, velocity_clip_id, prev, next);
            }
            PianoDrag::VelocityPaint {
                clip_id: velocity_clip_id,
                original_notes,
                touched,
                ..
            } => {
                let ids: Vec<u64> = touched.into_iter().collect();
                let id_set: std::collections::HashSet<u64> = ids.iter().copied().collect();
                let prev: Vec<MidiNoteState> = original_notes
                    .into_iter()
                    .filter(|note| id_set.contains(&note.id))
                    .collect();
                let next = self.snapshot_notes(cx, &velocity_clip_id, &ids);
                self.push_note_edit(cx, velocity_clip_id, prev, next);
            }
            PianoDrag::VelocityLine {
                clip_id: velocity_clip_id,
                original_notes,
                affected,
                ..
            } => {
                let ids: Vec<u64> = affected.into_iter().collect();
                let id_set: std::collections::HashSet<u64> = ids.iter().copied().collect();
                let prev: Vec<MidiNoteState> = original_notes
                    .into_iter()
                    .filter(|note| id_set.contains(&note.id))
                    .collect();
                let next = self.snapshot_notes(cx, &velocity_clip_id, &ids);
                self.push_note_edit(cx, velocity_clip_id, prev, next);
            }
            PianoDrag::MarqueeSelect { .. } | PianoDrag::VelocitySelect { .. } => {}
            PianoDrag::DrawNote { .. } | PianoDrag::EraseNotes { .. } => {}
            PianoDrag::CcPaint { .. }
            | PianoDrag::CcMove { .. }
            | PianoDrag::CcLine { .. }
            | PianoDrag::CcSelect { .. }
            | PianoDrag::ArtMove { .. } => {}
            PianoDrag::RulerSeek | PianoDrag::Pan { .. } => {}
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Typing in the editor while a tool previews keeps the tool's result
        // first, so the key acts on the notes as they look.
        self.accept_active_tool(cx);
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let key = event.keystroke.key.as_str();
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let shift = event.keystroke.modifiers.shift;
        match key {
            "up" if !ctrl && !self.selection.is_empty() => {
                cx.stop_propagation();
                if self.drum_mode {
                    self.move_selection_lanes(-1, cx);
                } else {
                    self.transpose_selection(if shift { 12 } else { 1 }, cx);
                }
            }
            "down" if !ctrl && !self.selection.is_empty() => {
                cx.stop_propagation();
                if self.drum_mode {
                    self.move_selection_lanes(1, cx);
                } else {
                    self.transpose_selection(if shift { -12 } else { -1 }, cx);
                }
            }
            // Articulation lane focus: Delete removes the selected direction
            // event; notes keep their own Delete handling below.
            "delete" | "backspace"
                if self.lane_view == PianoLaneView::Controller && !self.cc_selection.is_empty() =>
            {
                cx.stop_propagation();
                self.delete_selected_cc_points(cx);
            }
            "delete" | "backspace"
                if self.lane_view == PianoLaneView::Articulations
                    && self.selected_articulation.is_some() =>
            {
                cx.stop_propagation();
                self.delete_selected_articulation(cx);
            }
            "delete" | "backspace" if !self.selection.is_empty() => {
                cx.stop_propagation();
                self.delete_selection(cx);
            }
            "a" if ctrl && self.lane_view == PianoLaneView::Controller => {
                cx.stop_propagation();
                self.cc_selection = self
                    .timeline
                    .read(cx)
                    .state
                    .controller_lane_points(&clip_id, self.active_cc)
                    .map(|points| points.iter().map(|point| point.id).collect())
                    .unwrap_or_default();
                cx.notify();
            }
            "a" if ctrl => {
                cx.stop_propagation();
                self.selection = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(&clip_id)
                    .map(|notes| visible_note_ids(notes, self.channel_view))
                    .unwrap_or_default();
                cx.notify();
            }
            "c" if ctrl => {
                cx.stop_propagation();
                self.copy_selection(cx);
            }
            "x" if ctrl => {
                // Cut = copy then delete the selection. `delete_selection` records
                // one undoable edit, so the cut is a single undo step.
                cx.stop_propagation();
                if !self.selection.is_empty() {
                    self.copy_selection(cx);
                    self.delete_selection(cx);
                }
            }
            "v" if ctrl && shift => {
                cx.stop_propagation();
                self.paste_clipboard_at_mouse(cx);
            }
            "v" if ctrl => {
                cx.stop_propagation();
                self.paste_clipboard(cx);
            }
            "d" if ctrl && self.lane_view == PianoLaneView::Controller => {
                cx.stop_propagation();
                self.duplicate_selected_cc_points(cx);
            }
            "d" if ctrl => {
                cx.stop_propagation();
                self.duplicate_selection(shift, cx);
            }
            "m" if !ctrl => {
                cx.stop_propagation();
                self.toggle_mute_selection(cx);
            }
            "escape" => {
                cx.stop_propagation();
                if self.open_note_menu.take().is_some() || self.open_velocity_menu.take().is_some()
                {
                    cx.notify();
                } else if !matches!(self.drag, PianoDrag::None) {
                    self.cancel_active_gesture(cx);
                } else {
                    if self.lane_view == PianoLaneView::Controller {
                        self.cc_selection.clear();
                    } else {
                        self.selection.clear();
                    }
                    cx.notify();
                }
            }
            "f" if !ctrl => {
                cx.stop_propagation();
                self.fit_piano_roll_to_notes(cx, &clip_id);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Quantize note starts with the MIDI Tools panel's grid, strength and
    /// swing. Empty selection means every shown note in the clip.
    fn quantize_selection(&mut self, cx: &mut Context<Self>) {
        self.tool_quantize(
            crate::components::timeline::timeline_state::QuantizeTarget::Start,
            cx,
        );
    }

    /// Pitches the Draw tool will create for `root`. Chord mode stacks diatonic
    /// thirds from the active scale; Lock still applies to a single-note draw.
    fn chord_draw_pitches(&self, root: u8) -> Vec<u8> {
        if self.drum_mode {
            return vec![root];
        }
        match self.chord_kind {
            ChordInsertKind::Off => vec![self.pitch_ctx.constrain_pitch(root)],
            kind => self.pitch_ctx.scale.chord_pitches(root, kind.voices()),
        }
    }

    /// Snap selected notes (or every note in the clip when nothing is selected)
    /// to the nearest pitch in the active scale. No-op for Chromatic.
    fn snap_selection_to_scale(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let mut ids: Vec<u64> = self.selection.iter().copied().collect();
        if ids.is_empty() {
            ids = self
                .timeline
                .read(cx)
                .state
                .midi_clip_notes(&clip_id)
                .map(|notes| notes.iter().map(|n| n.id).collect())
                .unwrap_or_default();
        }
        if ids.is_empty() {
            return;
        }
        let scale = self.pitch_ctx.scale;
        self.run_note_tool("Snap to Scale", cx, move |notes, ids| {
            let mut changed = 0;
            for note in notes.iter_mut().filter(|n| ids.contains(&n.id)) {
                let pitch = scale.nearest_pitch(note.pitch);
                if pitch != note.pitch {
                    note.pitch = pitch;
                    changed += 1;
                }
            }
            changed
        });
    }

    /// Set the MIDI channel on the selected notes. No-op when nothing is
    /// selected.
    fn set_selected_notes_channel(&mut self, channel: MidiChannel, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.selection.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let target_ids = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.set_midi_notes_channel(cid, &target_ids, channel);
        });
    }

    /// Shift the selected notes' MIDI channel by `delta` (clamped 1..=16).
    fn nudge_selected_channel(&mut self, delta: i32, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.selection.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let target_ids = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.nudge_midi_notes_channel(cid, &target_ids, delta);
        });
    }

    /// Toggle the editing track's output channel policy between `Fixed` (the
    /// pre-existing single-channel behavior) and `PerNote`. Panics the track's
    /// active notes first so nothing sticks on the channel it was playing on.
    fn toggle_track_output_per_note(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let Some(track_id) = self
            .timeline
            .read(cx)
            .state
            .find_clip(&clip_id)
            .map(|(track, _)| track.id.clone())
        else {
            return;
        };
        let next = !self
            .timeline
            .read(cx)
            .state
            .find_track(&track_id)
            .map(|t| t.routing.midi_output_per_note)
            .unwrap_or(false);
        self.cleanup_midi_before_destructive_edit("midi_output_mode_change", cx);
        self.timeline.update(cx, |tl, tcx| {
            let edit = tl.begin_track_edit(
                crate::components::timeline::timeline_state::TrackEditScope::tracks([
                    track_id.clone()
                ]),
            );
            tl.state.set_track_midi_output_per_note(&track_id, next);
            // It is saved with the track and changes what plays, so it is an
            // edit like any other: one undo step, and the project is changed.
            if tl.commit_track_edit("MIDI Output Per Note", edit, false, tcx) {
                tl.mark_project_changed(tcx);
            }
            tcx.notify();
        });
    }

    fn track_output_per_note(&self, cx: &Context<Self>) -> bool {
        self.editing_clip_id(cx)
            .and_then(|clip_id| {
                self.timeline
                    .read(cx)
                    .state
                    .find_clip(&clip_id)
                    .map(|(track, _)| track.routing.midi_output_per_note)
            })
            .unwrap_or(false)
    }

    /// Set the channel-view filter (All Channels or a single Channel 1–16).
    fn set_channel_view(&mut self, mask: MidiChannelMask, cx: &mut Context<Self>) {
        self.channel_view = mask;
        self.open_select_menu = None;
        cx.notify();
    }

    fn channel_view_label(&self) -> String {
        if self.channel_view.is_all() {
            "All Channels".to_string()
        } else if let Some(ch) =
            MidiChannel::all().find(|ch| self.channel_view == MidiChannelMask::single(*ch))
        {
            format!("Channel {}", ch.ui())
        } else {
            "Custom".to_string()
        }
    }

    /// Begin middle-mouse grab-pan of the note grid.
    fn begin_pan(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.drag, PianoDrag::None | PianoDrag::Pan { .. }) {
            self.cancel_active_gesture(cx);
        }
        window.focus(&self.focus, cx);
        window.prevent_default();
        cx.stop_propagation();
        self.drag = PianoDrag::Pan {
            last_x: event.position.x.into(),
            last_y: event.position.y.into(),
        };
        cx.notify();
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        if self.selection.is_empty() {
            return;
        }
        let ids: HashSet<u64> = self.selection.clone();
        let notes: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| ids.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if notes.is_empty() {
            return;
        }
        // Stop/panic any sounding preview before the note data disappears so a
        // held audition (e.g. delete pressed mid-move) cannot get stuck.
        self.cleanup_midi_before_destructive_edit("note_delete", cx);
        self.run_edit_command(EditCommand::DeleteMidiNotes { clip_id, notes }, cx);
        self.selection.clear();
    }

    /// Toggle mute on the selection. When the selection is mixed, the gesture
    /// mutes everything (the common DAW behaviour); when all are already muted
    /// it unmutes. Routed through an undoable command.
    fn toggle_mute_selection(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        if self.selection.is_empty() {
            return;
        }
        let prev: Vec<(u64, bool)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| self.selection.contains(&n.id))
                    .map(|n| (n.id, n.muted))
                    .collect()
            })
            .unwrap_or_default();
        if prev.is_empty() {
            return;
        }
        // Unmute only when every selected note is already muted.
        let muted = !prev.iter().all(|(_, m)| *m);
        self.run_edit_command(
            EditCommand::SetMidiNotesMuted {
                clip_id,
                prev,
                muted,
            },
            cx,
        );
    }

    /// Erase tool: delete a single note by id, undoable.
    fn erase_note(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let note = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|ns| ns.iter().find(|n| n.id == id).cloned());
        let Some(note) = note else {
            return;
        };
        self.cleanup_midi_before_destructive_edit("note_erase", cx);
        self.run_edit_command(
            EditCommand::DeleteMidiNotes {
                clip_id,
                notes: vec![note],
            },
            cx,
        );
        self.selection.remove(&id);
        cx.notify();
    }

    /// Mute tool: toggle a single note's muted state, undoable.
    fn mute_note(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let was = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|ns| ns.iter().find(|n| n.id == id).map(|n| n.muted));
        let Some(was) = was else {
            return;
        };
        self.run_edit_command(
            EditCommand::SetMidiNotesMuted {
                clip_id,
                prev: vec![(id, was)],
                muted: !was,
            },
            cx,
        );
        cx.notify();
    }

    /// Split tool: cut a note at `beat` (clip-local) into two contiguous notes.
    /// Snaps the cut when snap is on; refuses cuts that would leave a part
    /// shorter than [`MIN_NOTE_BEATS`]. Selects the two resulting parts.
    fn split_note(&mut self, id: u64, beat: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let original = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .and_then(|ns| ns.iter().find(|n| n.id == id).cloned());
        let Some(original) = original else {
            return;
        };
        let cut = if self.snap_on {
            self.snap_beats(beat)
        } else {
            beat
        };
        let left_len = cut - original.start;
        let right_len = (original.start + original.duration) - cut;
        if left_len < MIN_NOTE_BEATS || right_len < MIN_NOTE_BEATS {
            return;
        }
        // Per-note expression follows the parts: every expression curve is cut
        // at the same beat and each right-hand half is re-based to its own
        // note start, so a split never silently discards a performance.
        let (left_curve, right_curve) = original
            .pitch_curve
            .as_ref()
            .map(|curve| curve.split_at(left_len))
            .unwrap_or_default();
        let (left_expression, right_expression) = original.expression.split_at(left_len);
        let mut left =
            MidiNoteState::new(original.pitch, original.start, left_len, original.velocity);
        left.muted = original.muted;
        left.channel = original.channel;
        left.articulation = original.articulation;
        left.pitch_curve = (!left_curve.is_empty()).then_some(left_curve);
        left.release_velocity = None;
        left.expression = left_expression;
        let mut right = MidiNoteState::new(original.pitch, cut, right_len, original.velocity);
        right.muted = original.muted;
        right.channel = original.channel;
        right.articulation = original.articulation;
        right.pitch_curve = (!right_curve.is_empty()).then_some(right_curve);
        right.release_velocity = original.release_velocity;
        right.expression = right_expression;
        let new_ids = [left.id, right.id];
        self.run_edit_command(
            EditCommand::SplitMidiNote {
                clip_id,
                original,
                parts: vec![left, right],
            },
            cx,
        );
        self.selection = new_ids.into_iter().collect();
        cx.notify();
    }

    /// The start of the grid step `beat` falls in, for a drum hit: the cell it
    /// was clicked in, not the nearest line. Off the grid (Shift, snap off, or
    /// a free grid) the hit lands where it was clicked.
    fn drum_step_start(&self, beat: f32, unsnap: bool) -> f32 {
        drum_cell_start(self.snapping_grid(unsnap), beat)
    }

    /// Move the selected hits `rows_down` lanes (negative is up), as one
    /// undoable edit — the drum editor's Up/Down.
    fn move_selection_lanes(&mut self, rows_down: i32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let moves: Vec<(u64, i32)> = {
            let Some(notes) = self.timeline.read(cx).state.midi_clip_notes(&clip_id) else {
                return;
            };
            notes
                .iter()
                .filter(|note| self.selection.contains(&note.id))
                .map(|note| {
                    let target = drum_map::drum_move(&self.drum_rows, note.pitch, rows_down);
                    (note.id, target as i32 - note.pitch as i32)
                })
                .filter(|(_, delta)| *delta != 0)
                .collect()
        };
        if moves.is_empty() {
            return;
        }
        // Lanes are not evenly spaced in pitch, so each hit moves by its own
        // semitone delta, in one edit.
        let deltas: std::collections::HashMap<u64, i32> = moves.into_iter().collect();
        self.transpose_notes_by(&clip_id, &deltas, cx);
    }

    /// Transpose each of `deltas`' notes by its own semitone delta, as one
    /// undoable edit.
    fn transpose_notes_by(
        &mut self,
        _clip_id: &str,
        deltas: &std::collections::HashMap<u64, i32>,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<u64> = deltas.keys().copied().collect();
        let deltas = deltas.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            for (id, delta) in &deltas {
                state.transpose_midi_notes(cid, &[*id], *delta);
            }
        });
    }

    /// Transpose the selected notes by `delta` semitones (clamped to 0..=127),
    /// recorded as one undoable edit. No-op on empty selection.
    fn transpose_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        if delta == 0 || self.selection.is_empty() {
            return;
        }
        let ids: Vec<u64> = self.selection.iter().copied().collect();
        let target = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.transpose_midi_notes(cid, &target, delta);
        });
    }

    /// Copy the current selection into the process-global note clipboard,
    /// storing timing relative to the earliest selected note.
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let notes: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| self.selection.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if notes.is_empty() {
            return;
        }
        let min_start = notes.iter().map(|n| n.start).fold(f32::INFINITY, f32::min);
        let clip: Vec<ClipboardNote> = notes
            .iter()
            .map(|n| ClipboardNote {
                pitch: n.pitch,
                rel_start: (n.start - min_start).max(0.0),
                duration: n.duration,
                velocity: n.velocity,
                muted: n.muted,
                channel: n.channel,
                articulation: n.articulation,
                pitch_curve: n.pitch_curve.clone(),
                expression: n.expression.clone(),
            })
            .collect();
        MIDI_NOTE_CLIPBOARD.with(|cb| {
            *cb.borrow_mut() = ClipboardPayload {
                version: MIDI_CLIPBOARD_VERSION,
                notes: clip,
            };
        });
        if midi_debug_enabled() {
            eprintln!("[midi] copy notes={}", notes.len());
        }
    }

    /// Build notes from the clipboard anchored so the earliest note lands at
    /// `anchor_beat` (clip-local). Returns an empty vec when the clipboard is
    /// empty. New notes get fresh transient ids.
    fn clipboard_notes_at(&self, anchor_beat: f32) -> Vec<MidiNoteState> {
        MIDI_NOTE_CLIPBOARD.with(|cb| {
            let payload = cb.borrow();
            // Reject data this build doesn't understand rather than mis-reading it.
            if payload.version != MIDI_CLIPBOARD_VERSION {
                return Vec::new();
            }
            payload
                .notes
                .iter()
                .map(|c| {
                    let mut note = MidiNoteState::new(
                        c.pitch,
                        (anchor_beat + c.rel_start).max(0.0),
                        c.duration,
                        c.velocity,
                    );
                    note.muted = c.muted;
                    note.channel = c.channel;
                    note.articulation = c.articulation;
                    note.pitch_curve = c.pitch_curve.as_ref().map(PitchCurve::cloned_with_new_ids);
                    note.expression = c.expression.clone();
                    note
                })
                .collect()
        })
    }

    /// Recompute the playhead's position and repaint only the line.
    ///
    /// Called from the audio poll on every transport tick. Returns `true` when
    /// the overlay was actually notified, so the caller can tell a frame that
    /// drew something from one that did not.
    ///
    /// Takes `&mut App` rather than `&mut Context<Self>` because the point is
    /// to *not* notify this entity: notifying the roll rebuilds every note,
    /// grid line and lane in it, which is the whole cost this avoids.
    /// Takes the entity rather than `&self` because the two halves need the
    /// app differently: reading the roll to work out where the line goes
    /// borrows it, and notifying the overlay needs it mutably. Resolving
    /// everything in one scope lets the read end before the write begins.
    pub fn publish_playhead(roll: &Entity<Self>, cx: &mut gpui::App) -> bool {
        let (overlay, frame, next) = {
            let this = roll.read(cx);
            let Some(overlay) = this.playhead_overlay.clone() else {
                // No overlay yet means this roll has never rendered, so there
                // is no line on screen to move.
                return false;
            };
            (
                overlay,
                this.playhead_frame.clone(),
                this.playhead_frame_now(cx),
            )
        };

        // Sub-pixel motion draws the same line. At 144 Hz on a zoomed-out clip
        // most ticks land inside one pixel, and repainting for them is work
        // with nothing to show for it. Visibility and transport state still
        // pass through, because those change what is drawn rather than where.
        let current = frame.get();
        if current.visible == next.visible
            && current.playing == next.playing
            && (current.x - next.x).abs() < 0.5
        {
            return false;
        }
        frame.set(next);
        overlay.update(cx, |_, cx| cx.notify());
        true
    }

    /// Continuous scrolling: while the transport plays and Follow is on, keep
    /// the playhead at the middle of the editor and scroll the notes under
    /// it. Call it only for a roll that is on screen — it repaints the whole
    /// roll whenever the view moves. Returns whether the view moved.
    ///
    /// Follows the transport's own Follow switch, so one control turns
    /// following on and off in the arrangement and the editor alike. A user
    /// scroll pauses it until the transport stops, and the view stops at the
    /// end of the track rather than scrolling into nothing.
    pub fn follow_playhead(roll: &Entity<Self>, cx: &mut gpui::App) -> bool {
        let (playing, target) = {
            let this = roll.read(cx);
            let state = &this.timeline.read(cx).state;
            let playing = state.transport.playing;
            let follow = state.follow_playhead
                && !state.follow_playhead_suspended
                && state.auto_scroll_mode
                    != crate::components::timeline::timeline_state::AutoScrollMode::Off;
            let target = (playing
                && follow
                && !this.follow_paused
                && !state.selection.selected_clip_ids.is_empty())
            .then(|| {
                let (view_w, _) = this.grid_view_size();
                let end = this.scope.extent().map_or(0.0, |(_, end)| end);
                let max_scroll_x = ((end + 4.0) * this.ppb - view_w).max(0.0);
                (state.transport.playhead_beats * this.ppb - view_w * 0.5).clamp(0.0, max_scroll_x)
            })
            .filter(|target| view_ready(this) && (target - this.scroll_x).abs() >= 0.5);
            (playing, target)
        };
        fn view_ready(roll: &PianoRoll) -> bool {
            roll.grid_view_size().0 > 1.0
        }
        if !playing {
            if roll.read(cx).follow_paused {
                roll.update(cx, |this, _| this.follow_paused = false);
            }
            return false;
        }
        let Some(target) = target else {
            return false;
        };
        roll.update(cx, |this, cx| {
            this.scroll_x = target;
            cx.notify();
        });
        true
    }

    /// A sideways scroll by the user during playback pauses following.
    fn note_user_scrolled_x(&mut self, before: f32, cx: &Context<Self>) {
        if (self.scroll_x - before).abs() > 0.5 && self.timeline.read(cx).state.transport.playing {
            self.follow_paused = true;
        }
    }

    /// The playhead frame for the clip currently being edited.
    ///
    /// Clip-local, like everything else the roll draws: the transport is a
    /// project-wide beat, and outside this clip's span there is no line to
    /// draw because the song is somewhere the editor is not showing.
    pub(super) fn playhead_frame_now(&self, cx: &gpui::App) -> playhead::PianoRollPlayheadFrame {
        let tl = self.timeline.read(cx);
        let playing = tl.state.transport.playing;
        let playhead = tl.state.transport.playhead_beats;

        let Some(clip_id) = tl.state.selection.selected_clip_ids.first() else {
            return playhead::PianoRollPlayheadFrame {
                x: 0.0,
                visible: false,
                playing,
            };
        };
        let Some((_track, clip)) = tl.state.find_clip(clip_id) else {
            return playhead::PianoRollPlayheadFrame {
                x: 0.0,
                visible: false,
                playing,
            };
        };
        let _ = clip;
        playhead::PianoRollPlayheadFrame {
            // Project beats, like the axis. The line is drawn wherever the
            // transport is, including over the clips either side of the one
            // being edited — which is the point of showing them.
            x: self.project_beat_to_x(playhead),
            visible: true,
            playing,
        }
    }

    /// Clip-local paste anchor at the playhead, falling back to clip beat 0 when
    /// the playhead sits outside the clip.
    fn playhead_paste_anchor(&self, cx: &Context<Self>, clip_id: &str) -> f32 {
        let (clip_start, _clip_len) = self.clip_meta(cx, clip_id);
        let playhead = self.timeline.read(cx).state.transport.playhead_beats;
        self.snap_beats((playhead - clip_start).max(0.0))
    }

    /// Paste the clipboard at the playhead. The pasted notes become the new
    /// selection.
    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let anchor = self.playhead_paste_anchor(cx, &clip_id);
        self.paste_clipboard_anchored(clip_id, anchor, cx);
    }

    /// Paste the clipboard at the last grid beat the pointer was over, falling
    /// back to the playhead anchor when the pointer hasn't been over the grid.
    fn paste_clipboard_at_mouse(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let anchor = match self.hover_beat {
            Some(beat) => self.snap_beats(beat.max(0.0)),
            None => self.playhead_paste_anchor(cx, &clip_id),
        };
        self.paste_clipboard_anchored(clip_id, anchor, cx);
    }

    /// Shared paste implementation: build clipboard notes at `anchor`, insert as
    /// one undoable command, and select the new notes.
    fn paste_clipboard_anchored(&mut self, clip_id: String, anchor: f32, cx: &mut Context<Self>) {
        let notes = self.clipboard_notes_at(anchor);
        if notes.is_empty() {
            return;
        }
        let new_ids: Vec<u64> = notes.iter().map(|n| n.id).collect();
        self.run_edit_command(EditCommand::CreateMidiNotes { clip_id, notes }, cx);
        self.selection = new_ids.into_iter().collect();
        if midi_debug_enabled() {
            eprintln!(
                "[midi] paste anchor={:.3} count={}",
                anchor,
                self.selection.len()
            );
        }
        cx.notify();
    }

    /// Duplicate the selection in place, offset forward by the selection's beat
    /// span so the copies sit after the originals. With snapping on, the span
    /// is measured on the note grid — whole grid steps, whole bars of the
    /// meter for "1 Bar", following meter changes (see
    /// [`MeterGrid::duplicate_offset`] for the rule); Shift, `Free` or snap
    /// off keep the exact span. Selects the new notes. Does not use the
    /// clipboard.
    fn duplicate_selection(&mut self, bypass_snap: bool, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let src: Vec<MidiNoteState> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|n| self.selection.contains(&n.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if src.is_empty() {
            return;
        }
        let min_start = src.iter().map(|n| n.start).fold(f32::INFINITY, f32::min);
        let max_end = src
            .iter()
            .map(|n| n.start + n.duration)
            .fold(0.0_f32, f32::max);
        // Shift bypass keeps the exact source span; otherwise the copies stay
        // on the note grid that movement snaps to.
        let grid = if bypass_snap || !self.snap_on {
            None
        } else {
            self.edit_grid()
        };
        let offset = duplicate_offset(grid, min_start, max_end);
        let notes: Vec<MidiNoteState> = src
            .iter()
            .map(|n| {
                let mut note =
                    MidiNoteState::new(n.pitch, n.start + offset, n.duration, n.velocity);
                note.muted = n.muted;
                note.channel = n.channel;
                note.articulation = n.articulation;
                note.pitch_curve = n.pitch_curve.as_ref().map(PitchCurve::cloned_with_new_ids);
                note.expression = n.expression.clone();
                note
            })
            .collect();
        let new_ids: Vec<u64> = notes.iter().map(|n| n.id).collect();
        self.run_edit_command(EditCommand::CreateMidiNotes { clip_id, notes }, cx);
        self.selection = new_ids.into_iter().collect();
        if midi_debug_enabled() {
            eprintln!(
                "[midi] duplicate offset={:.3} count={}",
                offset,
                self.selection.len()
            );
        }
        cx.notify();
    }

    // ── Articulation ops (selection + lane) ───────────────────────────────

    /// Assign (or clear, with `None`) a per-note articulation on every
    /// selected note, as one undoable `EditMidiNotes` command. Never touches
    /// note timing/velocity — articulation is playback-only metadata.
    pub(super) fn set_selection_articulation(
        &mut self,
        articulation: Option<ArticulationId>,
        cx: &mut Context<Self>,
    ) {
        // Every entry point funnels through here, so the capability table is
        // enforced once. Without it the arrangement clip menu could assign an
        // articulation the loaded instrument cannot play, leaving a badge the
        // inspector then offered no button to clear. Clearing (`None`) is
        // always allowed — it is how such a value gets removed.
        if let Some(requested) = articulation {
            if !self.available_articulations(cx).contains(&requested) {
                return;
            }
        }
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
        let target = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.set_midi_notes_articulation(cid, &target, articulation);
        });
        cx.notify();
    }

    /// Delete the selected direction articulation event (lane Delete key /
    /// lane context action) as one undoable command.
    pub(super) fn delete_selected_articulation(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_articulation.take() else {
            return;
        };
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let prev = self
            .timeline
            .read(cx)
            .state
            .articulations_snapshot(&clip_id);
        if !prev.iter().any(|e| e.id == id) {
            return;
        }
        let next: Vec<MidiArticulationEvent> =
            prev.iter().filter(|e| e.id != id).cloned().collect();
        self.run_edit_command(
            EditCommand::SetMidiArticulations {
                clip_id,
                prev,
                next,
            },
            cx,
        );
        cx.notify();
    }

    fn note_inspector_snapshot(&self, cx: &Context<Self>, clip_id: &str) -> NoteInspectorSnapshot {
        let selected = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|note| self.selection.contains(&note.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        NoteInspectorSnapshot { selected }
    }

    /// Ids of the notes currently selected in the roll.
    ///
    /// Public so the Solfege editor's Analyze Accent can scope what it *writes*
    /// to the selection. It never scopes what the analysis *reads*: accent is a
    /// contextual judgement and a three-note selection analysed alone would be
    /// given a three-note phrase.
    pub fn selected_note_ids(&self) -> Vec<u64> {
        self.selection.iter().copied().collect()
    }

    /// The articulation every selected note carries, or `None` when the
    /// selection is empty or mixed. The inner `Option` is the value itself, so
    /// `Some(None)` means "all selected notes have no articulation".
    ///
    /// Lets the articulation controls show which value is current instead of a
    /// rail of identical buttons.
    pub(super) fn uniform_selection_articulation(
        &self,
        cx: &Context<Self>,
    ) -> Option<Option<ArticulationId>> {
        let clip_id = self.editing_clip_id(cx)?;
        let snapshot = self.note_inspector_snapshot(cx, &clip_id);
        if snapshot.selected.is_empty() {
            return None;
        }
        let first = snapshot.selected[0].articulation;
        snapshot
            .selected
            .iter()
            .all(|note| note.articulation == first)
            .then_some(first)
    }

    /// `true` when every selected note is muted, so a toggle can name what it
    /// will actually do.
    pub(super) fn selection_all_muted(&self, cx: &Context<Self>) -> bool {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return false;
        };
        let snapshot = self.note_inspector_snapshot(cx, &clip_id);
        !snapshot.selected.is_empty() && snapshot.selected.iter().all(|note| note.muted)
    }

    fn nudge_selected_pitch(&mut self, semitones: i32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
        let will_change = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes.iter().any(|note| {
                    ids.contains(&note.id)
                        && note.pitch != (note.pitch as i32 + semitones).clamp(0, 127) as u8
                })
            })
            .unwrap_or(false);
        if !will_change {
            return;
        }
        let target_ids = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.transpose_midi_notes(cid, &target_ids, semitones);
        });
    }

    /// Move the selected notes one grid step `forward` or back (see
    /// [`nudged_start`]); `free_step` is the step when the grid is `Free`.
    pub(super) fn nudge_selected_start(
        &mut self,
        forward: bool,
        free_step: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
        let grid = self.edit_grid();
        let updates: Vec<(u64, f32, u8)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|note| ids.contains(&note.id))
                    .filter_map(|note| {
                        let new_start = nudged_start(grid, note.start, forward, free_step);
                        ((note.start - new_start).abs() > 1.0e-4)
                            .then_some((note.id, new_start, note.pitch))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if updates.is_empty() {
            return;
        }
        let ids: Vec<u64> = updates.iter().map(|(id, _, _)| *id).collect();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.move_midi_notes(cid, &updates);
        });
    }

    /// Lengthen (`forward`) or shorten the selected notes by the inspector's
    /// fine step (see [`nudged_length`]); `fine_step` is that step off the
    /// "1 Bar" grid.
    pub(super) fn nudge_selected_length(
        &mut self,
        forward: bool,
        fine_step: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
        let grid = self.edit_grid();
        let updates: Vec<(u64, f32)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|note| ids.contains(&note.id))
                    .filter_map(|note| {
                        let duration =
                            nudged_length(grid, note.start, note.duration, forward, fine_step);
                        ((note.duration - duration).abs() > 1.0e-4).then_some((note.id, duration))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if updates.is_empty() {
            return;
        }
        let ids: Vec<u64> = updates.iter().map(|(id, _)| *id).collect();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            for (id, duration) in updates {
                state.set_midi_note_length(cid, id, duration);
            }
        });
    }

    fn nudge_selected_velocity(&mut self, delta: i16, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
        let updates: Vec<(u64, u8)> = self
            .timeline
            .read(cx)
            .state
            .midi_clip_notes(&clip_id)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|note| ids.contains(&note.id))
                    .filter_map(|note| {
                        let velocity = (note.velocity as i16 + delta).clamp(1, 127) as u8;
                        (note.velocity != velocity).then_some((note.id, velocity))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if updates.is_empty() {
            return;
        }
        let ids: Vec<u64> = updates.iter().map(|(id, _)| *id).collect();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            for (id, velocity) in updates {
                state.set_midi_note_velocity(cid, id, velocity);
            }
        });
    }

    /// Restore default Zoom Y (`row_h`) without touching Zoom X (`ppb`).
    fn reset_row_h_zoom(&mut self, cx: &mut Context<Self>) {
        let (_, view_h) = self.grid_view_size();
        let anchor_y = view_h * 0.5;
        let old_row = self.note_row_h();
        // The row fraction under the middle stays put; drum lanes reset to
        // their own default height.
        let anchor_row = (anchor_y + self.scroll_y) / old_row.max(0.0001);
        self.row_h = if self.drum_mode {
            DRUM_ROW_H
        } else {
            DEFAULT_ROW_H
        };
        let new_row = self.note_row_h();
        self.scroll_y = (anchor_row * new_row - anchor_y).clamp(0.0, self.max_scroll_y());
        cx.notify();
    }

    /// Multiplicative vertical zoom around a grid-local y anchor. The pitch
    /// under `anchor_y` stays fixed while `row_h` changes.
    fn zoom_row_h_around(&mut self, factor: f32, anchor_y: f32, cx: &mut Context<Self>) {
        let factor = factor.max(0.0001);
        let old_row = self.note_row_h();
        let new_row = (old_row * factor).clamp(PIANO_ROLL_MIN_ROW_H, PIANO_ROLL_MAX_ROW_H);
        if (new_row - old_row).abs() < 0.0001 {
            return;
        }
        let (_, view_h) = self.grid_view_size();
        let anchor_y = anchor_y.clamp(0.0, view_h.max(0.0));
        // Fractional pitch row under the cursor (higher pitch = smaller row index).
        let anchor_row = (anchor_y + self.scroll_y) / old_row;
        self.row_h = new_row;
        self.scroll_y = (anchor_row * new_row - anchor_y).clamp(0.0, self.max_scroll_y());
        if midi_zoom_debug_enabled() {
            eprintln!(
                "[MidiEditorZoom] old_row_h={:.4} new_row_h={:.4} anchor_y={:.2} scroll_y={:.2}",
                old_row, new_row, anchor_y, self.scroll_y,
            );
        }
        cx.notify();
    }

    /// Multiplicative horizontal zoom around a grid-local x anchor. Mirrors the
    /// arrangement timeline's `TimelineState::zoom_by`: the beat under `anchor_x`
    /// stays visually fixed while `ppb` changes, so zooming never jumps back to
    /// bar 1 / beat 0. `anchor_x` is in note-grid content space (0 = left edge of
    /// the visible grid, already net of the left piano-key lane).
    fn zoom_ppb_around(&mut self, factor: f32, anchor_x: f32, cx: &mut Context<Self>) {
        let factor = factor.max(0.0001);
        let old_ppb = self.ppb.max(0.0001);
        let new_ppb = (old_ppb * factor).clamp(PIANO_ROLL_MIN_PPB, PIANO_ROLL_MAX_PPB);
        if (new_ppb - old_ppb).abs() < 0.0001 {
            return;
        }
        // Clamp the anchor into the visible grid so a cursor over the key lane
        // (negative local x) or past the right edge can't throw the anchor beat.
        let (view_w, _) = self.grid_view_size();
        let anchor_x = anchor_x.clamp(0.0, view_w.max(0.0));
        // Beat under the anchor before the change (inverse of `beat_to_x`).
        let anchor_beat = (anchor_x + self.scroll_x) / old_ppb;
        let old_scroll_x = self.scroll_x;
        self.ppb = new_ppb;
        // Re-solve scroll_x so the same anchor_beat lands under anchor_x.
        let new_scroll_x = (anchor_beat * new_ppb - anchor_x).max(0.0);
        self.scroll_x = new_scroll_x;
        if midi_zoom_debug_enabled() {
            eprintln!(
                "[MidiEditorZoom] old_px_per_beat={:.4} new_px_per_beat={:.4} old_scroll_x={:.2} anchor_x={:.2} anchor_beat={:.4} new_scroll_x={:.2} check_beat={:.4}",
                old_ppb,
                new_ppb,
                old_scroll_x,
                anchor_x,
                anchor_beat,
                new_scroll_x,
                (anchor_x + new_scroll_x) / new_ppb,
            );
        }
        cx.notify();
    }

    /// Scale horizontal zoom by `factor`, anchored at the viewport center.
    /// Used by the toolbar zoom buttons and keyboard zoom commands.
    fn zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        let (view_w, _) = self.grid_view_size();
        self.zoom_ppb_around(factor, view_w * 0.5, cx);
    }

    /// Handle a wheel event that landed on a surface sharing this viewport
    /// (the Solfege performance lanes). The lane stack is a *sibling* of the
    /// piano roll, not a descendant, so the roll's own root handler never sees
    /// those events; forwarding here keeps one modifier map for the editor
    /// instead of a second copy that drifts.
    pub fn forward_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_wheel(event, window, cx);
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let (dx, dy) = match event.delta {
            gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            gpui::ScrollDelta::Lines(p) => (p.x * 36.0, p.y * 36.0),
        };
        // Wheel up is a *positive* delta on every platform. The zoom helpers
        // multiply by the factor, so wheel up must yield a factor above 1
        // (zoom in) — the exponent is deliberately not negated. The scroll
        // branches below do subtract it, because panning maps wheel motion onto
        // the content origin rather than onto a scale.
        // The arrangement's map: Ctrl/Cmd zooms time, Ctrl/Cmd+Shift the rows
        // (the arrangement's track zoom), and Ctrl/Cmd+Alt — waveform zoom
        // there — has no waveform to scale here, so it does nothing rather
        // than zoom something else.
        let zoom = event.modifiers.control || event.modifiers.platform;
        if zoom && event.modifiers.alt {
            return;
        }
        if zoom && event.modifiers.shift {
            let (_, view_h) = self.grid_view_size();
            let anchor_y = self
                .grid_local(event.position)
                .map(|(_, ly)| ly)
                .unwrap_or(view_h * 0.5);
            // macOS turns a Shift-held mouse wheel into a horizontal delta.
            let delta = if dy.abs() <= 0.01 { dx } else { dy };
            let factor = (1.0022_f32).powf(delta);
            self.zoom_row_h_around(factor, anchor_y, cx);
            return;
        }
        if zoom {
            // Zoom horizontal, anchored at the cursor. Fall back to the viewport
            // center if the grid hasn't been laid out yet (no captured bounds).
            let (view_w, _) = self.grid_view_size();
            let anchor_x = self
                .grid_local(event.position)
                .map(|(lx, _)| lx)
                .unwrap_or(view_w * 0.5);
            let factor = (1.0022_f32).powf(dy);
            self.zoom_ppb_around(factor, anchor_x, cx);
            return;
        }
        let max_scroll_x = self.max_scroll_x(cx);
        let natural = cx
            .try_global::<crate::settings::GlobalSettingsModel>()
            .map(|g| g.0.read(cx).current.editing.mouse.natural_scroll)
            .unwrap_or(false);
        let (dx, dy) = if natural { (-dx, -dy) } else { (dx, dy) };
        let before_x = self.scroll_x;
        if event.modifiers.shift {
            self.scroll_x = (self.scroll_x - dy - dx).clamp(0.0, max_scroll_x);
        } else {
            self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll_y());
            self.scroll_x = (self.scroll_x - dx).clamp(0.0, max_scroll_x);
        }
        self.note_user_scrolled_x(before_x, cx);
        cx.notify();
    }
}

// ── Live display geometry during a drag ─────────────────────────────────────
struct DisplayNote {
    id: u64,
    pitch: u8,
    start: f32,
    duration: f32,
    velocity: u8,
}

#[derive(Clone)]
struct NoteInspectorSnapshot {
    selected: Vec<MidiNoteState>,
}

impl NoteInspectorSnapshot {
    fn count(&self) -> usize {
        self.selected.len()
    }

    fn pitch_label(&self) -> String {
        uniform_u8(&self.selected, |n| n.pitch)
            .map(|pitch| format!("{} ({})", note_name(pitch as i32), pitch))
            .unwrap_or_else(|| "Mixed".to_string())
    }

    fn start_label(&self) -> String {
        uniform_f32(&self.selected, |n| n.start)
            .map(format_beats)
            .unwrap_or_else(|| "Mixed".to_string())
    }

    fn length_label(&self) -> String {
        uniform_f32(&self.selected, |n| n.duration)
            .map(format_beats)
            .unwrap_or_else(|| "Mixed".to_string())
    }

    fn velocity_label(&self) -> String {
        uniform_u8(&self.selected, |n| n.velocity)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "Mixed".to_string())
    }

    fn channel_label(&self) -> String {
        uniform_u8(&self.selected, |n| n.channel.raw())
            .map(|raw| MidiChannel::from_raw(raw).label())
            .unwrap_or_else(|| "Mixed".to_string())
    }

    /// "None" / articulation name when uniform across the selection, else
    /// "Mixed" (per-note assignment only; the direction lane is separate).
    fn articulation_label(&self) -> String {
        uniform_u8(&self.selected, |n| {
            n.articulation.map(|a| a.to_tag()).unwrap_or(0)
        })
        .map(|tag| match ArticulationId::from_tag(tag) {
            Some(articulation) => articulation.name().to_string(),
            None => "None".to_string(),
        })
        .unwrap_or_else(|| "Mixed".to_string())
    }

    fn end_label(&self) -> String {
        if self.selected.len() == 1 {
            let note = &self.selected[0];
            format_beats(note.start + note.duration)
        } else {
            let Some(first) = self.selected.first() else {
                return "--".to_string();
            };
            let (min_start, max_end) = self.selected.iter().fold(
                (first.start, first.start + first.duration),
                |(min_start, max_end), note| {
                    (
                        min_start.min(note.start),
                        max_end.max(note.start + note.duration),
                    )
                },
            );
            format!("{}..{}", format_beats(min_start), format_beats(max_end))
        }
    }
}

// ── CC controller lane ───────────────────────────────────────────────────────
fn cc_kind_label(kind: MidiControllerKind) -> String {
    match kind {
        MidiControllerKind::CC(1) => "CC1 Mod".to_string(),
        MidiControllerKind::CC(7) => "CC7 Volume".to_string(),
        MidiControllerKind::CC(10) => "CC10 Pan".to_string(),
        MidiControllerKind::CC(11) => "CC11 Expr".to_string(),
        MidiControllerKind::CC(64) => "CC64 Sustain".to_string(),
        MidiControllerKind::CC(n) => format!("CC{n}"),
        MidiControllerKind::PitchBend => "Pitch Bend".to_string(),
        MidiControllerKind::ChannelPressure => "Ch Pressure".to_string(),
        MidiControllerKind::PolyPressure => "Poly Pressure".to_string(),
    }
}

// ── Small toolbar helpers ───────────────────────────────────────────────────
fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn note_count_for_clip(
    cx: &Context<PianoRoll>,
    timeline: &Entity<Timeline>,
    clip_id: &str,
) -> usize {
    timeline
        .read(cx)
        .state
        .midi_clip_notes(clip_id)
        .map(|notes| notes.len())
        .unwrap_or(0)
}

fn controller_display_value(kind: MidiControllerKind, value: f32) -> String {
    match kind {
        MidiControllerKind::PitchBend => {
            let semis = (value.clamp(0.0, 1.0) * 2.0 - 1.0) * 2.0;
            format!("{semis:+.2} st")
        }
        _ => format!("{}", (value.clamp(0.0, 1.0) * 127.0).round() as i32),
    }
}

fn controller_default_value(kind: MidiControllerKind) -> f32 {
    match kind {
        MidiControllerKind::PitchBend => 0.5,
        MidiControllerKind::CC(_)
        | MidiControllerKind::ChannelPressure
        | MidiControllerKind::PolyPressure => 0.0,
    }
}

/// The key drawn at a point of the key lane, given the grid row under it
/// (`row_pitch`) and how far down that row the point is. Only a black row is
/// ambiguous: left of the black key's edge it is the black key; right of it,
/// its upper half belongs to the white key above and its lower half to the
/// white key below.
fn keyboard_key_at(row_pitch: u8, y_in_row: f32, row_h: f32, x: f32, lane_w: f32) -> u8 {
    if x <= lane_w * BLACK_KEY_SHARE || !is_black(row_pitch as i32) {
        return row_pitch;
    }
    if y_in_row < row_h * 0.5 {
        row_pitch.saturating_add(1).min((PITCH_CNT - 1) as u8)
    } else {
        row_pitch.saturating_sub(1)
    }
}

/// Lane-local y of a velocity: the exact inverse of
/// [`PianoRoll::velocity_from_local_y`], so drawn and grabbed values agree.
fn velocity_lane_y(velocity: u8, usable_h: f32) -> f32 {
    let norm = (velocity.clamp(1, 127) as f32 - 1.0) / 126.0;
    2.0 + (1.0 - norm) * usable_h
}

/// `2401` → `2,401`.
fn group_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn value_chip(label: &str, left: f32, top: f32) -> impl IntoElement {
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::surface_card())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(9.0))
        .text_color(Colors::text_primary())
        .child(label.to_string())
}

// ── Editor chrome ───────────────────────────────────────────────────────────
//
// The editor's controls speak the Studio's state language: a ghost control
// paints nothing at rest, hover composites `state.hover` over the plane it sits
// on, pressed goes recessed, and a latched control carries the accent on two
// channels — a wash and the glyph.

/// Thin rule between toolbar modules.
fn bar_divider() -> gpui::Div {
    div()
        .flex_shrink_0()
        .w(px(1.0))
        .h(px(crate::theme::size::MICRO))
        .mx(px(crate::theme::space::SNUG))
        .bg(Colors::divider())
}

/// Tooltip for a control with a command behind it: its name and, when one is
/// bound, its shortcut.
fn command_tip(label: &str, command: &str) -> gpui::SharedString {
    match crate::keymap::shortcut_for_command(command) {
        Some(keys) => format!("{label}  {keys}").into(),
        None => label.to_string().into(),
    }
}

/// Ghost icon button on the toolbar. `toggled: Some(true)` latches it.
fn bar_icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    tip: gpui::SharedString,
    toggled: Option<bool>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size};
    let active = toggled.unwrap_or(false);
    let base = Colors::surface_panel();
    let rest = if active {
        Colors::composite(base, Colors::accent_active())
    } else {
        Colors::with_alpha(base, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { base }, Colors::state_hover());
    let pressed = Colors::composite(if active { rest } else { base }, Colors::state_recessed());
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(tip.clone())
        .when_some(toggled, |b, t| {
            b.aria_toggled(if t {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
        })
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(size::DEFAULT))
        .rounded(px(radius::CONTROL))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .tooltip(crate::components::controls::fb_tooltip(tip))
        .on_click(on_click)
        .child(svg().path(icon).size(px(14.0)).text_color(if active {
            Colors::accent_primary()
        } else {
            Colors::text_secondary()
        }))
}

/// Ghost text button on the toolbar or in the footer.
fn bar_text_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    tip: Option<gpui::SharedString>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, space, typography};
    let label = label.into();
    let base = Colors::surface_panel();
    let hover = Colors::composite(base, Colors::state_hover());
    let pressed = Colors::composite(base, Colors::state_recessed());
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .h(px(size::DEFAULT))
        .px(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover).text_color(Colors::text_primary()))
        .active(move |s| s.bg(pressed))
        .when_some(tip, |b, tip| {
            b.tooltip(crate::components::controls::fb_tooltip(tip))
        })
        .on_click(on_click)
        .child(label)
}

/// One tool in the tool strip: an icon that is either the active tool or not.
fn tool_segment(
    id: &'static str,
    icon: &'static str,
    tip: gpui::SharedString,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, space};
    let track = Colors::surface_input();
    let rest = if active {
        Colors::composite(track, Colors::state_selected())
    } else {
        Colors::with_alpha(track, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { track }, Colors::state_hover());
    let pressed = Colors::composite(if active { rest } else { track }, Colors::state_recessed());
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(tip.clone())
        .aria_toggled(if active {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .items_center()
        .justify_center()
        .w(px(size::COMFORTABLE))
        .h(px(size::DENSE))
        // Inset one `HAIR` inside the strip's `CONTROL` corner.
        .rounded(px(radius::inner(radius::CONTROL, space::HAIR)))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .tooltip(crate::components::controls::fb_tooltip(tip))
        .on_click(on_click)
        .child(svg().path(icon).size(px(13.0)).text_color(if active {
            Colors::accent_primary()
        } else {
            Colors::text_muted()
        }))
}

/// The inset strip the tool segments sit in.
fn tool_strip() -> gpui::Div {
    use crate::theme::{radius, space};
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .items_center()
        .gap(px(space::HAIR))
        .p(px(space::HAIR))
        .rounded(px(radius::CONTROL))
        .bg(Colors::surface_input())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
}

// ── Note inspector ──────────────────────────────────────────────────────────

/// Section heading: a quiet caption and a rule to the edge.
fn insp_section(title: &'static str) -> gpui::Div {
    use crate::theme::{space, typography};
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .pt(px(space::BASE))
        .pb(px(space::HAIR))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(title),
        )
        .child(div().flex_1().h(px(1.0)).bg(Colors::divider()))
}

/// Label, value and — for a value the selection can nudge — a −/+ pair.
fn insp_row(label: &str, value: String, stepper: Option<gpui::AnyElement>) -> gpui::Div {
    use crate::theme::{size, space, typography};
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .h(px(size::DEFAULT))
        .child(
            div()
                .w(px(60.0))
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(value),
        )
        .children(stepper)
}

/// The −/+ pair of an inspector row.
fn insp_stepper(
    id: &'static str,
    on_down: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    on_up: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .gap(px(crate::theme::space::HAIR))
        .child(insp_step_button(
            (id, 0usize),
            assets::ICON_MINUS_PATH,
            "Decrease",
            on_down,
        ))
        .child(insp_step_button(
            (id, 1usize),
            assets::ICON_PLUS_PATH,
            "Increase",
            on_up,
        ))
        .into_any_element()
}

fn insp_step_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size};
    let rest = Colors::button_bg();
    let hover = Colors::composite(rest, Colors::state_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .flex()
        .items_center()
        .justify_center()
        .size(px(size::DENSE))
        .rounded(px(radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(Colors::button_border())
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(
            svg()
                .path(icon)
                .size(px(11.0))
                .text_color(Colors::text_secondary()),
        )
}

/// A value the selection either carries or does not — the articulation
/// palette. Latched on fill and border.
fn insp_chip(
    id: &'static str,
    label: &str,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, space, typography};
    let base = Colors::surface_panel();
    let rest = if active {
        Colors::composite(base, Colors::accent_active())
    } else {
        Colors::with_alpha(base, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { base }, Colors::state_hover());
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.to_string())
        .aria_toggled(if active {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .items_center()
        .h(px(size::DENSE))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(if active {
            Colors::accent_primary()
        } else {
            Colors::border_subtle()
        })
        .bg(rest)
        .text_size(px(typography::DENSE_LABEL))
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(on_click)
        .child(label.to_string())
}

/// A command on the selection.
fn insp_action(
    id: &'static str,
    label: &str,
    destructive: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use crate::theme::{radius, size, typography};
    let rest = Colors::button_bg();
    let hover = Colors::composite(rest, Colors::state_hover());
    let pressed = Colors::composite(rest, Colors::state_recessed());
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.to_string())
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .justify_center()
        .h(px(size::DEFAULT))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::button_border())
        .bg(rest)
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if destructive {
            Colors::status_error()
        } else {
            Colors::button_text()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .on_click(on_click)
        .child(label.to_string())
}

/// Two inspector actions side by side.
fn insp_action_row(children: Vec<gpui::AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .gap(px(crate::theme::space::TIGHT))
        .children(children)
}

fn uniform_u8(notes: &[MidiNoteState], f: impl Fn(&MidiNoteState) -> u8) -> Option<u8> {
    let first = notes.first().map(&f)?;
    if notes.iter().all(|note| f(note) == first) {
        Some(first)
    } else {
        None
    }
}

fn uniform_f32(notes: &[MidiNoteState], f: impl Fn(&MidiNoteState) -> f32) -> Option<f32> {
    let first = notes.first().map(&f)?;
    if notes.iter().all(|note| (f(note) - first).abs() <= 1.0e-4) {
        Some(first)
    } else {
        None
    }
}

fn format_beats(value: f32) -> String {
    format!("{:.3}", value.max(0.0))
}

// ── WGPU render snapshot shape (scaffold) ────────────────────────────────────
// Immutable, flat description of everything the dense viewport needs to draw,
// already culled to the visible range and resolved to pixel coordinates. Built
// on the UI thread; intended for a future WGPU renderer to consume instead of
// thousands of GPUI elements. Not yet wired into paint — the element path above
// remains the live renderer, so this only fixes the data contract.

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct NoteRenderItem {
    pub id: u64,
    pub pitch: u8,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub selected: bool,
    pub muted: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct VelocityRenderItem {
    pub id: u64,
    pub x: f32,
    pub velocity: u8,
    pub selected: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct ControllerPointRenderItem {
    pub id: u64,
    pub x: f32,
    pub value: f32,
}

/// Basic note-expression geometry for the dense editor renderer. Points are
/// already in lane-local pixels and remain grouped by note id, so a renderer
/// can overlay selected-note gestures without reconstructing transport state.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionRenderItem {
    pub note_id: u64,
    pub lane: NoteExpressionLane,
    pub points: Vec<(f32, f32)>,
    pub selected: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct MidiEditorRenderSnapshot {
    pub clip_id: String,
    pub visible_beat_range: (f32, f32),
    pub visible_pitch_range: (u8, u8),
    pub notes: Vec<NoteRenderItem>,
    pub velocity: Vec<VelocityRenderItem>,
    pub controller_points: Vec<ControllerPointRenderItem>,
    pub expression: Vec<ExpressionRenderItem>,
}

impl PianoRoll {
    /// Build the immutable render snapshot for the dense WGPU viewport path.
    /// Mirrors the visible-range culling used by the GPUI element builders so a
    /// future renderer produces identical geometry. Read-only; not yet consumed.
    #[allow(dead_code)]
    pub fn build_render_snapshot(
        &self,
        cx: &Context<Self>,
        clip_id: &str,
    ) -> MidiEditorRenderSnapshot {
        let (view_w, view_h) = self.grid_view_size();
        let first_pitch = (self.y_to_pitch(view_h) as i32 - 1).max(0) as u8;
        let last_pitch = (self.y_to_pitch(0.0) as i32 + 1).min(PITCH_CNT - 1) as u8;
        let start_beat = self.x_to_clip_beat(0.0);
        let end_beat = self.x_to_clip_beat(view_w);

        let row_h = self.note_row_h();
        let tl = self.timeline.read(cx);
        let mut notes = Vec::new();
        let mut velocity = Vec::new();
        let mut expression = Vec::new();
        if let Some(ns) = tl.state.midi_clip_notes(clip_id) {
            for n in ns {
                let d = self.display_note(n);
                let x = self.clip_beat_to_x(d.start);
                let w = (d.duration * self.ppb).max(3.0);
                let y = self.pitch_to_y(d.pitch);
                if x + w < 0.0 || x > view_w || y + row_h < 0.0 || y > view_h {
                    continue;
                }
                let selected = self.selection.contains(&d.id);
                notes.push(NoteRenderItem {
                    id: d.id,
                    pitch: d.pitch,
                    x,
                    y,
                    w,
                    selected,
                    muted: n.muted,
                });
                if x >= -8.0 && x <= view_w {
                    velocity.push(VelocityRenderItem {
                        id: d.id,
                        x,
                        velocity: d.velocity,
                        selected,
                    });
                }
                for (lane, curve) in [
                    (NoteExpressionLane::Pitch, &n.expression.pitch),
                    (NoteExpressionLane::Pressure, &n.expression.pressure),
                    (NoteExpressionLane::Timbre, &n.expression.timbre),
                ] {
                    let points = curve
                        .points
                        .iter()
                        .filter_map(|point| {
                            let px = self.clip_beat_to_x(d.start + point.position);
                            let normalized = match lane {
                                NoteExpressionLane::Pitch => {
                                    (point.value.clamp(-1.0, 1.0) + 1.0) * 0.5
                                }
                                NoteExpressionLane::Pressure | NoteExpressionLane::Timbre => {
                                    point.value.clamp(0.0, 1.0)
                                }
                            };
                            (px >= -6.0 && px <= view_w + 6.0)
                                .then_some((px, (LANE_H - 1.0) * (1.0 - normalized)))
                        })
                        .collect::<Vec<_>>();
                    if !points.is_empty() {
                        expression.push(ExpressionRenderItem {
                            note_id: d.id,
                            lane,
                            points,
                            selected,
                        });
                    }
                }
            }
        }

        let mut controller_points = Vec::new();
        if let Some(ps) = tl.state.controller_lane_points(clip_id, self.active_cc) {
            for p in ps {
                let x = self.clip_beat_to_x(p.beat);
                if x < -6.0 || x > view_w + 6.0 {
                    continue;
                }
                controller_points.push(ControllerPointRenderItem {
                    id: p.id,
                    x,
                    value: p.value,
                });
            }
        }

        MidiEditorRenderSnapshot {
            clip_id: clip_id.to_string(),
            visible_beat_range: (start_beat, end_beat),
            visible_pitch_range: (first_pitch, last_pitch),
            notes,
            velocity,
            controller_points,
            expression,
        }
    }
}

#[cfg(test)]
mod piano_key_pitch_tests {
    use super::*;

    const MAX_PITCH: u8 = (PITCH_CNT - 1) as u8;

    /// Top of the lane is the highest pitch (reversed pitch order).
    #[test]
    fn top_row_is_highest_pitch() {
        assert_eq!(local_y_to_pitch(0.0, 0.0, ROW_H), MAX_PITCH);
    }

    /// Each row down drops exactly one semitone.
    #[test]
    fn moving_down_one_row_lowers_pitch_by_one() {
        assert_eq!(local_y_to_pitch(ROW_H, 0.0, ROW_H), MAX_PITCH - 1);
        assert_eq!(local_y_to_pitch(ROW_H * 2.0, 0.0, ROW_H), MAX_PITCH - 2);
    }

    /// Scrolling shifts which pitch is at the lane top, consistently (spec test
    /// 7: scroll + drag keeps the mapping correct).
    #[test]
    fn scroll_offsets_mapping_by_whole_rows() {
        let scroll = ROW_H * 3.0;
        assert_eq!(local_y_to_pitch(0.0, scroll, ROW_H), MAX_PITCH - 3);
        // ...and a click one row further down with the same scroll is one lower.
        assert_eq!(local_y_to_pitch(ROW_H, scroll, ROW_H), MAX_PITCH - 4);
    }

    /// The key lane mapping is the exact inverse of `pitch_to_y` (the layout
    /// used to position the keys) for the middle of each key's row — so the key
    /// under the cursor always matches the pitch that gets auditioned.
    #[test]
    fn round_trips_with_key_layout() {
        let scroll = ROW_H * 2.5;
        for pitch in [0u8, 24, 60, 100, MAX_PITCH] {
            // `pitch_to_y`: (PITCH_CNT - 1 - pitch) * ROW_H - scroll.
            let row_top = (PITCH_CNT - 1 - pitch as i32) as f32 * ROW_H - scroll;
            let row_mid = row_top + ROW_H * 0.5;
            assert_eq!(
                local_y_to_pitch(row_mid, scroll, ROW_H),
                pitch,
                "pitch {pitch}"
            );
        }
    }

    /// Out-of-range rows clamp to the MIDI bounds (never panic / wrap).
    #[test]
    fn clamps_beyond_the_range() {
        assert_eq!(local_y_to_pitch(1.0e6, 0.0, ROW_H), 0);
        assert_eq!(local_y_to_pitch(-1.0e6, 0.0, ROW_H), MAX_PITCH);
    }
}

#[cfg(test)]
mod velocity_and_timing_tests {
    use super::*;

    #[test]
    fn relative_velocity_preserves_differences_and_clamps() {
        assert_eq!(relative_velocity(40, 12), 52);
        assert_eq!(relative_velocity(80, 12), 92);
        assert_eq!(relative_velocity(120, 20), 127);
        assert_eq!(relative_velocity(8, -20), 1);
    }

    #[test]
    fn drag_delta_is_derived_from_start_and_shift_is_fine() {
        let normal = velocity_drag_delta(100.0, 80.0, LANE_H, false);
        let same_frame_again = velocity_drag_delta(100.0, 80.0, LANE_H, false);
        let fine = velocity_drag_delta(100.0, 80.0, LANE_H, true);
        assert_eq!(normal, same_frame_again);
        assert!(normal > 0);
        assert!(fine > 0 && fine < normal);
    }

    #[test]
    fn linear_velocity_line_interpolates_note_values() {
        assert_eq!(
            interpolate_velocity(20, 120, 0.0, VelocityCurve::Linear),
            20
        );
        assert_eq!(
            interpolate_velocity(20, 120, 0.5, VelocityCurve::Linear),
            70
        );
        assert_eq!(
            interpolate_velocity(20, 120, 1.0, VelocityCurve::Linear),
            120
        );
    }

    #[test]
    fn future_velocity_curves_stay_in_range() {
        for curve in [
            VelocityCurve::Exponential,
            VelocityCurve::Logarithmic,
            VelocityCurve::SCurve,
            VelocityCurve::ReverseSCurve,
        ] {
            let value = interpolate_velocity(1, 127, 0.4, curve);
            assert!((1..=127).contains(&value));
        }
    }

    #[test]
    fn beat_pixel_roundtrip_respects_scroll_and_zoom() {
        let ppb = 96.0;
        let scroll = 173.5;
        for beat in [0.0, 0.25, 1.0, 7.5, 64.0] {
            let x = beat_to_local_x(beat, ppb, scroll);
            let roundtrip = local_x_to_beat(x, ppb, scroll);
            assert!((roundtrip - beat).abs() < 1.0e-5, "beat={beat}");
        }
    }

    #[test]
    fn snap_boundaries_round_to_nearest_grid_line() {
        assert_eq!(snap_beat_to_step(0.1249, 0.25), 0.0);
        assert_eq!(snap_beat_to_step(0.1251, 0.25), 0.25);
        assert_eq!(snap_beat_to_step(1.3749, 0.25), 1.25);
        assert_eq!(snap_beat_to_step(1.3751, 0.25), 1.5);
    }

    #[test]
    fn cancellation_snapshot_restores_every_affected_velocity() {
        let mut notes = vec![
            MidiNoteState::new(60, 0.0, 1.0, 20),
            MidiNoteState::new(64, 1.0, 1.0, 80),
        ];
        let snapshot = vec![(notes[0].id, 20), (notes[1].id, 80)];
        notes[0].velocity = 75;
        notes[1].velocity = 127;
        restore_velocity_values(&mut notes, &snapshot);
        assert_eq!(notes[0].velocity, 20);
        assert_eq!(notes[1].velocity, 80);
    }
}

#[cfg(test)]
mod shared_grid_step_tests {
    use super::*;

    /// The Solfege lanes and the Pitch tab snap through the viewport's
    /// published step; the notes snap through `step_beats`. Both resolve to
    /// `grid_step_beats`, so they cannot diverge — this is the regression test
    /// for Adaptive reporting 1/32 to the lanes while notes snapped to a beat.
    #[test]
    fn viewport_publishes_the_step_the_notes_snap_to() {
        for bar in [4.0f32, 3.0, 3.5] {
            for ppb in [4.0f32, 12.0, 24.0, 48.0, 96.0, 200.0, 400.0] {
                for grid in GridRes::ALL {
                    let note_step = grid_step_beats(grid, ppb, bar);
                    let lane_step = viewport_snap_step(true, grid, ppb, bar);
                    if grid.is_free() {
                        assert_eq!(lane_step, 0.0, "{grid:?} @ {ppb}");
                    } else {
                        assert_eq!(lane_step, note_step.max(MIN_NOTE_BEATS), "{grid:?} @ {ppb}");
                    }
                }
            }
        }
    }

    /// #173: "1 Bar" steps by the meter's bar, not four quarters; "1/1"
    /// stays a whole note whatever the meter.
    #[test]
    fn one_bar_steps_by_the_meters_bar() {
        assert_eq!(grid_step_beats(GridRes::Bar, 96.0, 3.0), 3.0);
        assert_eq!(grid_step_beats(GridRes::Bar, 96.0, 3.5), 3.5);
        assert_eq!(grid_step_beats(GridRes::Whole, 96.0, 3.0), 4.0);
        assert_eq!(viewport_snap_step(true, GridRes::Bar, 96.0, 3.0), 3.0);
    }

    fn meter(points: &[(f64, u16, u16)]) -> EditorMeter {
        use crate::components::timeline::timeline_state::{TimeSignatureMap, TimeSignaturePoint};
        EditorMeter::from_map(&TimeSignatureMap::with_points(
            points
                .iter()
                .enumerate()
                .map(|(i, &(beat, numerator, denominator))| {
                    TimeSignaturePoint::with_id(&format!("m{i}"), beat, numerator, denominator)
                })
                .collect(),
        ))
    }

    /// #173: bar snapping lands on the drawn bar lines in 3/4 and 6/8.
    #[test]
    fn bar_snap_follows_the_time_signature() {
        let three_four = meter(&[(0.0, 3, 4)]);
        assert_eq!(three_four.bar_beats_at(10.0), 3.0);
        assert_eq!(three_four.snap_to_bar(1.4), 0.0);
        assert_eq!(three_four.snap_to_bar(1.6), 3.0);
        assert_eq!(three_four.snap_to_bar(4.4), 3.0);
        assert_eq!(three_four.snap_to_bar(4.6), 6.0);

        // 6/8: a bar is three quarters too.
        let six_eight = meter(&[(0.0, 6, 8)]);
        assert_eq!(six_eight.bar_beats_at(0.0), 3.0);
        assert_eq!(six_eight.snap_to_bar(5.0), 6.0);
    }

    /// Bars count from each meter's start, the way the grid draws them, and
    /// the change itself is a bar line.
    #[test]
    fn bar_snap_follows_a_meter_change() {
        // 4/4 for two bars, then 3/4 from beat 8.
        let m = meter(&[(0.0, 4, 4), (8.0, 3, 4)]);
        assert_eq!(m.snap_to_bar(5.0), 4.0);
        assert_eq!(m.snap_to_bar(7.0), 8.0);
        assert_eq!(m.snap_to_bar(9.0), 8.0);
        assert_eq!(m.snap_to_bar(10.0), 11.0);
        assert_eq!(m.snap_to_bar(12.6), 14.0);
        assert_eq!(m.bar_beats_at(2.0), 4.0);
        assert_eq!(m.bar_beats_at(9.0), 3.0);
    }

    #[test]
    fn adaptive_lane_step_follows_zoom_not_the_fixed_table() {
        // `GridRes::Adaptive.fixed_beats()` is 0.0; using it published 1/32.
        assert_eq!(viewport_snap_step(true, GridRes::Adaptive, 20.0, 4.0), 1.0);
        assert_eq!(viewport_snap_step(true, GridRes::Adaptive, 4.0, 4.0), 4.0);
        assert_eq!(
            viewport_snap_step(true, GridRes::Adaptive, 300.0, 4.0),
            0.0625
        );
    }

    #[test]
    fn snapping_off_publishes_no_step() {
        for grid in GridRes::ALL {
            assert_eq!(viewport_snap_step(false, grid, 96.0, 4.0), 0.0, "{grid:?}");
        }
        // Free never snaps even with the toggle on, matching `snap_beats`.
        assert_eq!(viewport_snap_step(true, GridRes::Free, 96.0, 4.0), 0.0);
    }

    /// A published step of `0.0` must leave the beat untouched, so "snap off"
    /// on a lane means the same thing it means on the grid.
    #[test]
    fn a_zero_step_is_a_pass_through() {
        assert_eq!(snap_beat_to_step(1.234, 0.0), 1.234);
    }
}

#[cfg(test)]
mod keyboard_and_lane_tests {
    use super::*;

    const LANE_W: f32 = 72.0;
    const ROW: f32 = 14.0;

    #[test]
    fn a_black_row_is_the_black_key_left_of_its_edge() {
        // C#4 = 61.
        assert_eq!(keyboard_key_at(61, 2.0, ROW, 10.0, LANE_W), 61);
        assert_eq!(keyboard_key_at(61, 12.0, ROW, 10.0, LANE_W), 61);
    }

    #[test]
    fn right_of_a_black_key_its_row_splits_between_the_white_keys() {
        let right = LANE_W * BLACK_KEY_SHARE + 4.0;
        // Upper half of C#4's row: D4 above it; lower half: C4 below.
        assert_eq!(keyboard_key_at(61, 3.0, ROW, right, LANE_W), 62);
        assert_eq!(keyboard_key_at(61, 11.0, ROW, right, LANE_W), 60);
    }

    #[test]
    fn a_white_row_is_its_own_key_anywhere() {
        assert_eq!(keyboard_key_at(60, 7.0, ROW, 5.0, LANE_W), 60);
        assert_eq!(keyboard_key_at(60, 7.0, ROW, 70.0, LANE_W), 60);
    }

    #[test]
    fn velocity_lane_y_inverts_the_press_mapping() {
        // `velocity_from_local_y`'s arithmetic, for a 139 px lane.
        let usable = 139.0 - 8.0;
        let from_y = |y: f32| {
            let norm = (1.0 - ((y - 2.0) / usable)).clamp(0.0, 1.0);
            (1.0 + norm * 126.0).round() as u8
        };
        for velocity in [1u8, 32, 64, 100, 127] {
            assert_eq!(from_y(velocity_lane_y(velocity, usable)), velocity);
        }
    }

    #[test]
    fn counts_group_their_thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(2401), "2,401");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }
}

#[cfg(test)]
mod note_hit_geometry_tests {
    use super::*;

    /// The rubber band and the drawn block must agree: a note narrower than
    /// `NOTE_MIN_W` at the current zoom is still painted (and clicked) at
    /// `NOTE_MIN_W`, so the marquee has to intersect the same width.
    #[test]
    fn minimum_note_width_is_shared_by_drawing_and_hit_testing() {
        let ppb = 20.0f32;
        let duration = 0.01f32;
        assert!(duration * ppb < NOTE_MIN_W);
        assert_eq!((duration * ppb).max(NOTE_MIN_W), NOTE_MIN_W);
    }

    /// A note showing a resize handle always keeps at least as much move zone
    /// beside it, so grabbing the body never becomes impossible.
    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "documents the note hit-zone constants as a named test"
    )]
    fn resize_handle_never_covers_the_whole_note() {
        assert!(NOTE_RESIZE_MIN_W - RESIZE_ZONE >= RESIZE_ZONE);
    }
}

#[cfg(test)]
mod editor_meter_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{TimeSignatureMap, TimeSignaturePoint};

    fn viewport(ppb: f32, sub: f32) -> PianoRollViewport {
        PianoRollViewport {
            ppb,
            scroll_x: 0.0,
            row_h: 12.0,
            scroll_y: 0.0,
            key_lane_width: 0.0,
            snap_step_beats: sub,
            snap_to_bars: false,
            sub_step_beats: sub,
            grid_width: 2000.0,
            grid_height: 400.0,
        }
    }

    /// 4/4 for two bars, then 3/4.
    fn four_then_three() -> TimeSignatureMap {
        TimeSignatureMap::with_points(vec![
            TimeSignaturePoint::with_id("a", 0.0, 4, 4),
            TimeSignaturePoint::with_id("b", 8.0, 3, 4),
        ])
    }

    fn bars(lines: &[(f32, GridLineKind)], ppb: f32) -> Vec<f32> {
        lines
            .iter()
            .filter(|(_, kind)| *kind == GridLineKind::Bar)
            .map(|(x, _)| x / ppb)
            .collect()
    }

    #[test]
    fn bar_lines_follow_a_meter_change() {
        let v = viewport(20.0, 1.0);
        let meter = EditorMeter::from_map(&four_then_three());
        let lines = v.grid_lines_in(0.0, 18.0, &meter);
        // 0, 4, 8 in 4/4; then every 3 beats: 11, 14, 17 (and one bar of
        // overscan past the right edge, so a bar straddling it still draws).
        assert_eq!(
            bars(&lines, 20.0),
            vec![0.0, 4.0, 8.0, 11.0, 14.0, 17.0, 20.0]
        );
        // The playhead-sampled single meter got this wrong from beat 8 on.
        let constant = v.grid_lines(0.0, 18.0, 4.0);
        assert_ne!(bars(&constant, 20.0), bars(&lines, 20.0));
    }

    #[test]
    fn ruler_numbers_bars_the_way_the_time_signature_map_does() {
        let map = four_then_three();
        let v = viewport(20.0, 1.0);
        let marks = v.ruler_marks_in(0.0, 18.0, &EditorMeter::from_map(&map));
        for mark in marks.iter().filter(|m| m.on_bar) {
            let expected = map.bar_beat_at_beat(f64::from(mark.beat)).bar;
            assert_eq!(mark.label, expected.to_string(), "beat {}", mark.beat);
        }
        assert_eq!(
            marks.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "3", "4", "5", "6", "7"]
        );
    }

    #[test]
    fn pointer_positions_read_as_the_time_signature_map_counts_them() {
        let map = four_then_three();
        let meter = EditorMeter::from_map(&map);
        for beat in [0.0_f32, 1.0, 3.5, 4.0, 7.9, 8.0, 9.0, 10.5, 11.0, 17.0] {
            let bb = map.bar_beat_at_beat(f64::from(beat));
            assert_eq!(
                meter.bar_beat_label(beat),
                format!("{}.{}", bb.bar, bb.beat_in_bar),
                "beat {beat}"
            );
        }
        assert_eq!(meter.bar_beat_label(-4.0), "1.1");
    }

    #[test]
    fn odd_meters_count_their_own_beats() {
        // 7/8: seven eighth-note beats, a bar every 3.5 quarters.
        let map = TimeSignatureMap::with_points(vec![TimeSignaturePoint::with_id("a", 0.0, 7, 8)]);
        let v = viewport(40.0, 0.25);
        let lines = v.grid_lines_in(0.0, 7.0, &EditorMeter::from_map(&map));
        assert_eq!(bars(&lines, 40.0), vec![0.0, 3.5, 7.0]);
        let beats = lines
            .iter()
            .filter(|(x, kind)| *kind == GridLineKind::Beat && *x < 3.5 * 40.0)
            .count();
        assert_eq!(beats, 6, "beats 2–7 of the first bar");
        // Sixteenth subdivisions fall between the eighths, never on them.
        assert!(lines
            .iter()
            .filter(|(_, kind)| *kind == GridLineKind::Subdivision)
            .all(|(x, _)| ((x / 40.0) / 0.5).fract().abs() > 1e-3));
    }

    #[test]
    fn zoomed_out_bars_thin_from_bar_one() {
        let v = viewport(1.0, 1.0); // a 4/4 bar is 4 px
        let lines = v.grid_lines_in(0.0, 64.0, &EditorMeter::constant(4.0));
        let xs = bars(&lines, 1.0);
        assert_eq!(xs.first(), Some(&0.0));
        assert!(xs.windows(2).all(|w| w[1] - w[0] >= 18.0));
        assert!(lines.iter().all(|(_, kind)| *kind == GridLineKind::Bar));
    }
}

#[cfg(test)]
mod drag_and_selection_tests {
    use super::*;

    /// #190: a velocity paint or line with nothing selected carries note ids,
    /// so pruning has to build the clip's id set for it rather than treat the
    /// gesture's notes as deleted and cancel it after one frame.
    #[test]
    fn gestures_carrying_note_ids_are_pruned_against_the_clip() {
        let paint = PianoDrag::VelocityPaint {
            clip_id: "c".into(),
            original_notes: vec![MidiNoteState::new(60, 0.0, 1.0, 100)],
            touched: HashSet::new(),
            last_x: 0.0,
            last_y: 0.0,
        };
        assert!(paint.holds_note_ids());
        let line = PianoDrag::VelocityLine {
            clip_id: "c".into(),
            original_notes: Vec::new(),
            affected: HashSet::new(),
            anchor_beat: 0.0,
            anchor_value: 64,
            current_beat: 0.0,
            current_value: 64,
            curve: VelocityCurve::Linear,
            unsnap: false,
        };
        assert!(line.holds_note_ids());
        assert!(PianoDrag::EraseNotes {
            start_x: 0.0,
            start_y: 0.0,
            current_x: 0.0,
            current_y: 0.0,
            erased: HashSet::new(),
        }
        .holds_note_ids());
        assert!(!PianoDrag::None.holds_note_ids());
        assert!(!PianoDrag::Pan {
            last_x: 0.0,
            last_y: 0.0
        }
        .holds_note_ids());
    }

    /// #184: the view scrolls under a cursor held still; the drag delta
    /// follows the cursor, not the content.
    #[test]
    fn a_scroll_under_a_still_cursor_moves_the_drag_with_the_cursor() {
        let (ppb, row_h) = (50.0, 14.0);
        let grab = DragGrab::at(100.0, 70.0, 0.0, 0.0, ppb, row_h);
        // Same pointer, view scrolled one beat right and two rows down.
        let now = DragGrab::at(100.0, 70.0, 50.0, 28.0, ppb, row_h);
        assert_eq!(grab.delta_to(now), (1.0, -2));
        // Unscrolled: a pointer move is the old window-delta arithmetic.
        let moved = DragGrab::at(175.0, 42.0, 0.0, 0.0, ppb, row_h);
        assert_eq!(grab.delta_to(moved), (1.5, 2));
        // A still pointer over a still view: no motion.
        assert_eq!(grab.delta_to(grab), (0.0, 0));
    }

    /// Zooming about the cursor keeps the beat under it, and so the delta.
    #[test]
    fn a_zoom_under_a_still_cursor_keeps_the_drag_where_the_cursor_is() {
        let grab = DragGrab::at(200.0, 0.0, 100.0, 0.0, 50.0, 14.0); // beat 6
                                                                     // Doubled zoom, scrolled so beat 6 stays at x = 200.
        let now = DragGrab::at(200.0, 0.0, 400.0, 0.0, 100.0, 14.0);
        let (beats, rows) = grab.delta_to(now);
        assert!(beats.abs() < 1.0e-6, "{beats}");
        assert_eq!(rows, 0);
    }

    /// #172: Select All takes only the notes the channel view shows.
    #[test]
    fn select_all_skips_notes_on_hidden_channels() {
        let mut on_one = MidiNoteState::new(60, 0.0, 1.0, 100);
        on_one.channel = MidiChannel::from_ui(1);
        let mut on_two = MidiNoteState::new(62, 1.0, 1.0, 100);
        on_two.channel = MidiChannel::from_ui(2);
        let mut also_two = MidiNoteState::new(64, 2.0, 1.0, 100);
        also_two.channel = MidiChannel::from_ui(2);
        let notes = vec![on_one.clone(), on_two.clone(), also_two.clone()];

        let only_two = MidiChannelMask::single(MidiChannel::from_ui(2));
        assert_eq!(
            visible_note_ids(&notes, only_two),
            HashSet::from([on_two.id, also_two.id])
        );
        assert_eq!(visible_note_ids(&notes, MidiChannelMask::ALL).len(), 3);
        assert!(visible_note_ids(&notes, MidiChannelMask::NONE).is_empty());
    }
}

#[cfg(test)]
mod meter_grid_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        snap_automation_beat, SnapDivision, SnapSettings, SnapShape, TimeSignatureMap,
        TimeSignaturePoint,
    };

    fn map(points: &[(f64, u16, u16)]) -> TimeSignatureMap {
        TimeSignatureMap::with_points(
            points
                .iter()
                .enumerate()
                .map(|(i, &(beat, n, d))| TimeSignaturePoint::with_id(format!("m{i}"), beat, n, d))
                .collect(),
        )
    }

    fn meter(points: &[(f64, u16, u16)]) -> EditorMeter {
        EditorMeter::from_map(&map(points))
    }

    /// 4/4 for two bars, then 3/4 from beat 8: bar lines 0, 4, 8, 11, 14, 17…
    fn four_then_three() -> EditorMeter {
        meter(&[(0.0, 4, 4), (8.0, 3, 4)])
    }

    /// Duplicate's offset for a selection over clip beats `[start, end)` in a
    /// clip starting at `origin`, on the "1 Bar" grid.
    fn dup_bar(m: &EditorMeter, origin: f32, start: f32, end: f32) -> f32 {
        let offset = duplicate_offset(note_grid(m, GridRes::Bar, 96.0, origin), start, end);
        assert!(
            start + offset >= end - 1.0e-4,
            "copies of [{start}, {end}) at +{offset} overlap the originals"
        );
        offset
    }

    fn lines(m: &EditorMeter, origin: f32, a: f32, b: f32) -> Vec<f32> {
        let grid = note_grid(m, GridRes::Bar, 96.0, origin).expect("bar grid");
        controller_grid_lines(grid, a, b)
    }

    /// #237: Duplicate moves a bar's worth of notes by the meter's bar in
    /// 4/4, 3/4 and 6/8, rounding the selection length up to whole bars so
    /// the copies never land on top of the originals.
    #[test]
    fn duplicate_moves_by_whole_bars_of_the_meter() {
        let four = meter(&[(0.0, 4, 4)]);
        assert_eq!(dup_bar(&four, 0.0, 0.0, 4.0), 4.0);
        assert_eq!(dup_bar(&four, 0.0, 0.0, 3.0), 4.0);
        assert_eq!(dup_bar(&four, 0.0, 0.0, 5.0), 8.0);

        let three = meter(&[(0.0, 3, 4)]);
        assert_eq!(dup_bar(&three, 0.0, 0.0, 3.0), 3.0);
        assert_eq!(dup_bar(&three, 0.0, 0.0, 4.0), 6.0);

        let six_eight = meter(&[(0.0, 6, 8)]);
        assert_eq!(dup_bar(&six_eight, 0.0, 0.0, 3.0), 3.0);
        assert_eq!(dup_bar(&six_eight, 0.0, 0.0, 4.5), 6.0);
    }

    /// #237, the reported case: a clip starting on beat 2 (or 3) of a 4/4
    /// bar duplicates four beats of notes one bar on, not by a snapped
    /// position.
    #[test]
    fn duplicate_in_a_clip_off_a_bar_line_moves_one_bar() {
        let four = meter(&[(0.0, 4, 4)]);
        assert_eq!(dup_bar(&four, 1.0, 0.0, 4.0), 4.0);
        assert_eq!(dup_bar(&four, 2.0, 0.0, 4.0), 4.0);
        // A selection that starts between bar lines keeps its place in the bar.
        assert_eq!(dup_bar(&four, 1.0, 1.0, 3.0), 4.0);

        let three = meter(&[(0.0, 3, 4)]);
        assert_eq!(dup_bar(&three, 1.0, 0.0, 3.0), 3.0);

        let six_eight = meter(&[(0.0, 6, 8)]);
        assert_eq!(dup_bar(&six_eight, 1.5, 0.0, 3.0), 3.0);
    }

    /// #237: across a meter change the copies land where the note grid counts
    /// the next bars — a 3/4 bar moves three beats, not the four of the bar
    /// the clip starts in.
    #[test]
    fn duplicate_across_a_meter_change_lands_on_the_bars_the_grid_counts() {
        let m = four_then_three();
        // Clip at the song start.
        assert_eq!(dup_bar(&m, 0.0, 8.0, 11.0), 3.0);
        assert_eq!(dup_bar(&m, 0.0, 8.0, 14.0), 6.0);
        assert_eq!(dup_bar(&m, 0.0, 4.0, 8.0), 4.0);
        // Four beats from the middle of bar 2 reach into the 3/4: one bar on
        // keeps the selection's place in its bar (beat 10 = 8 + 2).
        assert_eq!(dup_bar(&m, 0.0, 6.0, 10.0), 4.0);
        // Four 4/4 beats copied into the 3/4 section need two of its bars.
        assert_eq!(dup_bar(&m, 0.0, 8.0, 12.0), 6.0);

        // The same bars in a clip that starts mid-bar (song beat 2): the
        // change sits at clip beat 6.
        assert_eq!(dup_bar(&m, 2.0, 2.0, 6.0), 4.0);
        assert_eq!(dup_bar(&m, 2.0, 6.0, 9.0), 3.0);
    }

    /// Fixed grids round the length up to whole steps; no grid (snap off,
    /// Free, Shift) keeps the exact span.
    #[test]
    fn duplicate_on_a_fixed_grid_or_without_one() {
        let four = meter(&[(0.0, 4, 4)]);
        let quarter = || note_grid(&four, GridRes::Quarter, 96.0, 0.5);
        assert_eq!(duplicate_offset(quarter(), 0.0, 1.0), 1.0);
        assert_eq!(duplicate_offset(quarter(), 0.0, 1.2), 2.0);
        assert_eq!(duplicate_offset(quarter(), 0.25, 0.5), 1.0);
        assert!((duplicate_offset(None, 0.0, 1.2) - 1.2).abs() < 1.0e-6);
    }

    /// #237: a snapped controller stroke or ramp writes one point per bar line
    /// of the meter — after a change inside the clip too.
    #[test]
    fn controller_lines_follow_a_meter_change_inside_the_clip() {
        let m = four_then_three();
        assert_eq!(lines(&m, 0.0, 0.0, 14.0), vec![0.0, 4.0, 8.0, 11.0, 14.0]);
        // Right-to-left gives the same ascending lines.
        assert_eq!(lines(&m, 0.0, 14.0, 0.0), vec![0.0, 4.0, 8.0, 11.0, 14.0]);
        // Clip off a bar line (song beat 2): song 4, 8, 11, 14.
        assert_eq!(lines(&m, 2.0, 2.0, 12.0), vec![2.0, 6.0, 9.0, 12.0]);
    }

    #[test]
    fn controller_lines_in_three_four_six_eight_and_off_a_bar_line() {
        assert_eq!(
            lines(&meter(&[(0.0, 3, 4)]), 0.0, 0.0, 9.0),
            vec![0.0, 3.0, 6.0, 9.0]
        );
        // 6/8 clip starting half a bar in: song 3, 6, 9.
        assert_eq!(
            lines(&meter(&[(0.0, 6, 8)]), 1.5, 1.5, 7.5),
            vec![1.5, 4.5, 7.5]
        );
        assert_eq!(
            lines(&meter(&[(0.0, 4, 4)]), 1.0, 3.0, 11.0),
            vec![3.0, 7.0, 11.0]
        );
        // Same line twice: one point, the line under the cursor.
        assert_eq!(lines(&meter(&[(0.0, 4, 4)]), 0.0, 4.0, 4.0), vec![4.0]);
    }

    /// The controller lane's bar lines are the ones the note grid draws.
    #[test]
    fn controller_lines_are_the_drawn_bar_lines() {
        let m = four_then_three();
        let v = PianoRollViewport {
            ppb: 20.0,
            scroll_x: 0.0,
            row_h: 12.0,
            scroll_y: 0.0,
            key_lane_width: 0.0,
            snap_step_beats: 1.0,
            snap_to_bars: false,
            sub_step_beats: 1.0,
            grid_width: 2000.0,
            grid_height: 400.0,
        };
        let drawn: Vec<f32> = v
            .grid_lines_in(0.0, 20.0, &m)
            .iter()
            .filter(|(_, kind)| *kind == GridLineKind::Bar)
            .map(|(x, _)| x / 20.0)
            .filter(|beat| *beat <= 20.0)
            .collect();
        assert_eq!(lines(&m, 0.0, 0.0, 20.0), drawn);
    }

    /// #237: the arrangement's automation lanes and the piano roll's notes
    /// and controller lanes snap through the same grid: the same bars, the
    /// same lines, across meter changes.
    #[test]
    fn automation_and_controller_lanes_snap_where_the_notes_do() {
        let points = [(0.0, 4, 4), (8.0, 3, 4), (17.0, 6, 8)];
        let song = map(&points);
        let editor = EditorMeter::from_map(&song);
        for (grid_res, division) in [
            (GridRes::Bar, SnapDivision::Bar1),
            (GridRes::Quarter, SnapDivision::Div1_4),
            (GridRes::Sixteenth, SnapDivision::Div1_16),
        ] {
            let snap = SnapSettings {
                enabled: true,
                division,
                shape: SnapShape::Straight,
                beats_per_bar: 4.0,
                auto_step_beats: 0.25,
            };
            let notes = note_grid(&editor, grid_res, 96.0, 0.0).expect("grid");
            for i in 0..=96 {
                let beat = i as f32 * 0.27;
                let note = (notes.snap(f64::from(beat)) as f32).max(0.0);
                let automation = snap_automation_beat(beat, snap, &song, false);
                assert!(
                    (note - automation).abs() < 1.0e-4,
                    "{grid_res:?} at {beat}: notes {note}, automation {automation}"
                );
            }
        }
        // And the bars themselves agree beat for beat.
        for i in 0..=120 {
            let beat = f64::from(i) * 0.25;
            assert_eq!(song.bar_span(beat), editor.bar_span(beat), "beat {beat}");
        }
    }

    /// #262 step 3: with a 3/4 change at beat 8, "1 Bar" nudges walk the
    /// drawn bar lines 4, 8, 11, 14 — one step on from beat 8 is 11, not 12.
    #[test]
    fn bar_nudge_walks_the_drawn_bar_lines() {
        let m = four_then_three();
        let grid = note_grid(&m, GridRes::Bar, 96.0, 0.0);
        assert_eq!(nudged_start(grid, 8.0, true, 0.0), 11.0);
        assert_eq!(nudged_start(grid, 11.0, true, 0.0), 14.0);
        assert_eq!(nudged_start(grid, 11.0, false, 0.0), 8.0);
        assert_eq!(nudged_start(grid, 8.0, false, 0.0), 4.0);
        // Off a line: forward to the next line, back to the line before it.
        assert_eq!(nudged_start(grid, 9.5, true, 0.0), 11.0);
        assert_eq!(nudged_start(grid, 9.5, false, 0.0), 8.0);
        // Never before the clip start.
        assert_eq!(nudged_start(grid, 0.0, false, 0.0), 0.0);

        // A clip at song beat 2 in 4/4: its bar lines are clip beats 2, 6, …
        let off_bar = note_grid(&m, GridRes::Bar, 96.0, 2.0);
        assert_eq!(nudged_start(off_bar, 0.0, true, 0.0), 2.0);
        assert_eq!(nudged_start(off_bar, 1.0, true, 0.0), 2.0);
        assert_eq!(nudged_start(off_bar, 2.0, true, 0.0), 6.0);
        // Song beat 8 is the 3/4 change: the next bar line is song beat 11.
        assert_eq!(nudged_start(off_bar, 6.0, true, 0.0), 9.0);

        // A fixed step still moves by the step, keeping the note's offset;
        // no grid (Free) moves by the step the caller gives.
        let quarter = note_grid(&m, GridRes::Quarter, 96.0, 2.0);
        assert_eq!(nudged_start(quarter, 1.5, true, 0.0), 2.5);
        assert_eq!(nudged_start(quarter, 1.5, false, 0.0), 0.5);
        assert_eq!(nudged_start(None, 1.5, true, 0.25), 1.75);
        assert_eq!(nudged_start(None, 1.5, true, 0.0), 1.5);
    }

    /// #262: the inspector's fine Length step on "1 Bar" is a quarter of the
    /// bar at the note's end: 1 beat in 4/4, 0.75 in 3/4.
    #[test]
    fn bar_length_nudge_follows_the_bar_at_the_note_end() {
        let m = four_then_three();
        let grid = note_grid(&m, GridRes::Bar, 96.0, 0.0);
        assert_eq!(nudged_length(grid, 0.0, 2.0, true, 9.0), 3.0);
        assert_eq!(nudged_length(grid, 8.0, 1.0, true, 9.0), 1.75);
        // Shrinking back across the change uses the 4/4 bar before it.
        assert_eq!(nudged_length(grid, 4.0, 4.0, false, 9.0), 3.0);
        assert_eq!(nudged_length(grid, 4.0, 5.0, false, 9.0), 4.25);
        // A clip at song beat 2: clip beat 6 + 1 is song 9, in 3/4.
        let off_bar = note_grid(&m, GridRes::Bar, 96.0, 2.0);
        assert_eq!(nudged_length(off_bar, 6.0, 1.0, true, 9.0), 1.75);
        // Other grids keep the caller's fine step.
        let quarter = note_grid(&m, GridRes::Quarter, 96.0, 0.0);
        assert_eq!(nudged_length(quarter, 0.0, 1.0, true, 0.25), 1.25);
        assert_eq!(nudged_length(None, 0.0, 1.0, false, 0.25), 0.75);
    }

    /// #262: a drum hit fills the bar it was clicked in — the bar drawn there.
    #[test]
    fn bar_drum_hits_land_on_the_drawn_bar() {
        let m = four_then_three();
        let grid = note_grid(&m, GridRes::Bar, 96.0, 0.0);
        assert_eq!(drum_cell_start(grid, 9.5), 8.0);
        assert_eq!(drum_cell_start(grid, 12.2), 11.0);
        // A clip at song beat 2: the bar from song beat 4 starts at clip 2.
        let off_bar = note_grid(&m, GridRes::Bar, 96.0, 2.0);
        assert_eq!(drum_cell_start(off_bar, 3.2), 2.0);
        assert_eq!(drum_cell_start(off_bar, 7.5), 6.0);
        // Before the first bar line inside the clip: the clip start.
        assert_eq!(drum_cell_start(off_bar, 1.0), 0.0);
        // A fixed step counts from the clip start, as before.
        let quarter = note_grid(&m, GridRes::Quarter, 96.0, 2.0);
        assert_eq!(drum_cell_start(quarter, 1.7), 1.0);
        assert_eq!(drum_cell_start(None, 1.7), 1.7);
    }

    /// #262: a "1 Bar" note drawn in the 3/4 section is three beats long,
    /// and one drawn across it counts the bars there.
    #[test]
    fn bar_drawn_notes_are_as_long_as_the_bar_they_start_in() {
        let m = four_then_three();
        let grid = note_grid(&m, GridRes::Bar, 96.0, 0.0);
        assert_eq!(drawn_note_length(grid, 0.0, 0.0), 4.0);
        assert_eq!(drawn_note_length(grid, 8.0, 8.0), 3.0);
        assert_eq!(drawn_note_length(grid, 8.0, 11.0), 3.0);
        assert_eq!(drawn_note_length(grid, 8.0, 12.0), 6.0);
        assert_eq!(drawn_note_length(grid, 4.0, 9.0), 7.0);
        // A fixed step rounds up to whole steps, at least one.
        let quarter = note_grid(&m, GridRes::Quarter, 96.0, 0.0);
        assert_eq!(drawn_note_length(quarter, 1.0, 1.0), 1.0);
        assert_eq!(drawn_note_length(quarter, 1.0, 2.5), 2.0);
        // Unsnapped: the span as drawn.
        assert_eq!(drawn_note_length(None, 1.0, 2.5), 1.5);
    }

    /// #263: the Solfege lanes and the Pitch ruler snap "1 Bar" to the drawn
    /// bar lines through the shared viewport — after a 3/4 change at beat 8
    /// that is beat 11, not 12 — and in a clip that starts off a bar line.
    #[test]
    fn viewport_bar_snap_follows_the_meter_and_the_clip_origin() {
        let m = four_then_three();
        let v = PianoRollViewport {
            ppb: 20.0,
            scroll_x: 0.0,
            row_h: 12.0,
            scroll_y: 0.0,
            key_lane_width: 0.0,
            snap_step_beats: 4.0,
            snap_to_bars: true,
            sub_step_beats: 1.0,
            grid_width: 2000.0,
            grid_height: 400.0,
        };
        assert_eq!(v.snap_in_clip(10.6, &m, 0.0), 11.0);
        assert_eq!(v.snap_in_clip(11.4, &m, 0.0), 11.0);
        assert_eq!(v.snap_in_clip(13.0, &m, 0.0), 14.0);
        // Clip at song beat 2: bar lines at clip beats 2, 6, 9 (song 4, 8, 11).
        assert_eq!(v.snap_in_clip(1.5, &m, 2.0), 2.0);
        assert_eq!(v.snap_in_clip(8.4, &m, 2.0), 9.0);
        // The same beats the note grid snaps to.
        let notes = note_grid(&m, GridRes::Bar, 96.0, 2.0).expect("bar grid");
        for i in 0..=80 {
            let beat = i as f32 * 0.23;
            let note = (notes.snap(f64::from(beat)) as f32).max(0.0);
            assert_eq!(v.snap_in_clip(beat, &m, 2.0), note, "beat {beat}");
        }
        // A fixed step (or snapping off) keeps the plain step snap.
        let fixed = PianoRollViewport {
            snap_to_bars: false,
            snap_step_beats: 1.0,
            ..v
        };
        assert_eq!(fixed.snap_in_clip(10.6, &m, 0.0), 11.0);
        assert_eq!(fixed.snap_in_clip(11.6, &m, 0.0), 12.0);
    }
}

#[cfg(test)]
mod click_vs_drag_tests {
    use super::*;

    const PPB: f32 = 96.0;
    const ROW: f32 = 14.0;
    /// Window position of the note grid's top-left corner.
    const GRID_ORIGIN: (f32, f32) = (80.0, 60.0);
    const PRESS: (f32, f32) = (300.0, 200.0);

    /// The content point under window `pointer` with the view scrolled to
    /// `scroll` — what `PianoRoll::drag_grab_at` computes.
    fn content(pointer: (f32, f32), scroll: (f32, f32)) -> DragGrab {
        DragGrab::at(
            pointer.0 - GRID_ORIGIN.0,
            pointer.1 - GRID_ORIGIN.1,
            scroll.0,
            scroll.1,
            PPB,
            ROW,
        )
    }

    /// #242: follow playback scrolls the view under a pointer held still on a
    /// note. Every frame the drag is re-derived (as on render); the note must
    /// not move, and the release is a click — `on_up` returns before any
    /// edit command, so no undo step is recorded.
    #[test]
    fn a_still_click_during_follow_playback_does_not_move_the_note() {
        let grab = content(PRESS, (0.0, 0.0));
        let mut travel = PointerTravel::new(PRESS);
        let mut scroll = (0.0, 0.0);
        for _frame in 0..12 {
            scroll.0 += 6.0; // ~150 ms of follow-playhead at 60 fps
            assert!(!travel.track(PRESS));
            let now = content(PRESS, scroll);
            assert_eq!(note_drag_delta(travel, grab, now), (0.0, 0));
        }
        assert!(move_release_is_click(travel));
    }

    /// #242: the same on a note's resize handle — the length stays, even for
    /// a note whose end is off the grid (a re-snap would change it).
    #[test]
    fn a_still_press_on_a_resize_handle_during_scroll_does_not_resize() {
        let grab = content(PRESS, (0.0, 0.0));
        let mut travel = PointerTravel::new(PRESS);
        let snap = |beat: f32| snap_beat_to_step(beat, 0.25);
        let (anchor_start, prev_dur) = (2.0, 0.9);
        let mut scroll = (0.0, 0.0);
        for _frame in 0..12 {
            scroll.0 += 6.0;
            travel.track(PRESS);
            let now = content(PRESS, scroll);
            assert_eq!(
                resize_drag_duration(travel, grab, now, anchor_start, prev_dur, snap),
                prev_dur
            );
        }
    }

    /// #242: a modifier click that deselects on release (Shift on a selected
    /// note) still deselects while the wheel scrolls the view under it, in
    /// either axis. (Ctrl/Cmd-click toggles the note on press and never
    /// starts a drag, so scrolling cannot touch it.)
    #[test]
    fn a_click_to_deselect_survives_wheel_scrolling() {
        let grab = content(PRESS, (0.0, 0.0));
        let mut travel = PointerTravel::new(PRESS);
        travel.track(PRESS);
        for scroll in [(0.0, 42.0), (120.0, 0.0), (240.0, 70.0)] {
            // Nothing moves while the view scrolls, and the release is a click.
            assert_eq!(
                note_drag_delta(travel, grab, content(PRESS, scroll)),
                (0.0, 0)
            );
            assert!(move_release_is_click(travel));
        }
    }

    /// Hand jitter inside the slop stays a click; past it the press is a drag
    /// for good, even if the pointer comes back.
    #[test]
    fn the_threshold_is_pointer_travel_and_latches() {
        let mut travel = PointerTravel::new(PRESS);
        assert!(!travel.track((PRESS.0 + 2.0, PRESS.1)));
        assert!(!travel.track((PRESS.0 + 1.5, PRESS.1 + 1.5)));
        assert!(travel.track((PRESS.0, PRESS.1 + 3.0)));
        assert!(travel.track(PRESS));
    }

    /// #184 kept: once the pointer has really moved, the dragged note
    /// follows the content under the cursor through a scroll.
    #[test]
    fn a_real_drag_during_scroll_still_follows_the_content() {
        let grab = content(PRESS, (0.0, 0.0));
        let mut travel = PointerTravel::new(PRESS);
        let moved = (PRESS.0 + 24.0, PRESS.1);
        assert!(travel.track(moved));
        // Pointer held still at `moved`, view scrolled one beat and a row.
        let now = content(moved, (PPB, ROW));
        let (beats, rows) = note_drag_delta(travel, grab, now);
        assert!((beats - (24.0 + PPB) / PPB).abs() < 1.0e-5, "{beats}");
        assert_eq!(rows, -1);
        assert_eq!((beats, rows), grab.delta_to(now));
        assert!(!move_release_is_click(travel));
    }
}
