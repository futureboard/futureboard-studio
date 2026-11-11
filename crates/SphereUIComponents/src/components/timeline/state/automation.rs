use super::*;

/// Two automation points within this many beats are treated as the same slot
/// (a second add at the same beat replaces the existing point's value).
pub const AUTOMATION_BEAT_EPSILON: f32 = 1.0e-3;

/// Vertical padding (px) kept at the top/bottom of an automation lane so the
/// extreme 0.0/1.0 values never sit exactly on the lane border.
pub const AUTOMATION_LANE_PAD: f32 = 8.0;

/// In-flight automation point drag (move gesture). Held by the Timeline while
/// the mouse is down; the point is mutated live and committed once on release.
#[derive(Debug, Clone)]
pub struct AutomationPointDrag {
    pub track_id: String,
    pub lane_id: String,
    pub point_id: u64,
    /// Set once the point has actually moved, so a pure click (select only)
    /// never marks the project dirty.
    pub moved: bool,
    /// Lane snapshot taken before the gesture, so the whole drag collapses into
    /// one undo entry rather than one per frame (or none at all).
    pub undo_before: Vec<AutomationLaneState>,
    /// Every point the drag carries, as `(id, beat, value)` at the press. A
    /// press on a point of a selection moves the whole selection; the pressed
    /// point is always among them.
    pub group: Vec<(u64, f32, f32)>,
    /// Cursor value at the press. The group moves by the value delta from
    /// here, so nothing jumps to the cursor when the drag starts.
    pub press_value: f32,
    /// The press landed on a point already in a larger selection. A click
    /// that never moves narrows the selection to that point; a drag keeps it.
    pub narrow_on_click: bool,
}

/// In-flight automation curve-tension drag. Shapes one segment by adjusting the
/// tension stored on its left point; the automation points themselves never
/// move. Started with Alt+drag on a segment line.
#[derive(Debug, Clone)]
pub struct AutomationCurveDrag {
    pub track_id: String,
    pub lane_id: String,
    /// Left point of the segment being shaped (tension lives on the left point).
    pub left_point_id: u64,
    /// Tension captured at drag start, so the gesture is relative.
    pub start_tension: f32,
    /// Normalized value captured at drag start, for the vertical-delta mapping.
    pub start_value: f32,
    /// Set once tension actually changed, so a pure click never dirties.
    pub changed: bool,
    /// Lane snapshot taken before the gesture. See [`AutomationPointDrag`].
    pub undo_before: Vec<AutomationLaneState>,
}

/// Transient hover state for an automation lane — which point or curve segment
/// the cursor is over. UI-only (never serialized); drives the per-segment
/// highlight and the hover cursor. Point hover takes priority over segment
/// hover, so at most one of `point_id` / `segment_left_id` is set.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationHover {
    pub track_id: String,
    pub lane_id: String,
    /// Hovered point id (wins over a segment when the cursor is near both).
    pub point_id: Option<u64>,
    /// Left point id of the hovered curve segment (tension lives on the left
    /// point). `None` when the cursor is over a point or empty lane.
    pub segment_left_id: Option<u64>,
    /// True while the segment is being actively dragged (stronger highlight).
    pub active: bool,
}

impl AutomationHover {
    /// Whether this hover targets `(track_id, lane_id)`.
    pub fn matches_lane(&self, track_id: &str, lane_id: &str) -> bool {
        self.track_id == track_id && self.lane_id == lane_id
    }
}

/// A freehand automation stroke in progress: the Pen tool dragged across a
/// lane draws the curve under the pointer.
#[derive(Debug, Clone)]
pub struct AutomationPaintStroke {
    pub track_id: String,
    pub lane_id: String,
    /// The track's lanes before the stroke, for its one undo entry.
    pub undo_before: Vec<AutomationLaneState>,
    /// The lane's points before the stroke. Each move rebuilds the lane from
    /// these plus everything drawn so far, so drawing back over a stretch
    /// replaces what the stroke itself put there.
    pub base: Vec<AutomationPoint>,
    /// `(beat, value)` in the order they were drawn.
    pub samples: Vec<(f32, f32)>,
}

/// A lane redrawn by a freehand stroke: the points of `base` outside the
/// span the stroke covers, plus the stroke itself.
///
/// Where the stroke doubled back over itself the latest pass wins. Samples
/// closer than `min_gap` beats merge, and a sample a straight line through
/// its neighbours already draws (within `value_tol`) is dropped, so a smooth
/// stroke is a few points rather than one per pixel.
pub fn paint_automation_stroke(
    base: &[AutomationPoint],
    samples: &[(f32, f32)],
    min_gap: f32,
    value_tol: f32,
) -> Vec<AutomationPoint> {
    if samples.is_empty() {
        return base.to_vec();
    }
    let lo = samples
        .iter()
        .map(|s| s.0.max(0.0))
        .fold(f32::INFINITY, f32::min);
    let hi = samples
        .iter()
        .map(|s| s.0.max(0.0))
        .fold(f32::NEG_INFINITY, f32::max);

    let mut drawn: Vec<(f32, f32, usize)> = samples
        .iter()
        .enumerate()
        .map(|(order, &(beat, value))| (beat.max(0.0), value.clamp(0.0, 1.0), order))
        .collect();
    drawn.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f32, f32, usize)> = Vec::with_capacity(drawn.len());
    for sample in drawn {
        match merged.last_mut() {
            Some(last) if sample.0 - last.0 < min_gap => {
                if sample.2 > last.2 {
                    last.1 = sample.1;
                    last.2 = sample.2;
                }
            }
            _ => merged.push(sample),
        }
    }

    let mut kept: Vec<(f32, f32)> = Vec::with_capacity(merged.len());
    for (i, &(beat, value, _)) in merged.iter().enumerate() {
        let (Some(&(prev_beat, prev_value)), Some(&(next_beat, next_value, _))) =
            (kept.last(), merged.get(i + 1))
        else {
            kept.push((beat, value));
            continue;
        };
        let span = next_beat - prev_beat;
        let t = if span > f32::EPSILON {
            (beat - prev_beat) / span
        } else {
            0.0
        };
        let on_line = prev_value + (next_value - prev_value) * t;
        if (on_line - value).abs() > value_tol {
            kept.push((beat, value));
        }
    }

    let mut out: Vec<AutomationPoint> = base
        .iter()
        .filter(|p| p.beat < lo - AUTOMATION_BEAT_EPSILON || p.beat > hi + AUTOMATION_BEAT_EPSILON)
        .cloned()
        .map(|mut p| {
            p.selected = false;
            p
        })
        .collect();
    out.extend(
        kept.into_iter()
            .map(|(beat, value)| AutomationPoint::new(beat, value)),
    );
    out.sort_by(|a, b| {
        a.beat
            .partial_cmp(&b.beat)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// In-flight automation marquee (rubber-band) selection in beat/value space.
#[derive(Debug, Clone)]
pub struct AutomationMarquee {
    pub track_id: String,
    pub lane_id: String,
    pub start_beat: f32,
    pub start_value: f32,
    pub cur_beat: f32,
    pub cur_value: f32,
    pub additive: bool,
    /// Whether the pointer has moved far enough to be a marquee. Until then
    /// the press is a click, and neither the selection nor the frame changes.
    pub started: bool,
    /// Points of the lane selected when the press began. An additive marquee
    /// shows these plus the rectangle — so a point the rectangle passes over
    /// and leaves again goes back to how it was — and Escape restores them.
    pub before: Vec<u64>,
}

/// Where a group of automation points goes when one of them is dragged.
///
/// `pressed` lands on `target_beat` (already snapped by the caller), and every
/// other point in `group` keeps its distance from it. Values move by
/// `value_delta`. Both deltas are clamped so the *group* stays inside the lane:
/// clamping each point on its own would flatten the shape against the edge
/// instead of stopping it there.
pub fn automation_group_moves(
    group: &[(u64, f32, f32)],
    pressed: u64,
    target_beat: f32,
    value_delta: f32,
) -> Vec<(u64, f32, f32)> {
    let Some(&(_, pressed_beat, _)) = group.iter().find(|(id, _, _)| *id == pressed) else {
        return Vec::new();
    };
    let min_beat = group
        .iter()
        .map(|&(_, b, _)| b)
        .fold(f32::INFINITY, f32::min);
    let min_value = group
        .iter()
        .map(|&(_, _, v)| v)
        .fold(f32::INFINITY, f32::min);
    let max_value = group
        .iter()
        .map(|&(_, _, v)| v)
        .fold(f32::NEG_INFINITY, f32::max);
    let beat_delta = (target_beat - pressed_beat).max(-min_beat);
    let value_delta = value_delta.clamp(-min_value, 1.0 - max_value);
    group
        .iter()
        .map(|&(id, beat, value)| (id, beat + beat_delta, value + value_delta))
        .collect()
}
/// Distance (px) the pointer must travel before an automation press becomes
/// a marquee, the same threshold the arrangement's own marquee uses.
pub const AUTOMATION_MARQUEE_THRESHOLD_PX: f32 = 4.0;

/// VST3 parameter the user last moved inside a plugin editor. UI-only runtime
/// state (not serialized) used by the automation control lane quick-action.
#[derive(Debug, Clone, PartialEq)]
pub struct LastTouchedPluginParam {
    pub track_id: String,
    pub insert_id: String,
    pub parameter_id: String,
    pub parameter_name: String,
    pub plugin_name: String,
    pub normalized_value: f32,
}

impl LastTouchedPluginParam {
    pub fn display_label(&self) -> String {
        format!("{} > {}", self.plugin_name, self.parameter_name)
    }

    pub fn automation_target(&self) -> AutomationTarget {
        AutomationTarget::PluginParameter {
            insert_id: self.insert_id.clone(),
            parameter_id: self.parameter_id.clone(),
            parameter_name: self.display_label(),
        }
    }
}

/// Display label for a plugin parameter automation lane / picker row.
pub fn plugin_automation_display_name(plugin_name: &str, param_title: &str) -> String {
    format!("{plugin_name} > {param_title}")
}

/// One parameter row in the automation target picker.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationPickerParameter {
    pub target: AutomationTarget,
    /// Parameter title only (for per-plugin search).
    pub param_title: String,
    pub already_added: bool,
}

/// One plugin group in the automation target picker.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationPickerPluginGroup {
    pub insert_id: String,
    pub plugin_name: String,
    pub parameters: Vec<AutomationPickerParameter>,
}

/// Structured picker model: instrument → effects → track controls.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AutomationPickerModel {
    pub last_touched: Option<LastTouchedPluginParam>,
    pub instrument: Option<AutomationPickerPluginGroup>,
    pub effects: Vec<AutomationPickerPluginGroup>,
    /// Track-level targets — volume, pan, mute, each send's level and on/off,
    /// each insert's bypass — with the label to show and whether a lane for it
    /// already exists.
    pub track_targets: Vec<(AutomationTarget, String, bool)>,
}

/// Interpolation shape between an automation point and the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationCurve {
    Linear,
    Hold,
    /// S-curve, bendable by tension: sharper (`> 0`) or softer (`< 0`). See
    /// `DirectAudio::automation_curve_factor`.
    Smooth,
}

impl Default for AutomationCurve {
    fn default() -> Self {
        AutomationCurve::Linear
    }
}

impl AutomationCurve {
    pub fn to_tag(self) -> u8 {
        match self {
            AutomationCurve::Linear => 0,
            AutomationCurve::Hold => 1,
            AutomationCurve::Smooth => 2,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => AutomationCurve::Hold,
            2 => AutomationCurve::Smooth,
            _ => AutomationCurve::Linear,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AutomationCurve::Linear => "Linear",
            AutomationCurve::Hold => "Hold",
            AutomationCurve::Smooth => "Smooth",
        }
    }
}

/// A segment shape the Curve menu offers: a curve kind and its tension. The
/// segment can still be bent further by dragging it; these are starting
/// points that cover the common moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationCurveShape {
    Linear,
    /// Ease in: starts slowly, arrives fast.
    Slow,
    /// Ease out: moves fast, settles slowly.
    Fast,
    SCurve,
    /// A steeper S that holds near both ends.
    SharpS,
    Hold,
}

/// Tension of the Slow, Fast and Sharp S presets.
const CURVE_PRESET_TENSION: f32 = 0.6;

impl AutomationCurveShape {
    pub const ALL: [Self; 6] = [
        Self::Linear,
        Self::Slow,
        Self::Fast,
        Self::SCurve,
        Self::SharpS,
        Self::Hold,
    ];

    /// The curve kind and tension a segment takes for this shape.
    pub fn curve_and_tension(self) -> (AutomationCurve, f32) {
        match self {
            Self::Linear => (AutomationCurve::Linear, 0.0),
            Self::Slow => (AutomationCurve::Linear, CURVE_PRESET_TENSION),
            Self::Fast => (AutomationCurve::Linear, -CURVE_PRESET_TENSION),
            Self::SCurve => (AutomationCurve::Smooth, 0.0),
            Self::SharpS => (AutomationCurve::Smooth, CURVE_PRESET_TENSION),
            Self::Hold => (AutomationCurve::Hold, 0.0),
        }
    }

    /// The shape a segment with curve and 	ension is, if it is one of
    /// these. Hold ignores tension.
    pub fn of(curve: AutomationCurve, tension: f32) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| {
            let (c, t) = shape.curve_and_tension();
            c == curve && (curve == AutomationCurve::Hold || (t - tension).abs() < 1.0e-3)
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Slow => "Slow (ease in)",
            Self::Fast => "Fast (ease out)",
            Self::SCurve => "S-Curve",
            Self::SharpS => "Sharp S",
            Self::Hold => "Hold (step)",
        }
    }

    /// The menu command that applies this shape to the selected points.
    pub fn command(self) -> &'static str {
        match self {
            Self::Linear => "automation:curve:linear",
            Self::Slow => "automation:curve:slow",
            Self::Fast => "automation:curve:fast",
            Self::SCurve => "automation:curve:smooth",
            Self::SharpS => "automation:curve:sharp-s",
            Self::Hold => "automation:curve:hold",
        }
    }

    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|shape| shape.command() == command)
    }
}

/// What a single automation lane controls. `TrackVolume`/`TrackPan` are wired
/// first; `PluginParameter`/`SendLevel` carry their descriptor so they can be
/// persisted and shown in the picker even before runtime application lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AutomationTarget {
    TrackVolume,
    TrackPan,
    TrackMute,
    PluginParameter {
        insert_id: String,
        parameter_id: String,
        parameter_name: String,
    },
    SendLevel {
        send_id: String,
    },
    /// A send's on/off switch.
    SendEnabled {
        send_id: String,
    },
    /// An insert's bypass.
    InsertBypass {
        insert_id: String,
    },
}

impl AutomationTarget {
    /// Short label shown on the lane header / target picker.
    pub fn display_name(&self) -> String {
        match self {
            AutomationTarget::TrackVolume => "Volume".to_string(),
            AutomationTarget::TrackPan => "Pan".to_string(),
            AutomationTarget::TrackMute => "Mute".to_string(),
            AutomationTarget::PluginParameter { parameter_name, .. } => parameter_name.clone(),
            AutomationTarget::SendLevel { .. } => "Send Level".to_string(),
            AutomationTarget::SendEnabled { .. } => "Send On/Off".to_string(),
            AutomationTarget::InsertBypass { .. } => "Bypass".to_string(),
        }
    }

    /// Value used for the automation line before the first point / when a lane
    /// has no points yet. Normalized 0.0..=1.0.
    pub fn default_value(&self) -> f32 {
        match self {
            AutomationTarget::TrackVolume => volume::db_to_norm(0.0),
            AutomationTarget::TrackPan => 0.5,
            AutomationTarget::TrackMute => 0.0,
            AutomationTarget::PluginParameter { .. } => 0.5,
            // The send's own unity, on the same fader law as the track's.
            AutomationTarget::SendLevel { .. } => volume::db_to_norm(0.0),
            AutomationTarget::SendEnabled { .. } => 1.0,
            AutomationTarget::InsertBypass { .. } => 0.0,
        }
    }

    /// Readout of a normalized value in the target's own unit, so a lane can
    /// say "-6.0 dB" or "L32" instead of "0.71". Targets whose engine mapping is
    /// not a fader law (plug-in parameters, send levels) read as a percentage —
    /// the one description of a normalized value that cannot be wrong.
    pub fn format_value(&self, norm: f32) -> String {
        let norm = norm.clamp(0.0, 1.0);
        match self {
            AutomationTarget::TrackVolume => format!("{} dB", volume::format_db(norm)),
            AutomationTarget::TrackPan => {
                crate::components::knob::format_pan_label(norm * 2.0 - 1.0)
            }
            AutomationTarget::TrackMute => if norm >= 0.5 { "Muted" } else { "Open" }.to_string(),
            // A send is a fader too: same law, same readout.
            AutomationTarget::SendLevel { .. } => format!("{} dB", volume::format_db(norm)),
            AutomationTarget::SendEnabled { .. } => {
                if norm >= 0.5 { "On" } else { "Off" }.to_string()
            }
            AutomationTarget::InsertBypass { .. } => {
                if norm >= 0.5 { "Bypassed" } else { "Active" }.to_string()
            }
            AutomationTarget::PluginParameter { .. } => format!("{:.0}%", norm * 100.0),
        }
    }

    /// Stable discriminant tag for binary persistence.
    pub fn to_tag(&self) -> u8 {
        match self {
            AutomationTarget::TrackVolume => 0,
            AutomationTarget::TrackPan => 1,
            AutomationTarget::TrackMute => 2,
            AutomationTarget::PluginParameter { .. } => 3,
            AutomationTarget::SendLevel { .. } => 4,
            AutomationTarget::SendEnabled { .. } => 5,
            AutomationTarget::InsertBypass { .. } => 6,
        }
    }

    /// Best-effort mapping from a legacy lane name (pre-target persistence)
    /// onto a concrete target so old projects keep working.
    pub fn from_legacy_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower.contains("pan") {
            AutomationTarget::TrackPan
        } else if lower.contains("mute") {
            AutomationTarget::TrackMute
        } else {
            AutomationTarget::TrackVolume
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutomationPoint {
    /// Transient identity (not serialized) — lets the lane editor track
    /// selection and in-flight drag targets across edits.
    pub id: u64,
    pub beat: f32,
    /// Normalized value in `0.0..=1.0`.
    pub value: f32,
    pub curve: AutomationCurve,
    /// Per-segment curve tension in `-1.0..=1.0` for the outgoing (rightward)
    /// segment of this point: `0` = straight, `> 0` eases in (exponential),
    /// `< 0` eases out (logarithmic). Adjusted by dragging the curve line.
    pub tension: f32,
    /// UI-only selection flag. Never serialized.
    pub selected: bool,
}

impl AutomationPoint {
    pub fn new(beat: f32, value: f32) -> Self {
        Self {
            id: next_automation_point_id(),
            beat: beat.max(0.0),
            value: value.clamp(0.0, 1.0),
            curve: AutomationCurve::Linear,
            tension: 0.0,
            selected: false,
        }
    }

    pub fn with_curve(beat: f32, value: f32, curve: AutomationCurve) -> Self {
        let mut p = Self::new(beat, value);
        p.curve = curve;
        p
    }

    /// Clamp a tension delta into the safe range and store it on this point's
    /// outgoing segment.
    pub fn set_tension(&mut self, tension: f32) {
        self.tension = tension.clamp(-1.0, 1.0);
    }
}

/// One copied automation point, relative to the first point copied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutomationClipboardPoint {
    pub offset: f32,
    pub value: f32,
    pub curve: AutomationCurve,
    pub tension: f32,
}

/// Points copied from an automation lane, for Paste.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationClipboard {
    pub points: Vec<AutomationClipboardPoint>,
    /// Beats from the first point to the last.
    pub span: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutomationLaneState {
    pub id: String,
    /// Display name. Mirrors `target.display_name()` for built-ins but kept as
    /// a field for back-compat with the persisted `parameter_name`.
    pub name: String,
    pub target: AutomationTarget,
    pub enabled: bool,
    /// Whether the dedicated expanded sub-lane is shown (separate from the
    /// in-track automation overlay shown by [`TrackLaneMode::Automation`]).
    pub visible: bool,
    pub points: Vec<AutomationPoint>,
    /// A pass is recording into this lane right now. UI-only, never saved:
    /// the engine holds the lane back meanwhile so the moving control is what
    /// is heard (see `engine_snapshot`).
    pub recording: bool,
    /// The user moved this lane's control by hand while it was not being
    /// recorded, so the control's own value plays instead of the lane until
    /// Re-Enable Automation. Session-only, never saved: a reopened project
    /// plays its automation.
    pub overridden: bool,
}

impl AutomationLaneState {
    /// Whether the lane drives its control: switched on, not overridden by
    /// hand, and not held back while it is being recorded. Lanes without
    /// points have nothing to play either; callers check that themselves.
    pub fn plays(&self) -> bool {
        self.enabled && !self.recording && !self.overridden
    }

    /// Build an empty lane for `target` with an auto-derived id/name.
    pub fn new(id: impl Into<String>, target: AutomationTarget) -> Self {
        Self {
            id: id.into(),
            name: target.display_name(),
            target,
            enabled: true,
            visible: false,
            points: Vec::new(),
            recording: false,
            overridden: false,
        }
    }

    /// Re-sort points by beat. Call after any add/move so evaluation and line
    /// rendering can assume ascending order.
    pub fn sort_points(&mut self) {
        self.points.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

// ── Automation coordinate + evaluation helpers ───────────────────────────────

/// Map a normalized automation value (`0.0..=1.0`) to a local y within a lane
/// of `lane_height` px. Top of the usable area is `value = 1.0`. Respects
/// [`AUTOMATION_LANE_PAD`] top/bottom so the extremes never hug the border.
pub fn automation_value_to_y(value: f32, lane_height: f32) -> f32 {
    let usable = (lane_height - 2.0 * AUTOMATION_LANE_PAD).max(1.0);
    AUTOMATION_LANE_PAD + (1.0 - value.clamp(0.0, 1.0)) * usable
}

/// Inverse of [`automation_value_to_y`]: local y in a lane back to a normalized
/// value (clamped to `0.0..=1.0`).
pub fn automation_y_to_value(y: f32, lane_height: f32) -> f32 {
    let usable = (lane_height - 2.0 * AUTOMATION_LANE_PAD).max(1.0);
    (1.0 - (y - AUTOMATION_LANE_PAD) / usable).clamp(0.0, 1.0)
}

/// Evaluate an automation curve at `beat`. `points` must be sorted ascending by
/// beat. With no points the `default` value is returned; before the first point
/// the first point's value is held; after the last point the last value is
/// held. Between points the leading point's curve/tension decides the shape via
/// the shared [`DirectAudio::automation_curve_factor`] — the exact same math the
/// realtime engine and offline exporter use, so the drawn curve, the heard
/// curve, and the bounced curve are identical.
pub fn evaluate_automation(points: &[AutomationPoint], beat: f64, default: f32) -> f32 {
    if points.is_empty() {
        return default;
    }
    let beat = beat as f32;
    if beat <= points[0].beat {
        return points[0].value;
    }
    let last = points.len() - 1;
    if beat >= points[last].beat {
        return points[last].value;
    }
    // Find the segment [a, b] containing `beat`.
    for i in 0..last {
        let a = &points[i];
        let b = &points[i + 1];
        if beat >= a.beat && beat <= b.beat {
            return automation_segment_value(a, b, beat);
        }
    }
    points[last].value
}

/// Value of the segment `a → b` at `beat` (which lies inside it). Shared by
/// [`evaluate_automation`] and [`AutomationSampler`] so both produce the same
/// bits.
#[inline]
fn automation_segment_value(a: &AutomationPoint, b: &AutomationPoint, beat: f32) -> f32 {
    let span = (b.beat - a.beat).max(1.0e-6);
    let t = ((beat - a.beat) / span).clamp(0.0, 1.0);
    let factor = DirectAudio::automation_curve_factor(a.curve.to_tag(), a.tension, t);
    a.value + (b.value - a.value) * factor
}

/// [`evaluate_automation`] for many beats in a row, such as the per-pixel
/// samples of a drawn lane.
///
/// `evaluate_automation` scans every segment, so sampling a lane per pixel is
/// O(width × points). This keeps a cursor on the segment of the previous beat
/// and walks forward from it, so ascending beats cost O(width + points) in
/// total; a beat that goes backwards re-seeks with a binary search. Results
/// are bit-identical to `evaluate_automation`, including the held ends and
/// the first-matching segment among points that share a beat. Points that are
/// not sorted (or carry a NaN beat) fall back to `evaluate_automation` itself,
/// because the cursor's segment choice is only equivalent on sorted input.
pub struct AutomationSampler<'a> {
    points: &'a [AutomationPoint],
    default: f32,
    sorted: bool,
    /// First index whose beat is `>= last_beat`.
    next: usize,
    last_beat: f32,
}

impl<'a> AutomationSampler<'a> {
    pub fn new(points: &'a [AutomationPoint], default: f32) -> Self {
        let sorted = points.iter().all(|p| !p.beat.is_nan())
            && points.windows(2).all(|w| w[0].beat <= w[1].beat);
        Self {
            points,
            default,
            sorted,
            next: 0,
            last_beat: f32::NEG_INFINITY,
        }
    }

    /// Same as `evaluate_automation(points, beat, default)`.
    pub fn value_at(&mut self, beat: f64) -> f32 {
        let points = self.points;
        if !self.sorted {
            return evaluate_automation(points, beat, self.default);
        }
        if points.is_empty() {
            return self.default;
        }
        let beat = beat as f32;
        if beat <= points[0].beat {
            return points[0].value;
        }
        let last = points.len() - 1;
        if beat >= points[last].beat {
            return points[last].value;
        }
        if beat.is_nan() {
            // `evaluate_automation` matches no segment and lands on its tail.
            return points[last].value;
        }
        // `points[0].beat < beat < points[last].beat` here. The scan in
        // `evaluate_automation` takes the first `i` with
        // `points[i].beat <= beat <= points[i + 1].beat`; on sorted points that
        // is the segment ending at the first point whose beat is `>= beat`.
        let mut j = if beat >= self.last_beat {
            self.next
        } else {
            points.partition_point(|p| p.beat < beat)
        };
        while points[j].beat < beat {
            j += 1;
        }
        self.next = j;
        self.last_beat = beat;
        automation_segment_value(&points[j - 1], &points[j], beat)
    }
}

impl TimelineState {
    // ── Automation: mode, target, lanes, points ──────────────────────────────
    // Single source of truth for automation edits. The TrackHeader toggle, the
    // lane editor, and keyboard commands all route through these. Selection and
    // mode toggles are UI-only (never dirty the engine); point add/move/delete
    // and target/lane changes are committed edits the caller marks dirty once.

    pub fn track_lane_mode(&self, track_id: &str) -> TrackLaneMode {
        if track_id == MASTER_TRACK_ID {
            // The master has no clips: its "lane mode" is whether its
            // automation block is shown.
            return if self.master.automation_expanded {
                TrackLaneMode::Automation
            } else {
                TrackLaneMode::Clips
            };
        }
        self.find_track(track_id)
            .map(|t| t.lane_mode)
            .unwrap_or(TrackLaneMode::Clips)
    }

    /// The automation lanes of `track_id`: a track's, or the master bus's for
    /// [`MASTER_TRACK_ID`].
    pub fn automation_lanes_of(&self, track_id: &str) -> Option<&[AutomationLaneState]> {
        if track_id == MASTER_TRACK_ID {
            return Some(&self.master.automation_lanes);
        }
        self.find_track(track_id)
            .map(|track| track.automation_lanes.as_slice())
    }

    /// The lanes and the selected target of `track_id`, mutably — the one place
    /// that knows the master bus keeps its own.
    pub(super) fn automation_host_mut(
        &mut self,
        track_id: &str,
    ) -> Option<(&mut Vec<AutomationLaneState>, &mut Option<AutomationTarget>)> {
        if track_id == MASTER_TRACK_ID {
            let master = &mut self.master;
            return Some((
                &mut master.automation_lanes,
                &mut master.selected_automation_target,
            ));
        }
        let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        Some((
            &mut track.automation_lanes,
            &mut track.selected_automation_target,
        ))
    }

    fn selected_automation_target_of(&self, track_id: &str) -> Option<AutomationTarget> {
        if track_id == MASTER_TRACK_ID {
            return self.master.selected_automation_target.clone();
        }
        self.find_track(track_id)?
            .selected_automation_target
            .clone()
    }

    /// Every master control that can be automated: its volume, then each
    /// loaded insert's bypass.
    pub fn master_automation_targets(&self) -> Vec<AutomationTarget> {
        let mut out = vec![AutomationTarget::TrackVolume];
        for insert in self
            .master
            .inserts
            .iter()
            .filter(|insert| !insert.is_empty())
        {
            out.push(AutomationTarget::InsertBypass {
                insert_id: insert.id.clone(),
            });
        }
        out
    }

    /// Whether any master lane is shown in the arrangement.
    pub fn master_automation_shown(&self) -> bool {
        self.master.automation_expanded
    }

    /// Toggle a track between Clip and Automation mode. Never reaches the
    /// engine, but the mode is saved with the project (v54), so callers mark it
    /// view-only dirty. Returns the new mode. Selecting Automation mode also
    /// makes sure a lane exists for the active target so the editor has
    /// something to draw.
    pub fn toggle_track_lane_mode(&mut self, track_id: &str) -> Option<TrackLaneMode> {
        if track_id == MASTER_TRACK_ID {
            self.master.automation_expanded = !self.master.automation_expanded;
            let mode = self.track_lane_mode(track_id);
            if mode == TrackLaneMode::Automation {
                let target = self.active_automation_target(track_id);
                let _ = self.ensure_automation_lane(track_id, target);
            }
            return Some(mode);
        }
        let new_mode = {
            let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
            track.lane_mode = match track.lane_mode {
                TrackLaneMode::Clips => TrackLaneMode::Automation,
                TrackLaneMode::Automation => TrackLaneMode::Clips,
            };
            track.lane_mode
        };
        if new_mode == TrackLaneMode::Automation {
            let target = self.active_automation_target(track_id);
            let _ = self.ensure_automation_lane(track_id, target);
        }
        if automation_debug_enabled() {
            eprintln!("[automation] mode track={} mode={:?}", track_id, new_mode);
        }
        Some(new_mode)
    }

    /// The target the lane editor is focused on for a track (selected target,
    /// else the first existing lane's target, else Track Volume).
    pub fn active_automation_target(&self, track_id: &str) -> AutomationTarget {
        if let Some(target) = self.selected_automation_target_of(track_id) {
            return target;
        }
        self.automation_lanes_of(track_id)
            .and_then(|lanes| lanes.first())
            .map(|l| l.target.clone())
            .unwrap_or(AutomationTarget::TrackVolume)
    }

    /// Every mixer control of `track` that can be automated, in channel-strip
    /// order: volume, pan, mute, then each send's level and on/off, then each
    /// loaded insert's bypass.
    pub fn mixer_automation_targets(track: &TrackState) -> Vec<AutomationTarget> {
        let mut out = vec![
            AutomationTarget::TrackVolume,
            AutomationTarget::TrackPan,
            AutomationTarget::TrackMute,
        ];
        for send in &track.sends {
            out.push(AutomationTarget::SendLevel {
                send_id: send.id.clone(),
            });
            out.push(AutomationTarget::SendEnabled {
                send_id: send.id.clone(),
            });
        }
        for insert in track.inserts.iter().filter(|insert| !insert.is_empty()) {
            out.push(AutomationTarget::InsertBypass {
                insert_id: insert.id.clone(),
            });
        }
        out
    }

    /// What a lane for `target` is called on `track`: a send by where it
    /// goes, a bypass by the plug-in it switches.
    pub fn automation_target_label_for(track: &TrackState, target: &AutomationTarget) -> String {
        let send_name = |send_id: &str| {
            track
                .sends
                .iter()
                .find(|send| send.id == send_id)
                .map(|send| send.target_name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Send".to_string())
        };
        match target {
            AutomationTarget::SendLevel { send_id } => {
                format!("Send \u{2192} {}", send_name(send_id))
            }
            AutomationTarget::SendEnabled { send_id } => {
                format!("Send \u{2192} {} On/Off", send_name(send_id))
            }
            AutomationTarget::InsertBypass { insert_id } => {
                let plugin = track
                    .inserts
                    .iter()
                    .find(|insert| insert.id == *insert_id)
                    .map(|insert| insert.display_name.clone())
                    .unwrap_or_else(|| "Insert".to_string());
                format!("{plugin} Bypass")
            }
            other => other.display_name(),
        }
    }

    /// [`Self::automation_target_label_for`] by track id.
    pub fn automation_target_label(&self, track_id: &str, target: &AutomationTarget) -> String {
        if track_id == MASTER_TRACK_ID {
            return match target {
                AutomationTarget::TrackVolume => "Master Volume".to_string(),
                AutomationTarget::InsertBypass { insert_id } => {
                    let plugin = self
                        .master
                        .inserts
                        .iter()
                        .find(|insert| insert.id == *insert_id)
                        .map(|insert| insert.display_name.clone())
                        .unwrap_or_else(|| "Insert".to_string());
                    format!("{plugin} Bypass")
                }
                other => other.display_name(),
            };
        }
        self.find_track(track_id)
            .map(|track| Self::automation_target_label_for(track, target))
            .unwrap_or_else(|| target.display_name())
    }

    /// Targets offered by the picker for a track: its mixer controls, then
    /// plugin parameters (when metadata is available from the plugin host).
    pub fn available_automation_targets(&self, track_id: &str) -> Vec<AutomationTarget> {
        if track_id == MASTER_TRACK_ID {
            let mut out = self.master_automation_targets();
            for insert in self
                .master
                .inserts
                .iter()
                .filter(|insert| !insert.is_empty())
            {
                for param in &insert.parameters {
                    if !Self::plugin_parameter_picker_visible(param) {
                        continue;
                    }
                    out.push(AutomationTarget::PluginParameter {
                        insert_id: insert.id.clone(),
                        parameter_id: param.id.to_string(),
                        parameter_name: plugin_automation_display_name(
                            &insert.display_name,
                            &param.name,
                        ),
                    });
                }
            }
            return out;
        }
        let mut out = self
            .find_track(track_id)
            .map(Self::mixer_automation_targets)
            .unwrap_or_else(|| {
                vec![
                    AutomationTarget::TrackVolume,
                    AutomationTarget::TrackPan,
                    AutomationTarget::TrackMute,
                ]
            });
        if let Some(track) = self.find_track(track_id) {
            for insert in &track.inserts {
                if insert.is_empty() {
                    continue;
                }
                for param in &insert.parameters {
                    if !Self::plugin_parameter_picker_visible(param) {
                        continue;
                    }
                    out.push(AutomationTarget::PluginParameter {
                        insert_id: insert.id.clone(),
                        parameter_id: param.id.to_string(),
                        parameter_name: plugin_automation_display_name(
                            &insert.display_name,
                            &param.name,
                        ),
                    });
                }
            }
        }
        out
    }

    fn plugin_parameter_picker_visible(param: &PluginParameterState) -> bool {
        if param.hidden && !automation_debug_enabled() {
            return false;
        }
        if !param.automatable && !automation_debug_enabled() {
            return false;
        }
        true
    }

    fn plugin_group_for_insert(
        &self,
        insert: &InsertSlotState,
        existing: &std::collections::HashSet<AutomationTarget>,
    ) -> Option<AutomationPickerPluginGroup> {
        if insert.is_empty() {
            return None;
        }
        let mut parameters = Vec::new();
        for param in &insert.parameters {
            if !Self::plugin_parameter_picker_visible(param) {
                continue;
            }
            let target = AutomationTarget::PluginParameter {
                insert_id: insert.id.clone(),
                parameter_id: param.id.to_string(),
                parameter_name: plugin_automation_display_name(&insert.display_name, &param.name),
            };
            parameters.push(AutomationPickerParameter {
                already_added: existing.contains(&target),
                param_title: param.name.clone(),
                target,
            });
        }
        Some(AutomationPickerPluginGroup {
            insert_id: insert.id.clone(),
            plugin_name: insert.display_name.clone(),
            parameters,
        })
    }

    /// Build grouped automation picker content for `track_id`.
    pub fn automation_picker_model(&self, track_id: &str) -> Option<AutomationPickerModel> {
        if track_id == MASTER_TRACK_ID {
            let existing: std::collections::HashSet<_> = self
                .master
                .automation_lanes
                .iter()
                .map(|lane| lane.target.clone())
                .collect();
            let track_targets = self
                .master_automation_targets()
                .into_iter()
                .map(|target| {
                    let label = self.automation_target_label(track_id, &target);
                    let added = existing.contains(&target);
                    (target, label, added)
                })
                .collect();
            let effects = self
                .master
                .inserts
                .iter()
                .filter_map(|insert| self.plugin_group_for_insert(insert, &existing))
                .collect();
            return Some(AutomationPickerModel {
                last_touched: self.last_touched_plugin_param_for_track(track_id).cloned(),
                instrument: None,
                effects,
                track_targets,
            });
        }
        let track = self.find_track(track_id)?;
        let existing: std::collections::HashSet<_> = track
            .automation_lanes
            .iter()
            .map(|lane| lane.target.clone())
            .collect();
        let track_targets = Self::mixer_automation_targets(track)
            .into_iter()
            .map(|target| {
                let label = Self::automation_target_label_for(track, &target);
                let added = existing.contains(&target);
                (target, label, added)
            })
            .collect();
        let instrument = track
            .instrument_insert()
            .and_then(|insert| self.plugin_group_for_insert(insert, &existing));
        let effects = track
            .effect_inserts()
            .iter()
            .filter_map(|insert| self.plugin_group_for_insert(insert, &existing))
            .collect();
        Some(AutomationPickerModel {
            last_touched: self.last_touched_plugin_param_for_track(track_id).cloned(),
            instrument,
            effects,
            track_targets,
        })
    }

    /// Point the lane editor at `target`, creating its lane if needed. Committed
    /// edit (changes which lane renders/persists). Returns the lane id.
    pub fn set_track_automation_target(
        &mut self,
        track_id: &str,
        target: AutomationTarget,
    ) -> Option<String> {
        if let Some((_, selected)) = self.automation_host_mut(track_id) {
            *selected = Some(target.clone());
        }
        if automation_debug_enabled() {
            eprintln!(
                "[automation] target track={} target={}",
                track_id,
                target.display_name()
            );
        }
        self.ensure_automation_lane(track_id, target)
    }

    /// Cycle to the next available target (Volume → Pan → plugin params → …).
    pub fn cycle_automation_target(&mut self, track_id: &str) -> Option<String> {
        let targets = self.available_automation_targets(track_id);
        if targets.is_empty() {
            return None;
        }
        let current = self.active_automation_target(track_id);
        let idx = targets.iter().position(|t| *t == current).unwrap_or(0);
        let next = targets[(idx + 1) % targets.len()].clone();
        self.set_track_automation_target(track_id, next)
    }

    /// Ensure a lane exists for `target` on `track_id`; returns its id.
    pub fn ensure_automation_lane(
        &mut self,
        track_id: &str,
        target: AutomationTarget,
    ) -> Option<String> {
        let name = self.automation_target_label(track_id, &target);
        let (lanes, _) = self.automation_host_mut(track_id)?;
        if let Some(lane) = lanes.iter_mut().find(|l| l.target == target) {
            // Asking for a target shows its lane: a hidden one comes back
            // rather than staying out of reach (a Bus or Return has no other
            // way to get its row back).
            lane.visible = true;
            return Some(lane.id.clone());
        }
        // Unique even after removals: the count alone could repeat the id of
        // a lane that is still there.
        let mut n = lanes.len() + 1;
        let mut lane_id = format!("autolane-{track_id}-{n}");
        while lanes.iter().any(|l| l.id == lane_id) {
            n += 1;
            lane_id = format!("autolane-{track_id}-{n}");
        }
        let mut lane = AutomationLaneState::new(lane_id.clone(), target);
        lane.name = name;
        // New lanes are shown as sub-rows immediately so creating a target (via
        // the picker / toggle) reveals its lane under the parent track.
        lane.visible = true;
        lanes.push(lane);
        if automation_debug_enabled() {
            eprintln!(
                "[automation] create_lane track={} lane={}",
                track_id, lane_id
            );
        }
        Some(lane_id)
    }

    /// Id of the lane the editor is currently focused on for a track.
    pub fn active_automation_lane_id(&self, track_id: &str) -> Option<String> {
        let lanes = self.automation_lanes_of(track_id)?;
        let target = self.active_automation_target(track_id);
        lanes
            .iter()
            .find(|l| l.target == target)
            .map(|l| l.id.clone())
    }

    pub(super) fn lane_mut(
        &mut self,
        track_id: &str,
        lane_id: &str,
    ) -> Option<&mut AutomationLaneState> {
        self.automation_host_mut(track_id)?
            .0
            .iter_mut()
            .find(|l| l.id == lane_id)
    }

    pub fn automation_lane(&self, track_id: &str, lane_id: &str) -> Option<&AutomationLaneState> {
        self.automation_lanes_of(track_id)?
            .iter()
            .find(|l| l.id == lane_id)
    }

    /// Add a point at `(beat, value)` to a lane. If a point already sits within
    /// [`AUTOMATION_BEAT_EPSILON`] beats, its value is replaced instead. Returns
    /// the affected point id. Committed edit — caller marks dirty once.
    pub fn add_automation_point(
        &mut self,
        track_id: &str,
        lane_id: &str,
        beat: f32,
        value: f32,
    ) -> Option<u64> {
        let lane = self.lane_mut(track_id, lane_id)?;
        let beat = beat.max(0.0);
        let value = value.clamp(0.0, 1.0);
        let id = if let Some(existing) = lane
            .points
            .iter_mut()
            .find(|p| (p.beat - beat).abs() <= AUTOMATION_BEAT_EPSILON)
        {
            existing.value = value;
            existing.id
        } else {
            let point = AutomationPoint::new(beat, value);
            let id = point.id;
            lane.points.push(point);
            id
        };
        lane.sort_points();
        if automation_debug_enabled() {
            eprintln!(
                "[automation] add_point lane={} beat={:.3} value={:.3}",
                lane_id, beat, value
            );
        }
        // Preview the edited curve at the playhead so the fader/inspector follow
        // a Track Volume point edit immediately (even while stopped).
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "point_edit");
        Some(id)
    }

    /// Replace a lane's points wholesale — what a freehand stroke produces on
    /// every move. Live edit; the caller records one undo entry on release.
    pub fn set_automation_lane_points(
        &mut self,
        track_id: &str,
        lane_id: &str,
        points: Vec<AutomationPoint>,
    ) -> bool {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return false;
        };
        lane.points = points;
        lane.sort_points();
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "point_edit");
        true
    }

    /// Move a point to a new beat/value (clamped + re-sorted). Committed on
    /// release by the caller.
    pub fn move_automation_point(
        &mut self,
        track_id: &str,
        lane_id: &str,
        point_id: u64,
        beat: f32,
        value: f32,
    ) {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return;
        };
        if let Some(p) = lane.points.iter_mut().find(|p| p.id == point_id) {
            p.beat = beat.max(0.0);
            p.value = value.clamp(0.0, 1.0);
        }
        lane.sort_points();
        if automation_debug_enabled() {
            eprintln!(
                "[automation] move_point lane={} id={} beat={:.3} value={:.3}",
                lane_id, point_id, beat, value
            );
        }
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "point_edit");
    }

    /// Set a point's curve type. Committed edit.
    pub fn set_automation_point_curve(
        &mut self,
        track_id: &str,
        lane_id: &str,
        point_id: u64,
        curve: AutomationCurve,
    ) {
        if let Some(lane) = self.lane_mut(track_id, lane_id) {
            if let Some(p) = lane.points.iter_mut().find(|p| p.id == point_id) {
                p.curve = curve;
            }
        }
    }

    /// Give every selected point of one lane `shape`'s curve and tension —
    /// the segment leaving each. Returns how many changed. Committed edit.
    pub fn set_selected_automation_shape(
        &mut self,
        track_id: &str,
        lane_id: &str,
        shape: AutomationCurveShape,
    ) -> usize {
        let (curve, tension) = shape.curve_and_tension();
        let mut changed = 0;
        if let Some(lane) = self.lane_mut(track_id, lane_id) {
            for point in lane.points.iter_mut().filter(|p| p.selected) {
                if point.curve != curve || (point.tension - tension).abs() > 1.0e-6 {
                    point.curve = curve;
                    point.set_tension(tension);
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            let playhead = self.transport.playhead_beats;
            self.recompute_effective_volumes(playhead, "point_curve");
        }
        changed
    }

    /// The shape every selected point of one lane shares, or `None` when they
    /// differ, one was bent by hand, or nothing is selected — what the Curve
    /// menu checks.
    pub fn selected_automation_shape(
        &self,
        track_id: &str,
        lane_id: &str,
    ) -> Option<AutomationCurveShape> {
        let lane = self.automation_lane(track_id, lane_id)?;
        let mut shapes = lane
            .points
            .iter()
            .filter(|p| p.selected)
            .map(|p| AutomationCurveShape::of(p.curve, p.tension));
        let first = shapes.next()??;
        shapes.all(|shape| shape == Some(first)).then_some(first)
    }

    /// The selected points of one lane, relative to the first, for a paste
    /// elsewhere. `None` when nothing is selected.
    pub fn copy_selected_automation(
        &self,
        track_id: &str,
        lane_id: &str,
    ) -> Option<AutomationClipboard> {
        let lane = self.automation_lane(track_id, lane_id)?;
        let selected: Vec<&AutomationPoint> = lane.points.iter().filter(|p| p.selected).collect();
        let start = selected
            .iter()
            .map(|p| p.beat)
            .fold(f32::INFINITY, f32::min);
        if !start.is_finite() {
            return None;
        }
        let points: Vec<AutomationClipboardPoint> = selected
            .iter()
            .map(|p| AutomationClipboardPoint {
                offset: p.beat - start,
                value: p.value,
                curve: p.curve,
                tension: p.tension,
            })
            .collect();
        let span = points.iter().map(|p| p.offset).fold(0.0, f32::max);
        Some(AutomationClipboard { points, span })
    }

    /// Paste `clipboard` into one lane starting at `at_beat`: the points it
    /// covers are replaced, the pasted ones come in selected. Returns how many
    /// were pasted. Committed edit.
    pub fn paste_automation(
        &mut self,
        track_id: &str,
        lane_id: &str,
        at_beat: f32,
        clipboard: &AutomationClipboard,
    ) -> usize {
        let at_beat = at_beat.max(0.0);
        let end = at_beat + clipboard.span;
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return 0;
        };
        for point in lane.points.iter_mut() {
            point.selected = false;
        }
        lane.points
            .retain(|p| p.beat < at_beat - 1e-4 || p.beat > end + 1e-4);
        for copied in &clipboard.points {
            let mut point =
                AutomationPoint::with_curve(at_beat + copied.offset, copied.value, copied.curve);
            point.tension = copied.tension;
            point.selected = true;
            lane.points.push(point);
        }
        lane.sort_points();
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "point_paste");
        clipboard.points.len()
    }

    /// Find the automation segment under `(beat, value)` and return its LEFT
    /// point id (tension lives on the left point). Matches only when the cursor
    /// is within `value_tol` of the *curve line* at `beat` and is bracketed by
    /// two points. Used to start an Alt curve-tension drag / reset. Pure query.
    pub fn automation_segment_left_point_at(
        &self,
        track_id: &str,
        lane_id: &str,
        beat: f32,
        value: f32,
        value_tol: f32,
    ) -> Option<u64> {
        let lane = self.automation_lane(track_id, lane_id)?;
        if lane.points.len() < 2 {
            return None;
        }
        // `beat` must sit strictly inside the authored range — outside it the
        // curve is a flat hold with no shapeable segment.
        if beat <= lane.points[0].beat || beat >= lane.points[lane.points.len() - 1].beat {
            return None;
        }
        let mut left: Option<&AutomationPoint> = None;
        for p in &lane.points {
            if p.beat <= beat {
                left = Some(p);
            } else {
                break;
            }
        }
        let a = left?;
        let curve_v = evaluate_automation(&lane.points, beat as f64, lane.target.default_value());
        ((curve_v - value).abs() <= value_tol).then_some(a.id)
    }

    /// Current tension of a segment's left point (captured at drag start).
    pub fn automation_segment_tension(
        &self,
        track_id: &str,
        lane_id: &str,
        left_point_id: u64,
    ) -> f32 {
        self.automation_lane(track_id, lane_id)
            .and_then(|l| l.points.iter().find(|p| p.id == left_point_id))
            .map(|p| p.tension)
            .unwrap_or(0.0)
    }

    /// Set a segment's tension (clamped to `-1.0..=1.0`). A Smooth segment
    /// stays an S and bends (sharper or softer); a Hold segment, which has no
    /// shape to bend, becomes a curved Linear one so the tension is
    /// visible and audible. Committed edit — the caller dirties once on
    /// release.
    pub fn set_automation_segment_tension(
        &mut self,
        track_id: &str,
        lane_id: &str,
        left_point_id: u64,
        tension: f32,
    ) {
        if let Some(lane) = self.lane_mut(track_id, lane_id) {
            if let Some(p) = lane.points.iter_mut().find(|p| p.id == left_point_id) {
                if p.curve != AutomationCurve::Smooth {
                    p.curve = AutomationCurve::Linear;
                }
                p.set_tension(tension);
            }
        }
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "curve_edit");
    }

    /// Reset a segment back to a straight line (Linear, tension 0). Committed
    /// edit. Returns whether anything changed (so a no-op double-click is free).
    pub fn reset_automation_segment_curve(
        &mut self,
        track_id: &str,
        lane_id: &str,
        left_point_id: u64,
    ) -> bool {
        let mut changed = false;
        if let Some(lane) = self.lane_mut(track_id, lane_id) {
            if let Some(p) = lane.points.iter_mut().find(|p| p.id == left_point_id) {
                if p.tension != 0.0 || p.curve != AutomationCurve::Linear {
                    p.curve = AutomationCurve::Linear;
                    p.tension = 0.0;
                    changed = true;
                }
            }
        }
        if changed {
            let playhead = self.transport.playhead_beats;
            self.recompute_effective_volumes(playhead, "curve_reset");
        }
        changed
    }

    /// Select a single point (or add to the selection when `additive`). UI-only.
    pub fn select_automation_point(
        &mut self,
        track_id: &str,
        lane_id: &str,
        point_id: u64,
        additive: bool,
    ) {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return;
        };
        for p in lane.points.iter_mut() {
            if p.id == point_id {
                p.selected = if additive { !p.selected } else { true };
            } else if !additive {
                p.selected = false;
            }
        }
    }

    /// Clear automation point selection on a track. UI-only. Returns true when
    /// anything was actually deselected.
    pub fn clear_automation_selection(&mut self, track_id: &str) -> bool {
        let Some((lanes, _)) = self.automation_host_mut(track_id) else {
            return false;
        };
        let mut changed = false;
        for lane in lanes.iter_mut() {
            for p in lane.points.iter_mut() {
                if p.selected {
                    p.selected = false;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Select every point in a lane. UI-only. Returns the count selected.
    pub fn select_all_automation_points(&mut self, track_id: &str, lane_id: &str) -> usize {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return 0;
        };
        for p in lane.points.iter_mut() {
            p.selected = true;
        }
        lane.points.len()
    }

    /// Marquee selection: the points inside a beat/value rectangle, plus
    /// `keep` (the selection an additive marquee started from). Everything
    /// else in the lane is deselected, so the result depends only on where the
    /// rectangle is now — not on where it has been. UI-only. Returns how many
    /// points the rectangle covers.
    pub fn marquee_select_automation(
        &mut self,
        track_id: &str,
        lane_id: &str,
        beat_lo: f32,
        beat_hi: f32,
        value_lo: f32,
        value_hi: f32,
        keep: &[u64],
    ) -> usize {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return 0;
        };
        let (b0, b1) = if beat_lo <= beat_hi {
            (beat_lo, beat_hi)
        } else {
            (beat_hi, beat_lo)
        };
        let (v0, v1) = if value_lo <= value_hi {
            (value_lo, value_hi)
        } else {
            (value_hi, value_lo)
        };
        let mut count = 0;
        for p in lane.points.iter_mut() {
            let inside = p.beat >= b0 && p.beat <= b1 && p.value >= v0 && p.value <= v1;
            if inside {
                count += 1;
            }
            p.selected = inside || keep.contains(&p.id);
        }
        count
    }

    /// Ids of the selected points in one lane.
    pub fn selected_automation_point_ids(&self, track_id: &str, lane_id: &str) -> Vec<u64> {
        self.automation_lane(track_id, lane_id)
            .map(|lane| {
                lane.points
                    .iter()
                    .filter(|p| p.selected)
                    .map(|p| p.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Selected points of one lane as `(id, beat, value)` — what a group drag
    /// carries.
    pub fn selected_automation_points(
        &self,
        track_id: &str,
        lane_id: &str,
    ) -> Vec<(u64, f32, f32)> {
        self.automation_lane(track_id, lane_id)
            .map(|lane| {
                lane.points
                    .iter()
                    .filter(|p| p.selected)
                    .map(|p| (p.id, p.beat, p.value))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Sets exactly `ids` selected in one lane. UI-only.
    pub fn set_automation_selection(&mut self, track_id: &str, lane_id: &str, ids: &[u64]) {
        if let Some(lane) = self.lane_mut(track_id, lane_id) {
            for p in lane.points.iter_mut() {
                p.selected = ids.contains(&p.id);
            }
        }
    }

    /// Moves several points of one lane at once: `(id, beat, value)` each,
    /// clamped, then one re-sort and one playback refresh for the lot.
    /// Committed on release by the caller.
    pub fn move_automation_points(
        &mut self,
        track_id: &str,
        lane_id: &str,
        moves: &[(u64, f32, f32)],
    ) {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return;
        };
        for p in lane.points.iter_mut() {
            if let Some(&(_, beat, value)) = moves.iter().find(|(id, _, _)| *id == p.id) {
                p.beat = beat.max(0.0);
                p.value = value.clamp(0.0, 1.0);
            }
        }
        lane.sort_points();
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "point_edit");
    }

    /// Find the closest automation point to `(beat, value)` within the given
    /// tolerances (in beats / normalized value). Returns its id. Used by the
    /// lane editor for click hit-testing.
    pub fn automation_point_at(
        &self,
        track_id: &str,
        lane_id: &str,
        beat: f32,
        value: f32,
        beat_tol: f32,
        value_tol: f32,
    ) -> Option<u64> {
        let lane = self.automation_lane(track_id, lane_id)?;
        let mut best: Option<(f32, u64)> = None;
        for p in &lane.points {
            let db = (p.beat - beat).abs();
            let dv = (p.value - value).abs();
            if db <= beat_tol && dv <= value_tol {
                // Rank by normalized combined distance so the nearest wins.
                let score = (db / beat_tol.max(1.0e-6)).hypot(dv / value_tol.max(1.0e-6));
                if best.map(|(s, _)| score < s).unwrap_or(true) {
                    best = Some((score, p.id));
                }
            }
        }
        best.map(|(_, id)| id)
    }

    pub fn selected_automation_point_count(&self, track_id: &str) -> usize {
        self.automation_lanes_of(track_id)
            .map(|lanes| {
                lanes
                    .iter()
                    .flat_map(|l| l.points.iter())
                    .filter(|p| p.selected)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Remove one automation point by id. Returns `true` when it was there.
    ///
    /// The single-point counterpart to
    /// [`Self::delete_selected_automation_points`], for the right-click delete:
    /// that gesture acts on what is under the cursor, which is not necessarily
    /// what is selected, and must not take the selection with it.
    pub fn delete_automation_point(
        &mut self,
        track_id: &str,
        lane_id: &str,
        point_id: u64,
    ) -> bool {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return false;
        };
        let before = lane.points.len();
        lane.points.retain(|point| point.id != point_id);
        let removed = before != lane.points.len();
        if removed {
            if automation_debug_enabled() {
                eprintln!(
                    "[automation] delete_point track={track_id} lane={lane_id} id={point_id}"
                );
            }
            // The curve under the playhead changed: the fader and its readout
            // follow it now, as they do for an added or moved point.
            let playhead = self.transport.playhead_beats;
            self.recompute_effective_volumes(playhead, "point_delete");
        }
        removed
    }

    /// Delete every selected automation point on a track, in every lane.
    /// Committed edit — caller marks dirty once. Returns how many were removed.
    pub fn delete_selected_automation_points(&mut self, track_id: &str) -> usize {
        let Some((lanes, _)) = self.automation_host_mut(track_id) else {
            return 0;
        };
        let mut removed = 0;
        for lane in lanes.iter_mut() {
            let before = lane.points.len();
            lane.points.retain(|p| !p.selected);
            removed += before - lane.points.len();
        }
        self.after_selected_points_deleted(track_id, removed);
        removed
    }

    /// Delete the selected points of one lane only — what Cut removes, since
    /// Cut copies one lane's selection ([`Self::copy_selected_automation`]) and
    /// must not take points from other lanes that never reach the clipboard.
    /// Committed edit. Returns how many were removed.
    pub fn delete_selected_automation_points_in_lane(
        &mut self,
        track_id: &str,
        lane_id: &str,
    ) -> usize {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return 0;
        };
        let before = lane.points.len();
        lane.points.retain(|p| !p.selected);
        let removed = before - lane.points.len();
        self.after_selected_points_deleted(track_id, removed);
        removed
    }

    fn after_selected_points_deleted(&mut self, track_id: &str, removed: usize) {
        if removed == 0 {
            return;
        }
        if automation_debug_enabled() {
            eprintln!(
                "[automation] delete_points track={} count={}",
                track_id, removed
            );
        }
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "point_delete");
    }

    // ── Automation sub-lane actions (lane header controls) ────────────────────

    /// Focus the editor on `lane_id` (sets the active automation target to that
    /// lane's target). UI-only; an expanded track's focus is saved with the
    /// project, see `Timeline::focus_automation_lane`. Called when a sub-lane
    /// header is clicked.
    pub fn activate_automation_lane(&mut self, track_id: &str, lane_id: &str) {
        let target = self
            .automation_lane(track_id, lane_id)
            .map(|l| l.target.clone());
        if let (Some(target), Some((_, selected))) = (target, self.automation_host_mut(track_id)) {
            *selected = Some(target);
        }
    }

    /// Switch a lane on or off. Returns whether it changed. Committed edit:
    /// an off lane keeps its points but never plays or records.
    pub fn set_automation_lane_enabled(
        &mut self,
        track_id: &str,
        lane_id: &str,
        enabled: bool,
    ) -> bool {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return false;
        };
        if lane.enabled == enabled {
            return false;
        }
        lane.enabled = enabled;
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "lane_enable_set");
        true
    }

    /// Whether the transport is recording automation right now: the arm is on
    /// and playback runs. A control moved meanwhile is written, not overridden.
    pub fn automation_writing(&self) -> bool {
        self.automation_arm && self.transport.playing
    }

    /// The user moved a control by hand: every lane of `track_id` whose
    /// target `matches` and that would play stops playing, and the control's
    /// own value is heard until [`Self::reenable_automation`]. Nothing is
    /// overridden while automation is being written — the recorder takes the
    /// move instead. Returns whether a lane was newly overridden; the engine
    /// then has to hear about it ([`Self::take_automation_override_change`]).
    pub fn override_automation(
        &mut self,
        track_id: &str,
        matches: impl Fn(&AutomationTarget) -> bool,
    ) -> bool {
        if self.automation_writing() {
            return false;
        }
        let lanes = if track_id == MASTER_TRACK_ID {
            &mut self.master.automation_lanes
        } else {
            match self.tracks.iter_mut().find(|t| t.id == track_id) {
                Some(track) => &mut track.automation_lanes,
                None => return false,
            }
        };
        let mut overridden = false;
        for lane in lanes.iter_mut() {
            if lane.plays() && !lane.points.is_empty() && matches(&lane.target) {
                lane.overridden = true;
                overridden = true;
            }
        }
        if overridden {
            self.automation_override_changed = true;
        }
        overridden
    }

    /// Whether any lane anywhere is overridden: what lights Re-Enable
    /// Automation.
    pub fn has_overridden_automation(&self) -> bool {
        self.tracks
            .iter()
            .flat_map(|t| t.automation_lanes.iter())
            .chain(self.master.automation_lanes.iter())
            .any(|lane| lane.overridden)
    }

    /// Re-Enable Automation: every overridden lane plays again. Returns
    /// whether anything changed. Not an undo step — overriding is not one
    /// either; both are about what plays, not what the project holds.
    pub fn reenable_automation(&mut self) -> bool {
        let mut changed = false;
        for lane in self
            .tracks
            .iter_mut()
            .flat_map(|t| t.automation_lanes.iter_mut())
            .chain(self.master.automation_lanes.iter_mut())
        {
            if lane.overridden {
                lane.overridden = false;
                changed = true;
            }
        }
        if changed {
            self.automation_override_changed = true;
            let playhead = self.transport.playhead_beats;
            self.recompute_effective_volumes(playhead, "reenable_automation");
        }
        changed
    }

    /// Re-enable one lane. Returns whether it was overridden.
    pub fn reenable_automation_lane(&mut self, track_id: &str, lane_id: &str) -> bool {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return false;
        };
        if !lane.overridden {
            return false;
        }
        lane.overridden = false;
        self.automation_override_changed = true;
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "reenable_automation_lane");
        true
    }

    /// Whether an override or re-enable is waiting for the engine, without
    /// taking it.
    pub fn automation_override_pending(&self) -> bool {
        self.automation_override_changed
    }

    /// Whether lanes were overridden or re-enabled since the last call: the
    /// engine's copy of the lanes has to be resent.
    pub fn take_automation_override_change(&mut self) -> bool {
        std::mem::take(&mut self.automation_override_changed)
    }

    /// Hide a lane's sub-row (does not delete it). UI-only.
    pub fn hide_automation_lane(&mut self, track_id: &str, lane_id: &str) -> bool {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return false;
        };
        if !lane.visible {
            return false;
        }
        lane.visible = false;
        true
    }

    /// Remove every point from a lane (keeps the lane). Committed edit. Returns
    /// how many points were removed.
    pub fn clear_automation_lane(&mut self, track_id: &str, lane_id: &str) -> usize {
        let Some(lane) = self.lane_mut(track_id, lane_id) else {
            return 0;
        };
        let removed = lane.points.len();
        lane.points.clear();
        if removed > 0 {
            let playhead = self.transport.playhead_beats;
            self.recompute_effective_volumes(playhead, "lane_clear");
        }
        removed
    }

    /// Remove one automation lane from a track. Committed edit. Returns true
    /// when the lane existed and was removed.
    pub fn remove_automation_lane(&mut self, track_id: &str, lane_id: &str) -> bool {
        let Some((lanes, selected)) = self.automation_host_mut(track_id) else {
            return false;
        };
        let before = lanes.len();
        lanes.retain(|l| l.id != lane_id);
        if lanes.len() == before {
            return false;
        }
        if selected
            .as_ref()
            .is_some_and(|t| !lanes.iter().any(|l| l.target == *t))
        {
            *selected = lanes.first().map(|l| l.target.clone());
        }
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "lane_remove");
        true
    }

    /// Remove every automation lane on a track. Committed edit. Returns how
    /// many lanes were removed.
    pub fn clear_all_automation_lanes(&mut self, track_id: &str) -> usize {
        let Some((lanes, selected)) = self.automation_host_mut(track_id) else {
            return 0;
        };
        let removed = lanes.len();
        if removed == 0 {
            return 0;
        }
        lanes.clear();
        *selected = None;
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "lanes_clear_all");
        removed
    }

    /// Every automation lane on `track_id`, cloned for an undo snapshot.
    ///
    /// Taken once at the start and once at the end of a gesture — never
    /// per-frame — so the clone cost is a gesture-rate concern, not a drag-rate
    /// one.
    pub fn capture_automation_lanes(&self, track_id: &str) -> Vec<AutomationLaneState> {
        self.automation_lanes_of(track_id)
            .map(<[AutomationLaneState]>::to_vec)
            .unwrap_or_default()
    }

    /// Replace a track's automation lanes wholesale. The undo/redo counterpart
    /// to [`Self::capture_automation_lanes`]; keeps the selected target valid
    /// and recomputes effective volumes so playback follows the restored curve.
    pub fn set_track_automation_lanes(
        &mut self,
        track_id: &str,
        mut lanes: Vec<AutomationLaneState>,
    ) {
        let Some((host_lanes, selected)) = self.automation_host_mut(track_id) else {
            return;
        };
        // Overriding is about what plays now, not what the project holds, so
        // undo and redo carry each lane's current override across instead of
        // the one captured with the edit.
        for lane in &mut lanes {
            lane.overridden = host_lanes
                .iter()
                .find(|current| current.id == lane.id)
                .is_some_and(|current| current.overridden);
        }
        *host_lanes = lanes;
        // A restored lane set may no longer contain the selected target (undoing
        // the creation of the lane that was selected), so re-point it rather
        // than leaving a selection that resolves to nothing.
        let selected_still_present = selected
            .as_ref()
            .is_some_and(|t| host_lanes.iter().any(|l| l.target == *t));
        if !selected_still_present {
            *selected = host_lanes.first().map(|l| l.target.clone());
        }
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "automation_lanes_restore");
    }

    /// Last touched VST3 parameter for `track_id`, if any.
    pub fn last_touched_plugin_param_for_track(
        &self,
        track_id: &str,
    ) -> Option<&LastTouchedPluginParam> {
        self.last_touched_plugin_param
            .as_ref()
            .filter(|p| p.track_id == track_id)
    }
}

/// Build a context-menu command id that selects `target` on `track_id`.
pub fn automation_target_menu_command(track_id: &str, target: &AutomationTarget) -> String {
    match target {
        AutomationTarget::TrackVolume => format!("automation:add-target:{track_id}:volume"),
        AutomationTarget::TrackPan => format!("automation:add-target:{track_id}:pan"),
        AutomationTarget::TrackMute => format!("automation:add-target:{track_id}:mute"),
        AutomationTarget::PluginParameter {
            insert_id,
            parameter_id,
            ..
        } => format!("automation:add-target:{track_id}:plugin:{insert_id}:{parameter_id}"),
        AutomationTarget::SendLevel { send_id } => {
            format!("automation:add-target:{track_id}:send:{send_id}")
        }
        AutomationTarget::SendEnabled { send_id } => {
            format!("automation:add-target:{track_id}:send-on:{send_id}")
        }
        AutomationTarget::InsertBypass { insert_id } => {
            format!("automation:add-target:{track_id}:bypass:{insert_id}")
        }
    }
}

/// Decode [`automation_target_menu_command`].
pub fn parse_automation_target_menu_command(command: &str) -> Option<(String, AutomationTarget)> {
    let rest = command.strip_prefix("automation:add-target:")?;
    let (track_id, kind) = rest.split_once(':')?;
    match kind {
        "volume" => Some((track_id.to_string(), AutomationTarget::TrackVolume)),
        "pan" => Some((track_id.to_string(), AutomationTarget::TrackPan)),
        "mute" => Some((track_id.to_string(), AutomationTarget::TrackMute)),
        _ if kind.starts_with("plugin:") => {
            let parts = kind.strip_prefix("plugin:")?;
            let (insert_id, parameter_id) = parts.split_once(':')?;
            let parameter_name = parameter_id.to_string();
            Some((
                track_id.to_string(),
                AutomationTarget::PluginParameter {
                    insert_id: insert_id.to_string(),
                    parameter_id: parameter_id.to_string(),
                    parameter_name,
                },
            ))
        }
        _ if kind.starts_with("send-on:") => Some((
            track_id.to_string(),
            AutomationTarget::SendEnabled {
                send_id: kind.strip_prefix("send-on:")?.to_string(),
            },
        )),
        _ if kind.starts_with("bypass:") => Some((
            track_id.to_string(),
            AutomationTarget::InsertBypass {
                insert_id: kind.strip_prefix("bypass:")?.to_string(),
            },
        )),
        _ if kind.starts_with("send:") => {
            let send_id = kind.strip_prefix("send:")?;
            Some((
                track_id.to_string(),
                AutomationTarget::SendLevel {
                    send_id: send_id.to_string(),
                },
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{evaluate_automation, AutomationCurve, AutomationPoint};

    const EPS: f32 = 1e-5;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    #[test]
    fn empty_lane_returns_default() {
        assert!(approx(evaluate_automation(&[], 3.0, 0.42), 0.42));
    }

    #[test]
    fn before_first_and_after_last_hold_endpoints() {
        let pts = [
            AutomationPoint::new(2.0, 0.2),
            AutomationPoint::new(6.0, 0.8),
        ];
        // Before the first point: hold the first value (not the default).
        assert!(approx(evaluate_automation(&pts, 0.0, 0.0), 0.2));
        // Exactly on the first point.
        assert!(approx(evaluate_automation(&pts, 2.0, 0.0), 0.2));
        // Exactly on / after the last point: hold the last value.
        assert!(approx(evaluate_automation(&pts, 6.0, 0.0), 0.8));
        assert!(approx(evaluate_automation(&pts, 99.0, 0.0), 0.8));
    }

    #[test]
    fn linear_segment_interpolates() {
        // Linear/tension-0 factor is identity, so the midpoint is the average.
        let pts = [
            AutomationPoint::new(0.0, 0.2),
            AutomationPoint::new(4.0, 0.6),
        ];
        assert!(approx(evaluate_automation(&pts, 2.0, 0.0), 0.4));
        assert!(approx(evaluate_automation(&pts, 1.0, 0.0), 0.3));
    }

    #[test]
    fn hold_curve_steps_at_the_next_point() {
        // Hold keeps the left value across the whole segment; the step lands at
        // the next point (here the last, so after-last holds the right value).
        let pts = [
            AutomationPoint::with_curve(0.0, 0.2, AutomationCurve::Hold),
            AutomationPoint::new(4.0, 0.8),
        ];
        assert!(approx(evaluate_automation(&pts, 1.0, 0.0), 0.2));
        assert!(approx(evaluate_automation(&pts, 3.999, 0.0), 0.2));
        assert!(approx(evaluate_automation(&pts, 4.0, 0.0), 0.8));
    }

    #[test]
    fn selects_the_correct_segment_of_three_points() {
        let pts = [
            AutomationPoint::new(0.0, 0.0),
            AutomationPoint::new(4.0, 1.0),
            AutomationPoint::new(8.0, 0.5),
        ];
        // First segment [0,4] midpoint.
        assert!(approx(evaluate_automation(&pts, 2.0, 0.0), 0.5));
        // Second segment [4,8] midpoint: 1.0 + (0.5-1.0)*0.5 = 0.75.
        assert!(approx(evaluate_automation(&pts, 6.0, 0.0), 0.75));
    }

    fn point(beat: f32, value: f32, curve: AutomationCurve, tension: f32) -> AutomationPoint {
        let mut p = AutomationPoint::with_curve(beat, value, curve);
        p.set_tension(tension);
        p
    }

    /// Lanes covering every curve shape, tension, step points, shared beats
    /// and unsorted input.
    fn sampler_lanes() -> Vec<Vec<AutomationPoint>> {
        use AutomationCurve::{Hold, Linear, Smooth};
        let mut lanes = vec![
            Vec::new(),
            vec![point(3.0, 0.4, Linear, 0.0)],
            vec![point(0.0, 0.2, Linear, 0.0), point(4.0, 0.6, Linear, 0.0)],
            vec![
                point(0.0, 0.1, Linear, 0.7),
                point(1.5, 0.9, Hold, 0.0),
                point(3.0, 0.3, Smooth, -0.6),
                point(4.25, 0.8, Linear, -1.0),
                point(6.0, 0.0, Hold, 0.4),
                point(8.0, 1.0, Linear, 1.0),
                point(9.0, 0.5, Linear, 0.0),
            ],
            // Step points: two points on one beat jump the value there.
            vec![
                point(1.0, 0.2, Linear, 0.3),
                point(2.0, 0.2, Linear, 0.0),
                point(2.0, 0.9, Hold, 0.0),
                point(2.0, 0.5, Linear, -0.5),
                point(5.0, 0.1, Smooth, 0.2),
                point(5.0, 0.7, Linear, 0.0),
                point(7.0, 0.7, Linear, 0.0),
                point(7.0, 0.0, Linear, 0.0),
            ],
            // Shared beats at both ends.
            vec![
                point(2.0, 0.3, Linear, 0.0),
                point(2.0, 0.6, Linear, 0.5),
                point(4.0, 0.9, Linear, 0.0),
                point(4.0, 0.1, Linear, 0.0),
            ],
            // Unsorted: the sampler must fall back, not pick another segment.
            vec![
                point(4.0, 0.9, Linear, 0.0),
                point(1.0, 0.2, Hold, 0.0),
                point(6.0, 0.4, Smooth, 0.3),
                point(3.0, 0.7, Linear, -0.2),
            ],
        ];
        // A long, dense lane with pseudo-random shapes (deterministic LCG).
        let mut seed: u32 = 0x1234_5678;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let mut beat = 0.0f32;
        let mut dense = Vec::new();
        for _ in 0..200 {
            // Some steps land on the previous beat to make duplicates.
            if next() > 0.15 {
                beat += next() * 0.75;
            }
            let curve = match (next() * 3.0) as u32 {
                0 => Linear,
                1 => Hold,
                _ => Smooth,
            };
            dense.push(point(beat, next(), curve, next() * 2.0 - 1.0));
        }
        lanes.push(dense);
        lanes
    }

    /// Sample positions: dense ascending, every point's exact beat (and its
    /// neighbouring floats), then backwards and jumping around to exercise the
    /// sampler's re-seek.
    fn sampler_beats(points: &[AutomationPoint]) -> Vec<f64> {
        let end = points.iter().map(|p| p.beat).fold(10.0f32, f32::max) + 2.0;
        let mut beats: Vec<f64> = (0..=4000)
            .map(|i| -1.0 + i as f64 * (end as f64 + 1.0) / 4000.0)
            .collect();
        for p in points {
            let b = p.beat;
            beats.push(f32::from_bits(b.to_bits().saturating_sub(1)) as f64);
            beats.push(b as f64);
            beats.push(f32::from_bits(b.to_bits() + 1) as f64);
        }
        let ascending = beats.clone();
        beats.extend(ascending.iter().rev());
        beats.extend(ascending.iter().step_by(7).rev().step_by(3));
        beats.extend([f64::NAN, 2.0, f64::INFINITY, f64::NEG_INFINITY, 2.0]);
        beats
    }

    #[test]
    fn sampler_matches_evaluate_automation_bit_for_bit() {
        for (lane_index, points) in sampler_lanes().iter().enumerate() {
            for default in [0.0f32, 0.5, 0.8] {
                // Ascending only (the render path), then everything in order.
                let mut ascending = super::AutomationSampler::new(points, default);
                let mut beats = sampler_beats(points);
                let mut sorted = beats.clone();
                sorted.retain(|b| !b.is_nan());
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                for &beat in &sorted {
                    let want = evaluate_automation(points, beat, default);
                    let got = ascending.value_at(beat);
                    assert_eq!(
                        got.to_bits(),
                        want.to_bits(),
                        "lane {lane_index} ascending beat {beat}: {got} != {want}"
                    );
                }
                let mut mixed = super::AutomationSampler::new(points, default);
                for beat in beats.drain(..) {
                    let want = evaluate_automation(points, beat, default);
                    let got = mixed.value_at(beat);
                    assert_eq!(
                        got.to_bits(),
                        want.to_bits(),
                        "lane {lane_index} mixed beat {beat}: {got} != {want}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod marquee_and_group_tests {
    use super::*;

    #[test]
    fn a_group_keeps_its_shape_and_stays_in_the_lane() {
        let group = [(1, 4.0, 0.2), (2, 6.0, 0.8)];
        // Point 1 dragged to beat 5, up by 0.5: point 2 would go past 1.0, so
        // the whole group stops where point 2 meets the top.
        let close = |got: Vec<(u64, f32, f32)>, want: &[(u64, f32, f32)]| {
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(want) {
                assert_eq!(g.0, w.0);
                assert!(
                    (g.1 - w.1).abs() < 1e-5 && (g.2 - w.2).abs() < 1e-5,
                    "{g:?} vs {w:?}"
                );
            }
        };
        close(
            automation_group_moves(&group, 1, 5.0, 0.5),
            &[(1, 5.0, 0.4), (2, 7.0, 1.0)],
        );
        // Dragged before the start of the song: the earliest point stops at 0.
        close(
            automation_group_moves(&group, 2, 1.0, 0.0),
            &[(1, 0.0, 0.2), (2, 2.0, 0.8)],
        );
    }

    #[test]
    fn a_point_outside_the_group_moves_nothing() {
        assert!(automation_group_moves(&[(1, 0.0, 0.5)], 9, 2.0, 0.1).is_empty());
    }
}

#[cfg(test)]
mod paint_stroke_tests {
    use super::*;

    fn beats(points: &[AutomationPoint]) -> Vec<f32> {
        points.iter().map(|p| p.beat).collect()
    }

    /// A stroke replaces what it passes over and leaves the rest alone.
    #[test]
    fn a_stroke_replaces_the_points_under_it_only() {
        let base = vec![
            AutomationPoint::new(0.0, 0.2),
            AutomationPoint::new(2.0, 0.9),
            AutomationPoint::new(6.0, 0.4),
        ];
        let samples: Vec<(f32, f32)> = (0..=40).map(|i| (1.0 + i as f32 * 0.1, 0.5)).collect();
        let out = paint_automation_stroke(&base, &samples, 0.01, 0.001);
        assert_eq!(out.first().unwrap().beat, 0.0, "before the stroke survives");
        assert_eq!(out.last().unwrap().beat, 6.0, "after the stroke survives");
        assert!(!beats(&out).contains(&2.0), "the point under it is gone");
        // A flat line is two points, not forty.
        let drawn: Vec<&AutomationPoint> = out
            .iter()
            .filter(|p| p.beat >= 1.0 && p.beat <= 5.0)
            .collect();
        assert_eq!(drawn.len(), 2, "{:?}", beats(&out));
    }

    /// A curve keeps enough points to draw it, sorted by time.
    #[test]
    fn a_curved_stroke_keeps_its_shape() {
        let samples: Vec<(f32, f32)> = (0..=100)
            .map(|i| {
                let beat = i as f32 * 0.04;
                (beat, 0.5 + 0.4 * (beat * std::f32::consts::PI).sin())
            })
            .collect();
        let out = paint_automation_stroke(&[], &samples, 0.01, 0.01);
        assert!(out.len() > 6, "a sine keeps its turns: {}", out.len());
        assert!(out.windows(2).all(|w| w[0].beat <= w[1].beat));
        for &(beat, value) in &samples {
            let drawn = evaluate_automation(&out, beat as f64, 0.0);
            assert!(
                (drawn - value).abs() < 0.03,
                "at {beat}: drew {drawn}, wanted {value}"
            );
        }
    }

    /// Drawing back over a stretch: the latest pass is what stays.
    #[test]
    fn going_back_over_a_stretch_keeps_the_latest_pass() {
        let mut samples: Vec<(f32, f32)> = (0..=20).map(|i| (i as f32 * 0.1, 0.2)).collect();
        samples.extend((0..=10).rev().map(|i| (i as f32 * 0.1, 0.8)));
        let out = paint_automation_stroke(&[], &samples, 0.05, 0.001);
        assert!((evaluate_automation(&out, 0.5, 0.0) - 0.8).abs() < 1e-3);
        assert!((evaluate_automation(&out, 1.9, 0.0) - 0.2).abs() < 1e-3);
    }

    /// A click is one point.
    #[test]
    fn a_click_is_one_point() {
        let out = paint_automation_stroke(&[], &[(1.5, 0.7)], 0.01, 0.001);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].beat, out[0].value), (1.5, 0.7));
    }
}

#[cfg(test)]
mod mixer_target_tests {
    use super::*;

    fn bus_send_track() -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Vox".to_string(),
            color: gpui::rgb(0x336699),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let track = state.tracks.iter_mut().find(|t| t.id == id).unwrap();
        track.sends.push(SendSlotState {
            id: "send-1".to_string(),
            target_track_id: "ret-1".to_string(),
            target_name: "Reverb".to_string(),
            enabled: true,
            pre_fader: false,
            gain_db: 0.0,
            wdw: false,
            automated: None,
        });
        state
    }

    /// In Read, a send shows what its lane plays at the playhead; Off, or
    /// while the lane is being recorded, it shows its own setting.
    #[test]
    fn a_reading_send_lane_moves_the_send_control() {
        let mut state = bus_send_track();
        let track_id = state.tracks[0].id.clone();
        let lane = state
            .ensure_automation_lane(
                &track_id,
                AutomationTarget::SendLevel {
                    send_id: "send-1".to_string(),
                },
            )
            .unwrap();
        state.add_automation_point(&track_id, &lane, 0.0, volume::db_to_norm(-12.0));
        // A point edit refreshes it at once, not only the next playback tick.
        state.recompute_effective_volumes(2.0, "playback_tick");
        let send = &state.tracks[0].sends[0];
        assert!((send.display_gain_db() - (-12.0)).abs() < 0.05);
        assert_eq!(send.gain_db, 0.0, "the send's own level is untouched");

        state.lane_mut(&track_id, &lane).unwrap().recording = true;
        state.recompute_effective_volumes(2.0, "playback_tick");
        assert_eq!(state.tracks[0].sends[0].automated, None);

        state.lane_mut(&track_id, &lane).unwrap().recording = false;
        state.set_automation_lane_enabled(&track_id, &lane, false);
        state.recompute_effective_volumes(2.0, "playback_tick");
        assert_eq!(state.tracks[0].sends[0].display_gain_db(), 0.0);
    }

    /// Every send offers its level and its on/off, named by where it goes.
    #[test]
    fn sends_are_automatable_and_named_by_their_return() {
        let state = bus_send_track();
        let track = &state.tracks[0];
        let targets = TimelineState::mixer_automation_targets(track);
        let level = AutomationTarget::SendLevel {
            send_id: "send-1".to_string(),
        };
        let on = AutomationTarget::SendEnabled {
            send_id: "send-1".to_string(),
        };
        assert!(targets.contains(&level) && targets.contains(&on));
        assert_eq!(
            TimelineState::automation_target_label_for(track, &level),
            "Send \u{2192} Reverb"
        );
        assert!(
            level.format_value(volume::db_to_norm(0.0)).ends_with(" dB"),
            "a send reads in dB"
        );
        assert_eq!(on.format_value(1.0), "On");
        assert_eq!(on.format_value(0.0), "Off");
        assert_eq!(
            AutomationTarget::InsertBypass {
                insert_id: "x".to_string()
            }
            .format_value(1.0),
            "Bypassed"
        );
    }

    #[test]
    fn new_targets_survive_their_menu_commands() {
        for target in [
            AutomationTarget::SendEnabled {
                send_id: "send-1".to_string(),
            },
            AutomationTarget::InsertBypass {
                insert_id: "insert-2".to_string(),
            },
            AutomationTarget::SendLevel {
                send_id: "send-1".to_string(),
            },
        ] {
            let command = automation_target_menu_command("track-1", &target);
            assert_eq!(
                parse_automation_target_menu_command(&command),
                Some(("track-1".to_string(), target))
            );
        }
    }

    /// A Bus or Return shows in the arrangement only while it carries a shown
    /// lane.
    #[test]
    fn a_bus_row_appears_with_its_automation() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Bus,
            name: "Drums Bus".to_string(),
            color: gpui::rgb(0x336699),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        assert!(is_arrangement_hidden_track(state.find_track(&id).unwrap()));
        state.ensure_automation_lane(&id, AutomationTarget::TrackVolume);
        assert!(!is_arrangement_hidden_track(state.find_track(&id).unwrap()));
    }
}

#[cfg(test)]
mod point_tool_tests {
    use super::*;

    fn lane_with_points(beats: &[f32]) -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let lane = state
            .ensure_automation_lane(&track, AutomationTarget::TrackVolume)
            .unwrap();
        for (i, beat) in beats.iter().enumerate() {
            state.add_automation_point(&track, &lane, *beat, 0.1 * (i + 1) as f32);
        }
        (state, track, lane)
    }

    #[test]
    fn the_curve_menu_sets_only_the_selected_points() {
        let (mut state, track, lane) = lane_with_points(&[0.0, 4.0, 8.0]);
        let ids: Vec<u64> = state
            .automation_lane(&track, &lane)
            .unwrap()
            .points
            .iter()
            .map(|p| p.id)
            .collect();
        state.set_automation_selection(&track, &lane, &ids[..2]);
        assert_eq!(
            state.selected_automation_shape(&track, &lane),
            Some(AutomationCurveShape::Linear)
        );
        assert_eq!(
            state.set_selected_automation_shape(&track, &lane, AutomationCurveShape::Hold),
            2
        );
        let curves: Vec<AutomationCurve> = state
            .automation_lane(&track, &lane)
            .unwrap()
            .points
            .iter()
            .map(|p| p.curve)
            .collect();
        assert_eq!(
            curves,
            vec![
                AutomationCurve::Hold,
                AutomationCurve::Hold,
                AutomationCurve::Linear
            ]
        );
        assert_eq!(
            state.selected_automation_shape(&track, &lane),
            Some(AutomationCurveShape::Hold)
        );
    }

    /// A preset sets curve and tension together and is recognised again; a
    /// segment bent by hand is no preset. Bending a Smooth segment keeps it
    /// an S; bending a Hold one makes it a curved line.
    #[test]
    fn curve_presets_and_bending() {
        let (mut state, track, lane) = lane_with_points(&[0.0, 4.0, 8.0]);
        let ids: Vec<u64> = state
            .automation_lane(&track, &lane)
            .unwrap()
            .points
            .iter()
            .map(|p| p.id)
            .collect();
        state.set_automation_selection(&track, &lane, &ids[..1]);
        assert_eq!(
            state.set_selected_automation_shape(&track, &lane, AutomationCurveShape::SharpS),
            1
        );
        let first = &state.automation_lane(&track, &lane).unwrap().points[0];
        assert_eq!(first.curve, AutomationCurve::Smooth);
        assert!(first.tension > 0.0);
        assert_eq!(
            state.selected_automation_shape(&track, &lane),
            Some(AutomationCurveShape::SharpS)
        );

        state.set_automation_segment_tension(&track, &lane, ids[0], -0.35);
        let first = &state.automation_lane(&track, &lane).unwrap().points[0];
        assert_eq!(first.curve, AutomationCurve::Smooth, "still an S");
        assert_eq!(first.tension, -0.35);
        assert_eq!(state.selected_automation_shape(&track, &lane), None);

        state.set_automation_selection(&track, &lane, &ids[1..2]);
        state.set_selected_automation_shape(&track, &lane, AutomationCurveShape::Hold);
        state.set_automation_segment_tension(&track, &lane, ids[1], 0.5);
        assert_eq!(
            state.automation_lane(&track, &lane).unwrap().points[1].curve,
            AutomationCurve::Linear
        );
        for shape in AutomationCurveShape::ALL {
            assert_eq!(
                AutomationCurveShape::from_command(shape.command()),
                Some(shape)
            );
            let (curve, tension) = shape.curve_and_tension();
            assert_eq!(AutomationCurveShape::of(curve, tension), Some(shape));
        }
    }

    /// Paste lands the copied shape at the new beat, replacing what was there,
    /// and leaves the pasted points selected.
    #[test]
    fn pasted_points_keep_their_shape_and_replace_what_they_cover() {
        let (mut state, track, lane) = lane_with_points(&[0.0, 2.0, 10.0, 11.0]);
        let ids: Vec<u64> = state
            .automation_lane(&track, &lane)
            .unwrap()
            .points
            .iter()
            .map(|p| p.id)
            .collect();
        state.set_automation_selection(&track, &lane, &ids[..2]);
        let copied = state
            .copy_selected_automation(&track, &lane)
            .expect("copied");
        assert_eq!(copied.span, 2.0);
        assert_eq!(state.paste_automation(&track, &lane, 9.5, &copied), 2);
        let points = &state.automation_lane(&track, &lane).unwrap().points;
        let beats: Vec<f32> = points.iter().map(|p| p.beat).collect();
        assert_eq!(
            beats,
            vec![0.0, 2.0, 9.5, 11.5],
            "10 and 11 were covered and replaced"
        );
        assert!(points[2].selected && points[3].selected && !points[0].selected);
        assert!((points[3].value - 0.2).abs() < 1e-6);
    }
}

#[cfg(test)]
mod master_lane_tests {
    use super::*;

    fn state_with_one_track() -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.create_audio_track();
        state
    }

    /// The master keeps its own lanes: a lane made for it lands on the master
    /// bus, named for the master, and no track gains one.
    #[test]
    fn master_lanes_live_on_the_master_bus() {
        let mut state = state_with_one_track();
        let lane = state
            .ensure_automation_lane(MASTER_TRACK_ID, AutomationTarget::TrackVolume)
            .expect("master lane");
        assert_eq!(state.master.automation_lanes.len(), 1);
        assert_eq!(state.master.automation_lanes[0].name, "Master Volume");
        assert!(state.tracks[0].automation_lanes.is_empty());
        assert!(state.automation_lane(MASTER_TRACK_ID, &lane).is_some());
        // Point edits reach it through the same calls a track lane uses.
        state.add_automation_point(MASTER_TRACK_ID, &lane, 4.0, 0.25);
        assert_eq!(
            state
                .automation_lane(MASTER_TRACK_ID, &lane)
                .unwrap()
                .points
                .len(),
            1
        );
        // And undo's snapshot restores it.
        let before = state.capture_automation_lanes(MASTER_TRACK_ID);
        state.clear_all_automation_lanes(MASTER_TRACK_ID);
        assert!(state.master.automation_lanes.is_empty());
        state.set_track_automation_lanes(MASTER_TRACK_ID, before);
        assert_eq!(state.master.automation_lanes.len(), 1);
    }

    /// Showing master automation opens a block after the last track with a
    /// Volume lane, and the lane's geometry is inside that block.
    #[test]
    fn showing_master_automation_adds_a_block_after_the_tracks() {
        let mut state = state_with_one_track();
        let tracks_height = state.track_row_layout().total_height;
        assert_eq!(state.track_row_layout().master_height, 0.0);

        assert_eq!(
            state.toggle_track_lane_mode(MASTER_TRACK_ID),
            Some(TrackLaneMode::Automation)
        );
        let layout = state.track_row_layout();
        assert_eq!(layout.master_y, tracks_height);
        assert_eq!(
            layout.master_height,
            AUTOMATION_CONTROL_LANE_HEIGHT + AUTOMATION_SUBLANE_HEIGHT
        );
        assert_eq!(layout.total_height, tracks_height + layout.master_height);
        let lane = &state.master.automation_lanes[0];
        assert_eq!(lane.target, AutomationTarget::TrackVolume);
        assert_eq!(
            state.automation_sublane_geometry(MASTER_TRACK_ID, &lane.id),
            Some((
                tracks_height + AUTOMATION_CONTROL_LANE_HEIGHT,
                AUTOMATION_SUBLANE_HEIGHT
            ))
        );
        // The block does not belong to any track.
        assert!(state
            .track_row_layout()
            .track_at_content_y(tracks_height + 1.0)
            .is_none());

        state.toggle_track_lane_mode(MASTER_TRACK_ID);
        assert_eq!(state.track_row_layout().master_height, 0.0);
        assert_eq!(
            state.master.automation_lanes.len(),
            1,
            "hiding keeps the lanes"
        );
    }

    /// The master offers its volume and each loaded insert's bypass, and is
    /// never made the selected track.
    #[test]
    fn master_targets_and_selection() {
        let mut state = state_with_one_track();
        state.master.inserts.push(InsertSlotState {
            display_name: "Limiter".to_string(),
            ..InsertSlotState::empty("master-ins-1")
        });
        let bypass = AutomationTarget::InsertBypass {
            insert_id: "master-ins-1".to_string(),
        };
        let targets = state.master_automation_targets();
        assert!(targets.contains(&AutomationTarget::TrackVolume));
        assert_eq!(
            state.automation_target_label(MASTER_TRACK_ID, &bypass),
            "Limiter Bypass"
        );
        let before = state.selection.selected_track_id.clone();
        state.select_track(MASTER_TRACK_ID);
        assert_eq!(state.selection.selected_track_id, before);
    }
}
