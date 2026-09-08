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
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, deferred, div, fill, point, pulsating_between, px, size, svg, Animation, AnimationExt,
    AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, ScrollWheelEvent, StatefulInteractiveElement, Styled, Subscription, Window,
};

use crate::assets;
use crate::components::edit::{normalize_range, EditCommand};
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::{
    midi_debug_enabled, ArticulationId, MidiArticulationEvent, MidiChannel, MidiChannelMask,
    MidiControllerKind, MidiControllerPoint, MidiNoteState, PitchCurve, PitchTransformContext,
    ScaleKind, ScaleRoot, TimelineState, MIN_NOTE_BEATS,
};
use crate::theme::Colors;

// ── Layout constants (CSS px) ───────────────────────────────────────────────
mod articulation_lane;
mod cc_lane;
mod cc_lane_render;
pub mod playhead;
mod render;
pub mod scope;

/// Default note-row height (px per semitone). Reset-zoom restores this value so
/// note height matches the editor's default density after Zoom Y changes.
const DEFAULT_ROW_H: f32 = 14.0;
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
    /// Opacity the MIDI editor paints this tier at, over
    /// [`Colors::text_primary`]. Shared so every surface in the editor draws
    /// the same grid weight.
    pub fn alpha(self) -> f32 {
        match self {
            Self::Bar => 0.26,
            Self::Beat => 0.13,
            Self::Subdivision => 0.06,
        }
    }
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

    /// Bar/beat marks across `[start_beat, end_beat]`.
    ///
    /// Labels each beat when zoomed in past 36 px/beat, otherwise labels bar
    /// starts, doubling the bar step until labels are at least 56 px apart.
    /// The single source of ruler geometry for the whole MIDI editor.
    pub fn ruler_marks(&self, start_beat: f32, end_beat: f32, bpb: f32) -> Vec<RulerMark> {
        let ppb = self.ppb.max(0.0001);
        let bpb = bpb.max(1.0);
        let label_beats = ppb >= 36.0;
        let step = if label_beats {
            1.0
        } else if ppb * bpb >= 56.0 {
            bpb
        } else {
            let mut bars = 2.0_f32;
            while bars * bpb * ppb < 56.0 && bars < 256.0 {
                bars *= 2.0;
            }
            bpb * bars
        };

        let mut out = Vec::new();
        let mut beat = (start_beat / step).floor() * step;
        let mut guard = 0;
        while beat <= end_beat + step && guard < 2000 {
            guard += 1;
            let b = beat;
            beat += step;
            if b < -1.0e-3 {
                continue;
            }
            let bar = (b / bpb).floor() as i32 + 1;
            let on_bar = is_multiple(b, bpb);
            let label = if label_beats {
                let beat_in_bar = (b - (bar - 1) as f32 * bpb).floor() as i32 + 1;
                format!("{bar}.{beat_in_bar}")
            } else {
                format!("{bar}")
            };
            out.push(RulerMark {
                x: self.beat_to_x(b),
                beat: b,
                label,
                on_bar,
            });
        }
        out
    }

    /// Vertical grid lines across `[start_beat, end_beat]`, thinned by zoom.
    /// The single source of grid geometry for the whole MIDI editor.
    pub fn grid_lines(&self, start_beat: f32, end_beat: f32, bpb: f32) -> Vec<(f32, GridLineKind)> {
        let ppb = self.ppb.max(0.0001);
        let bpb = bpb.max(1.0);
        let show_beats = ppb >= 10.0;
        let sub_step = self.sub_step_beats.max(1.0 / 32.0);
        let show_subs = show_beats && sub_step * ppb >= 7.0 && ppb >= 24.0;
        let bar_step = if ppb * bpb >= 18.0 {
            bpb
        } else {
            let mut bars = 2.0_f32;
            while bars * bpb * ppb < 18.0 && bars < 256.0 {
                bars *= 2.0;
            }
            bpb * bars
        };

        let iter_step = if show_subs {
            sub_step
        } else if show_beats {
            1.0
        } else {
            bar_step
        };

        let mut out = Vec::new();
        let mut beat = (start_beat / iter_step).floor() * iter_step;
        let mut guard = 0;
        while beat <= end_beat + iter_step && guard < 8000 {
            guard += 1;
            let b = beat;
            beat += iter_step;
            if b < -1.0e-3 {
                continue;
            }
            let kind = if is_multiple(b, bpb) {
                GridLineKind::Bar
            } else if is_multiple(b, 1.0) {
                GridLineKind::Beat
            } else {
                GridLineKind::Subdivision
            };
            let keep = match kind {
                GridLineKind::Bar => is_multiple(b, bar_step),
                GridLineKind::Beat => show_beats,
                GridLineKind::Subdivision => show_subs,
            };
            if keep {
                out.push((self.beat_to_x(b), kind));
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
fn grid_step_beats(grid_res: GridRes, ppb: f32) -> f32 {
    match grid_res {
        GridRes::Free => 0.0,
        GridRes::Adaptive => adaptive_step_beats(ppb),
        other => other.fixed_beats(),
    }
}

/// Snap step published on [`PianoRollViewport`]. `0.0` when snapping is off or
/// the grid is `Free` — the same two conditions [`PianoRoll::snap_beats`]
/// treats as "do not snap".
fn viewport_snap_step(snap_on: bool, grid_res: GridRes, ppb: f32) -> f32 {
    if !snap_on || grid_res.is_free() {
        return 0.0;
    }
    grid_step_beats(grid_res, ppb).max(MIN_NOTE_BEATS)
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
        key_lane_width: 72.0,
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
const RULER_H: f32 = 18.0; // bar/beat ruler header height
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
}

/// Internal clipboard format version. Bumped if [`ClipboardNote`] layout or
/// semantics change so a paste can reject data it doesn't understand instead of
/// mis-reading it. The clipboard is process-local today, but versioning keeps
/// the contract explicit for a future cross-process / serialized clipboard.
/// v2 added the per-note MIDI channel. v3 added the per-note articulation.
/// v4 added the per-note pitch curve.
const MIDI_CLIPBOARD_VERSION: u32 = 4;

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
    ScaleRoot,
    ScaleKind,
    Lane,
    /// Channel view filter (All Channels / Channel 1–16).
    Channel,
    /// Articulation palette for the lane's insert tool (which articulation a
    /// lane click inserts).
    Articulation,
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
    /// (id, start, pitch). `dx_beats` / `dpitch` are the live, snapped deltas.
    /// `grab_pitch` is the original pitch of the note under the pointer — the
    /// anchor for the live audition preview while the pitch is dragged.
    Move {
        start_x: f32,
        start_y: f32,
        prev: Vec<(u64, f32, u8)>,
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
    },
    /// Resize notes from a right-edge handle. If the grabbed note is part of a
    /// multi-selection, every selected note receives the same duration delta.
    /// `prev_durs` snapshots each affected note so live geometry and undo stay
    /// coherent without writing timeline state on mouse move.
    Resize {
        id: u64,
        ids: Vec<u64>,
        start_x: f32,
        prev_dur: f32,
        prev_durs: Vec<(u64, f32)>,
        /// Original start of the grabbed note; right-edge snapping is performed
        /// against `anchor_start + duration`, not duration in isolation.
        anchor_start: f32,
        delta_dur: f32,
        new_dur: f32,
        /// Live Shift-held state, refreshed every mouse move (see
        /// `PianoDrag::Move::unsnap`).
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
    /// at drag start; each point moves by the same Δbeat/Δvalue from the grabbed
    /// anchor so relative offsets are preserved. One undo entry on release.
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
    /// The clip the view was last framed for.
    ///
    /// With a project-beat axis, opening a clip that starts at bar 33 would
    /// otherwise leave the view at bar 1 looking at empty grid. This is what
    /// notices the target changed so the view can go to it — once, so a user
    /// who then scrolls away is not dragged back every frame.
    framed_clip_id: Option<String>,
    /// Where the playhead is this frame. Shared with the overlay entity below
    /// so the line can move without rebuilding the editor around it.
    playhead_frame: playhead::PianoRollPlayheadFrameCell,
    /// The playhead's own entity, created on first render because it needs no
    /// geometry until there is some.
    playhead_overlay: Option<Entity<playhead::PianoRollPlayheadOverlay>>,
    /// Docked editor only: opens the floating MIDI editor window.
    on_pop_out: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    on_midi_preview:
        Option<std::sync::Arc<dyn Fn(UiMidiPreviewCommand, &mut gpui::App) + Send + Sync>>,
    tool: PianoTool,
    /// Horizontal zoom: pixels per beat (Zoom X).
    ppb: f32,
    /// Vertical zoom: pixels per semitone / note-row height (Zoom Y).
    /// Independent of [`Self::ppb`]. Reset restores [`DEFAULT_ROW_H`].
    row_h: f32,
    snap_on: bool,
    grid_res: GridRes,
    /// Quantize strength resolution, independent of the visual grid.
    quantize_res: GridRes,
    selection: HashSet<u64>,
    scroll_x: f32,
    scroll_y: f32,
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
    /// Lane points snapshotted when a CC paint/erase gesture begins (undo prev).
    cc_edit_prev: Option<Vec<MidiControllerPoint>>,
    /// Clip/controller captured with `cc_edit_prev`; cancellation and commit must
    /// never resolve against a different clip or lane selected mid-gesture.
    cc_edit_target: Option<(String, MidiControllerKind)>,
    /// Selected CC point ids in the active controller lane (multi-edit).
    cc_selection: HashSet<u64>,
    /// Point selection captured when a CC marquee starts.
    cc_selection_before_marquee: HashSet<u64>,
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
            framed_clip_id: None,
            playhead_frame: std::rc::Rc::new(std::cell::Cell::new(
                playhead::PianoRollPlayheadFrame::default(),
            )),
            playhead_overlay: None,
            on_pop_out: None,
            on_midi_preview: None,
            tool: PianoTool::Draw,
            ppb: 80.0,
            row_h: DEFAULT_ROW_H,
            snap_on: true,
            grid_res: GridRes::Sixteenth,
            quantize_res: GridRes::Sixteenth,
            selection: HashSet::new(),
            scroll_x: 0.0,
            scroll_y: 0.0,
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
            cc_edit_prev: None,
            cc_edit_target: None,
            cc_selection: HashSet::new(),
            cc_selection_before_marquee: HashSet::new(),
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
            channel_view: MidiChannelMask::ALL,
        }
    }

    pub fn set_pop_out_handler(
        &mut self,
        handler: Option<std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + Send + Sync>>,
    ) {
        self.on_pop_out = handler;
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
        let needs_note_ids = !self.selection.is_empty()
            || !self.selection_before_marquee.is_empty()
            || !self.erase_preview_ids.is_empty();
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
            PianoDrag::Move { prev, .. } => {
                prev.retain(|(id, _, _)| valid_note_ids.contains(id));
                prev.is_empty()
            }
            PianoDrag::Resize { id, .. } => !valid_note_ids.contains(id),
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

    fn toolbar_status(&self, note_count: usize, sel_count: usize) -> String {
        let pointer = match (self.hover_pitch, self.hover_beat) {
            (Some(pitch), Some(beat)) => format!("{} @ {:.2}", note_name(pitch as i32), beat),
            _ => "Pointer: —".to_string(),
        };
        let drag = match &self.drag {
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
                ..
            } => {
                let (lo, hi) = normalize_range(*start_beat, *end_beat);
                format!(
                    "Draw {} · {:.2}+{:.2}",
                    note_name(*pitch as i32),
                    lo,
                    (hi - lo).max(self.step_beats())
                )
            }
            PianoDrag::Move {
                dx_beats, dpitch, ..
            } => format!("Move Δ{:.2} beat Δ{} st", dx_beats, dpitch),
            PianoDrag::Resize { new_dur, .. } => format!("Length {:.2}", new_dur),
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
        };
        format!("{} notes · {} sel · {}", note_count, sel_count, drag)
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
                        .map(|notes| notes.iter().map(|note| note.id).collect())
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
            "midi:nudge-left" => self.nudge_selected_start(-self.step_beats(), cx),
            "midi:nudge-right" => self.nudge_selected_start(self.step_beats(), cx),
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

    fn step_beats(&self) -> f32 {
        grid_step_beats(self.grid_res, self.ppb)
    }

    fn snap_beats(&self, beats: f32) -> f32 {
        if !self.snap_on || self.grid_res.is_free() {
            return beats.max(0.0);
        }
        let step = self.step_beats();
        if step <= 0.0 {
            return beats.max(0.0);
        }
        // Round (not floor) so drag jitter near a grid line settles stably
        // instead of snapping one step early under the cursor.
        snap_beat_to_step(beats, step)
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
        local_y_to_pitch(local_y, self.scroll_y, self.row_h)
    }
    fn pitch_to_y(&self, pitch: u8) -> f32 {
        (PITCH_CNT - 1 - pitch as i32) as f32 * self.row_h - self.scroll_y
    }

    /// Current Zoom Y (px per semitone). Prefer this over the `DEFAULT_ROW_H`
    /// constant in render/hit-test paths so vertical zoom stays coherent.
    #[inline]
    fn note_row_h(&self) -> f32 {
        self.row_h.max(0.0001)
    }

    fn total_pitch_h(&self) -> f32 {
        PITCH_CNT as f32 * self.note_row_h()
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
        Some(self.y_to_pitch(local_y))
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
            key_lane_width: key_lane_width(),
            snap_step_beats: viewport_snap_step(self.snap_on, self.grid_res, self.ppb),
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
        let span = (max_pitch - min_pitch).max(1.0);
        let fit_h = (view_h - PIANO_ROLL_FIT_PAD_PX * 2.0).max(24.0);
        let new_row_h = (fit_h / span).clamp(PIANO_ROLL_MIN_ROW_H, PIANO_ROLL_MAX_ROW_H);
        self.row_h = new_row_h;
        let anchor_pitch = (min_pitch + max_pitch) * 0.5;
        let target_top = (PITCH_CNT - 1) as f32 * new_row_h - anchor_pitch * new_row_h;
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
        let target = ((PITCH_CNT - 1) as f32 - pitch as f32) * row_h - view_h * 0.5 + row_h * 0.5;
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
            (lo.saturating_sub(6), hi.saturating_add(6))
        };

        let mid = (min_p as f32 + max_p as f32) * 0.5;
        let row_h = self.note_row_h();
        let target_scroll = ((PITCH_CNT - 1) as f32 - mid) * row_h - view_h * 0.5 + row_h * 0.5;
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
                let pitch = self.pitch_ctx.constrain_pitch(self.y_to_pitch(ly));
                let unsnap = event.modifiers.shift;
                let start = self.snap_beats_live(self.x_to_clip_beat(lx), unsnap);
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
        if shift || ctrl {
            // Toggle this note in/out of the selection — no drag.
            if self.selection.contains(&id) {
                self.selection.remove(&id);
            } else {
                self.selection.insert(id);
            }
            cx.notify();
            return;
        }
        if !self.selection.contains(&id) {
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
        self.drag = PianoDrag::Move {
            start_x: event.position.x.into(),
            start_y: event.position.y.into(),
            prev,
            dx_beats: 0.0,
            dpitch: 0,
            grab_pitch,
            anchor_start,
            clone_on_commit,
            unsnap: false,
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
        if !multi {
            self.selection = HashSet::from([id]);
        }
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let selected = self.selection.clone();
        let prev_durs: Vec<(u64, f32)> = self
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
        let ids: Vec<u64> = prev_durs.iter().map(|(note_id, _)| *note_id).collect();
        self.drag = PianoDrag::Resize {
            id,
            ids,
            start_x: event.position.x.into(),
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
                Some(pitch) => {
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
            PianoDrag::CcSelect { .. } => {
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.update_cc_select(lx, ly, cx);
                }
                return;
            }
            PianoDrag::CcPaint { erase, .. } => {
                // Alt is the live "free" modifier: releasing the grid mid-stroke
                // takes effect on the next segment rather than only at press.
                if let PianoDrag::CcPaint { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.alt;
                }
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.cc_paint_stroke_to(lx, ly, erase, cx);
                }
                return;
            }
            PianoDrag::CcMove { .. } => {
                if let PianoDrag::CcMove { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.alt || event.modifiers.shift;
                }
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.cc_move_selection_to(lx, ly, cx);
                }
                return;
            }
            PianoDrag::CcLine {
                anchor_beat,
                anchor_value,
                ..
            } => {
                if let PianoDrag::CcLine { unsnap, .. } = &mut self.drag {
                    *unsnap = event.modifiers.alt
                        || (self.tool == PianoTool::Line && event.modifiers.shift);
                }
                if let Some((lx, ly)) = self.cc_local(event.position) {
                    self.cc_line_to(anchor_beat, anchor_value, lx, ly, cx);
                }
                return;
            }
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
                self.scroll_x = (self.scroll_x - dx).clamp(0.0, max_scroll_x);
                self.scroll_y = (self.scroll_y - dy).clamp(0.0, max_scroll_y);
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
            let cur_x: f32 = event.position.x.into();
            let cur_y: f32 = event.position.y.into();
            let ppb = self.ppb.max(0.0001);
            let row_h = self.note_row_h();
            let pitch_ctx = self.pitch_ctx;
            let mut audition_pitch: Option<u8> = None;
            if let PianoDrag::Move {
                start_x,
                start_y,
                dx_beats,
                dpitch,
                grab_pitch,
                unsnap,
                ..
            } = &mut self.drag
            {
                // Store the raw beat delta; snapping is applied per-note against
                // each note's absolute start in `display_note` / commit.
                *dx_beats = (cur_x - *start_x) / ppb;
                *dpitch = -(((cur_y - *start_y) / row_h).round() as i32);
                *unsnap = event.modifiers.shift;
                let raw_pitch = (*grab_pitch as i32 + *dpitch).clamp(0, 127) as u8;
                audition_pitch = Some(pitch_ctx.constrain_pitch(raw_pitch));
            }
            // Switch the live audition note when the dragged pitch changes; a
            // horizontal-only (timing) move never retriggers.
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
        let ppb = self.ppb.max(0.0001);
        let snap_enabled = self.snap_on && !self.grid_res.is_free();
        let snap_step = self.step_beats();
        match &mut self.drag {
            PianoDrag::None => {}
            PianoDrag::Move { .. } => {}
            PianoDrag::Resize {
                start_x,
                prev_dur,
                anchor_start,
                delta_dur,
                new_dur,
                unsnap,
                ..
            } => {
                *unsnap = event.modifiers.shift;
                let cur_x: f32 = event.position.x.into();
                let raw_edge = *anchor_start + *prev_dur + (cur_x - *start_x) / ppb;
                let snapped_edge = if snap_enabled && !*unsnap {
                    snap_beat_to_step(raw_edge, snap_step)
                } else {
                    raw_edge.max(0.0)
                };
                let d = (snapped_edge - *anchor_start).max(MIN_NOTE_BEATS);
                *delta_dur = d - *prev_dur;
                *new_dur = d;
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
        let step = self.step_beats().max(MIN_NOTE_BEATS);
        let minimum = if self.snap_on && !self.grid_res.is_free() && !unsnap {
            step
        } else {
            MIN_NOTE_BEATS
        };
        let mut duration = (hi - lo).max(minimum);
        if self.snap_on && !self.grid_res.is_free() && !unsnap {
            duration = ((duration / step).ceil() * step).max(MIN_NOTE_BEATS);
        }
        // Do not clamp the note into the current clip length — a note drawn past
        // the clip end auto-expands the clip (see `CreateMidiNote::execute`).
        // `MidiNoteState::new` clamps start >= 0, pitch 0..=127, dur >= MIN.
        let mut note = MidiNoteState::new(pitch, lo, duration, DEFAULT_NOTE_VELOCITY);
        note.channel = channel;
        let id = note.id;
        self.run_edit_command(EditCommand::CreateMidiNote { clip_id, note }, cx);
        self.selection = HashSet::from([id]);
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
        if matches!(self.drag, PianoDrag::CcSelect { .. }) {
            let drag = std::mem::replace(&mut self.drag, PianoDrag::None);
            if let PianoDrag::CcSelect { mode, dragging, .. } = drag {
                if !dragging && mode == MarqueeSelectionMode::Replace {
                    self.cc_selection.clear();
                }
            }
            self.cc_selection_before_marquee.clear();
            cx.notify();
            return;
        }
        if matches!(
            self.drag,
            PianoDrag::CcPaint { .. } | PianoDrag::CcMove { .. } | PianoDrag::CcLine { .. }
        ) {
            self.drag = PianoDrag::None;
            self.drag_value_status = None;
            self.commit_cc_edit(cx);
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
                dx_beats,
                dpitch,
                anchor_start,
                clone_on_commit,
                unsnap,
                ..
            } => {
                if dx_beats.abs() < 0.0001 && dpitch == 0 {
                    return;
                }
                let pitch_ctx = self.pitch_ctx;
                let snapped_anchor = self.snap_beats_live(anchor_start + dx_beats, unsnap);
                let effective_delta = snapped_anchor - anchor_start;
                let updates: Vec<(u64, f32, u8)> = prev
                    .iter()
                    .map(|(id, start, pitch)| {
                        let new_start = (*start + effective_delta).max(0.0);
                        let raw_pitch = (*pitch as i32 + dpitch).clamp(0, 127) as u8;
                        let new_pitch = pitch_ctx.constrain_pitch(raw_pitch);
                        (*id, new_start, new_pitch)
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
                new_dur,
                prev_dur,
                prev_durs,
                delta_dur,
                ..
            } => {
                if (new_dur - prev_dur).abs() < 0.0001 {
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
                let prev: Vec<MidiNoteState> = next
                    .iter()
                    .map(|n| {
                        let mut p = n.clone();
                        if let Some((_, v)) = orig.iter().find(|(id, _)| *id == n.id) {
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
                let prev: Vec<MidiNoteState> = original_notes
                    .into_iter()
                    .filter(|note| ids.contains(&note.id))
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
                let prev: Vec<MidiNoteState> = original_notes
                    .into_iter()
                    .filter(|note| ids.contains(&note.id))
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
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let key = event.keystroke.key.as_str();
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let shift = event.keystroke.modifiers.shift;
        match key {
            "up" if !ctrl && !self.selection.is_empty() => {
                cx.stop_propagation();
                self.transpose_selection(if shift { 12 } else { 1 }, cx);
            }
            "down" if !ctrl && !self.selection.is_empty() => {
                cx.stop_propagation();
                self.transpose_selection(if shift { -12 } else { -1 }, cx);
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
                let all: Vec<u64> = self
                    .timeline
                    .read(cx)
                    .state
                    .midi_clip_notes(&clip_id)
                    .map(|notes| notes.iter().map(|n| n.id).collect())
                    .unwrap_or_default();
                self.selection = all.into_iter().collect();
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

    fn quantize_selection(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        // Empty selection means quantize every note in the clip.
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
        let step = self.quantize_res.beats();
        let target_ids = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.quantize_midi_notes(cid, &target_ids, step);
        });
    }

    /// Snap the selected notes' pitches to the nearest note in the active
    /// scale (independent of the live `constrain` toggle used for drag).
    /// No-op when nothing is selected or the active scale is Chromatic.
    fn snap_selection_to_scale(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.selection.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let scale = self.pitch_ctx.scale;
        let target_ids = ids.clone();
        self.commit_note_transform(cx, &ids, move |state, cid| {
            state.snap_midi_notes_to_scale(cid, &target_ids, scale);
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
            tl.state.set_track_midi_output_per_note(&track_id, next);
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
        // Per-note expression follows the parts: the pitch curve is cut at the
        // same beat and each half is re-based to its own note start, so a split
        // never silently discards a performance.
        let (left_curve, right_curve) = original
            .pitch_curve
            .as_ref()
            .map(|curve| curve.split_at(left_len))
            .unwrap_or_default();
        let mut left =
            MidiNoteState::new(original.pitch, original.start, left_len, original.velocity);
        left.muted = original.muted;
        left.channel = original.channel;
        left.articulation = original.articulation;
        left.pitch_curve = (!left_curve.is_empty()).then_some(left_curve);
        let mut right = MidiNoteState::new(original.pitch, cut, right_len, original.velocity);
        right.muted = original.muted;
        right.channel = original.channel;
        right.articulation = original.articulation;
        right.pitch_curve = (!right_curve.is_empty()).then_some(right_curve);
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
    /// span so the copies sit immediately after the originals. Selects the new
    /// notes. Does not use the clipboard.
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
        // Shift bypass keeps the exact source span; otherwise duplicates remain
        // grid-aligned through the same piano-roll snap helper used by movement.
        let raw_offset = (max_end - min_start).max(MIN_NOTE_BEATS);
        let offset = self
            .snap_beats_live(raw_offset, bypass_snap)
            .max(MIN_NOTE_BEATS);
        let notes: Vec<MidiNoteState> = src
            .iter()
            .map(|n| {
                let mut note =
                    MidiNoteState::new(n.pitch, n.start + offset, n.duration, n.velocity);
                note.muted = n.muted;
                note.channel = n.channel;
                note.articulation = n.articulation;
                note.pitch_curve = n.pitch_curve.as_ref().map(PitchCurve::cloned_with_new_ids);
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

    fn nudge_selected_start(&mut self, delta_beats: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
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
                        let new_start = (note.start + delta_beats).max(0.0);
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

    fn nudge_selected_length(&mut self, delta_beats: f32, cx: &mut Context<Self>) {
        let Some(clip_id) = self.editing_clip_id(cx) else {
            return;
        };
        let ids = self.selected_note_ids();
        if ids.is_empty() {
            return;
        }
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
                        let duration = (note.duration + delta_beats).max(MIN_NOTE_BEATS);
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
        let anchor_pitch_f = (PITCH_CNT as f32 - 1.0) - ((anchor_y + self.scroll_y) / old_row);
        self.row_h = DEFAULT_ROW_H;
        let new_row = self.note_row_h();
        self.scroll_y = (((PITCH_CNT as f32 - 1.0) - anchor_pitch_f) * new_row - anchor_y)
            .clamp(0.0, self.max_scroll_y());
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
        // Alt + wheel = Zoom Y (independent of Zoom X).
        if event.modifiers.alt && !(event.modifiers.control || event.modifiers.platform) {
            let (_, view_h) = self.grid_view_size();
            let anchor_y = self
                .grid_local(event.position)
                .map(|(_, ly)| ly)
                .unwrap_or(view_h * 0.5);
            let factor = (1.0022_f32).powf(dy);
            self.zoom_row_h_around(factor, anchor_y, cx);
            return;
        }
        if event.modifiers.control || event.modifiers.platform {
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
        if event.modifiers.shift {
            self.scroll_x = (self.scroll_x - dy - dx).clamp(0.0, max_scroll_x);
        } else {
            self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll_y());
            self.scroll_x = (self.scroll_x - dx).clamp(0.0, max_scroll_x);
        }
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

fn toolbar_group(label: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .px(px(3.0))
        .py(px(2.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::surface_panel_alt())
        .border(px(1.0))
        .border_color(Colors::divider())
        .child(
            div()
                .px(px(3.0))
                .text_size(px(8.0))
                .text_color(Colors::text_faint())
                .child(label),
        )
}

fn tool_btn(
    id: &'static str,
    label: &str,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(22.0))
        .min_w(px(24.0))
        .px(px(7.0))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .text_size(px(10.0))
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        // Active is accent-based (matching the Solfege tab chips and the Pitch
        // tab's tools); hover is a weaker graphite wash. Painting both with
        // `surface_hover` made a hovered tool indistinguishable from the
        // selected one.
        .bg(if active {
            Colors::accent_muted()
        } else {
            Colors::with_alpha(Colors::text_primary(), 0.0)
        })
        .border(px(1.0))
        .border_color(if active {
            Colors::accent_primary()
        } else {
            Colors::with_alpha(Colors::text_primary(), 0.0)
        })
        .when(!active, |s| s.hover(|s| s.bg(Colors::surface_hover())))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |ev, w, cx| on_click(ev, w, cx))
        .child(label.to_string())
}

fn note_inspector_label(label: &str) -> impl IntoElement {
    div()
        .text_size(px(9.0))
        .text_color(Colors::text_muted())
        .font_weight(gpui::FontWeight::BOLD)
        .child(label.to_string())
}

fn note_value_row(label: &str, value: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .min_h(px(22.0))
        .child(
            div()
                .text_size(px(9.0))
                .text_color(Colors::text_muted())
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(10.0))
                .text_color(Colors::text_primary())
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(value),
        )
}

fn note_button_row(children: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(4.0))
        .children(children)
}

fn note_action_button(
    id: &'static str,
    label: &str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(22.0))
        .min_w(px(54.0))
        .px(px(6.0))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .text_size(px(10.0))
        .text_color(Colors::text_secondary())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_raised())
        .hover(|s| s.bg(Colors::surface_hover()))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |ev, w, cx| on_click(ev, w, cx))
        .child(label.to_string())
}

/// A [`note_action_button`] that also shows whether its value is the one the
/// selection currently carries. Used by the articulation palette, where a rail
/// of identical buttons otherwise gave no indication of the current value.
fn note_toggle_button(
    id: &'static str,
    label: &str,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(22.0))
        .min_w(px(54.0))
        .px(px(6.0))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .text_size(px(10.0))
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .border(px(1.0))
        .border_color(if active {
            Colors::accent_primary()
        } else {
            Colors::border_subtle()
        })
        .bg(if active {
            Colors::accent_muted()
        } else {
            Colors::surface_raised()
        })
        .when(!active, |s| s.hover(|s| s.bg(Colors::surface_hover())))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |ev, w, cx| on_click(ev, w, cx))
        .child(label.to_string())
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

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct MidiEditorRenderSnapshot {
    pub clip_id: String,
    pub visible_beat_range: (f32, f32),
    pub visible_pitch_range: (u8, u8),
    pub notes: Vec<NoteRenderItem>,
    pub velocity: Vec<VelocityRenderItem>,
    pub controller_points: Vec<ControllerPointRenderItem>,
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
        for ppb in [4.0f32, 12.0, 24.0, 48.0, 96.0, 200.0, 400.0] {
            for grid in GridRes::ALL {
                let note_step = grid_step_beats(grid, ppb);
                let lane_step = viewport_snap_step(true, grid, ppb);
                if grid.is_free() {
                    assert_eq!(lane_step, 0.0, "{grid:?} @ {ppb}");
                } else {
                    assert_eq!(lane_step, note_step.max(MIN_NOTE_BEATS), "{grid:?} @ {ppb}");
                }
            }
        }
    }

    #[test]
    fn adaptive_lane_step_follows_zoom_not_the_fixed_table() {
        // `GridRes::Adaptive.fixed_beats()` is 0.0; using it published 1/32.
        assert_eq!(viewport_snap_step(true, GridRes::Adaptive, 20.0), 1.0);
        assert_eq!(viewport_snap_step(true, GridRes::Adaptive, 4.0), 4.0);
        assert_eq!(viewport_snap_step(true, GridRes::Adaptive, 300.0), 0.0625);
    }

    #[test]
    fn snapping_off_publishes_no_step() {
        for grid in GridRes::ALL {
            assert_eq!(viewport_snap_step(false, grid, 96.0), 0.0, "{grid:?}");
        }
        // Free never snaps even with the toggle on, matching `snap_beats`.
        assert_eq!(viewport_snap_step(true, GridRes::Free, 96.0), 0.0);
    }

    /// A published step of `0.0` must leave the beat untouched, so "snap off"
    /// on a lane means the same thing it means on the grid.
    #[test]
    fn a_zero_step_is_a_pass_through() {
        assert_eq!(snap_beat_to_step(1.234, 0.0), 1.234);
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
    fn resize_handle_never_covers_the_whole_note() {
        assert!(NOTE_RESIZE_MIN_W - RESIZE_ZONE >= RESIZE_ZONE);
    }
}
