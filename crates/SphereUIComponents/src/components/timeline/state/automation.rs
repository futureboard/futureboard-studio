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
    pub track_targets: Vec<(AutomationTarget, bool)>,
}

/// Interpolation shape between an automation point and the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationCurve {
    Linear,
    Hold,
    /// S-curve — reserved. Evaluated as Linear until the curve math lands, but
    /// stored/round-tripped so existing data is never lost.
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
}

impl AutomationTarget {
    /// Short label shown on the lane header / target picker.
    pub fn display_name(&self) -> String {
        match self {
            AutomationTarget::TrackVolume => "Volume".to_string(),
            AutomationTarget::TrackPan => "Pan".to_string(),
            AutomationTarget::TrackMute => "Mute".to_string(),
            AutomationTarget::PluginParameter { parameter_name, .. } => parameter_name.clone(),
            AutomationTarget::SendLevel { send_id } => format!("Send {send_id}"),
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
            AutomationTarget::SendLevel { .. } => 0.0,
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
            AutomationTarget::PluginParameter { .. } | AutomationTarget::SendLevel { .. } => {
                format!("{:.0}%", norm * 100.0)
            }
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
}

impl AutomationLaneState {
    /// Build an empty lane for `target` with an auto-derived id/name.
    pub fn new(id: impl Into<String>, target: AutomationTarget) -> Self {
        Self {
            id: id.into(),
            name: target.display_name(),
            target,
            enabled: true,
            visible: false,
            points: Vec::new(),
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
            let span = (b.beat - a.beat).max(1.0e-6);
            let t = ((beat - a.beat) / span).clamp(0.0, 1.0);
            let factor = DirectAudio::automation_curve_factor(a.curve.to_tag(), a.tension, t);
            return a.value + (b.value - a.value) * factor;
        }
    }
    points[last].value
}

impl TimelineState {
    // ── Automation: mode, target, lanes, points ──────────────────────────────
    // Single source of truth for automation edits. The TrackHeader toggle, the
    // lane editor, and keyboard commands all route through these. Selection and
    // mode toggles are UI-only (never dirty the engine); point add/move/delete
    // and target/lane changes are committed edits the caller marks dirty once.

    pub fn track_lane_mode(&self, track_id: &str) -> TrackLaneMode {
        self.find_track(track_id)
            .map(|t| t.lane_mode)
            .unwrap_or(TrackLaneMode::Clips)
    }

    /// Toggle a track between Clip and Automation mode. Never reaches the
    /// engine, but the mode is saved with the project (v54), so callers mark it
    /// view-only dirty. Returns the new mode. Selecting Automation mode also
    /// makes sure a lane exists for the active target so the editor has
    /// something to draw.
    pub fn toggle_track_lane_mode(&mut self, track_id: &str) -> Option<TrackLaneMode> {
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
        let Some(track) = self.find_track(track_id) else {
            return AutomationTarget::TrackVolume;
        };
        if let Some(target) = track.selected_automation_target.clone() {
            return target;
        }
        track
            .automation_lanes
            .first()
            .map(|l| l.target.clone())
            .unwrap_or(AutomationTarget::TrackVolume)
    }

    /// Targets offered by the picker for a track: Volume, Pan, Mute, then plugin
    /// parameters (when metadata is available from the plugin host).
    pub fn available_automation_targets(&self, track_id: &str) -> Vec<AutomationTarget> {
        let mut out = vec![
            AutomationTarget::TrackVolume,
            AutomationTarget::TrackPan,
            AutomationTarget::TrackMute,
        ];
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
        let track = self.find_track(track_id)?;
        let existing: std::collections::HashSet<_> = track
            .automation_lanes
            .iter()
            .map(|lane| lane.target.clone())
            .collect();
        let track_targets = [
            AutomationTarget::TrackVolume,
            AutomationTarget::TrackPan,
            AutomationTarget::TrackMute,
        ]
        .into_iter()
        .map(|target| (target.clone(), existing.contains(&target)))
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
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            track.selected_automation_target = Some(target.clone());
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

    fn lane_index_for_target(track: &TrackState, target: &AutomationTarget) -> Option<usize> {
        track
            .automation_lanes
            .iter()
            .position(|l| l.target == *target)
    }

    /// Ensure a lane exists for `target` on `track_id`; returns its id.
    pub fn ensure_automation_lane(
        &mut self,
        track_id: &str,
        target: AutomationTarget,
    ) -> Option<String> {
        let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        if let Some(idx) = Self::lane_index_for_target(track, &target) {
            return Some(track.automation_lanes[idx].id.clone());
        }
        let lane_id = format!("autolane-{}-{}", track.id, track.automation_lanes.len() + 1);
        let mut lane = AutomationLaneState::new(lane_id.clone(), target);
        // New lanes are shown as sub-rows immediately so creating a target (via
        // the picker / toggle) reveals its lane under the parent track.
        lane.visible = true;
        track.automation_lanes.push(lane);
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
        let track = self.find_track(track_id)?;
        let target = self.active_automation_target(track_id);
        track
            .automation_lanes
            .iter()
            .find(|l| l.target == target)
            .map(|l| l.id.clone())
    }

    fn lane_mut(&mut self, track_id: &str, lane_id: &str) -> Option<&mut AutomationLaneState> {
        self.tracks
            .iter_mut()
            .find(|t| t.id == track_id)?
            .automation_lanes
            .iter_mut()
            .find(|l| l.id == lane_id)
    }

    pub fn automation_lane(&self, track_id: &str, lane_id: &str) -> Option<&AutomationLaneState> {
        self.find_track(track_id)?
            .automation_lanes
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

    /// Set a segment's tension (clamped to `-1.0..=1.0`), forcing the curved
    /// (Linear) kind so the tension is visible/audible. Committed edit — the
    /// caller dirties once on release.
    pub fn set_automation_segment_tension(
        &mut self,
        track_id: &str,
        lane_id: &str,
        left_point_id: u64,
        tension: f32,
    ) {
        if let Some(lane) = self.lane_mut(track_id, lane_id) {
            if let Some(p) = lane.points.iter_mut().find(|p| p.id == left_point_id) {
                p.curve = AutomationCurve::Linear;
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
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        let mut changed = false;
        for lane in track.automation_lanes.iter_mut() {
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
        self.find_track(track_id)
            .map(|t| {
                t.automation_lanes
                    .iter()
                    .flat_map(|l| l.points.iter())
                    .filter(|p| p.selected)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Delete every selected automation point on a track. Committed edit —
    /// caller marks dirty once. Returns how many were removed.
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
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(lane) = track.automation_lanes.iter_mut().find(|l| l.id == lane_id) else {
            return false;
        };
        let before = lane.points.len();
        lane.points.retain(|point| point.id != point_id);
        let removed = before != lane.points.len();
        if removed && automation_debug_enabled() {
            eprintln!("[automation] delete_point track={track_id} lane={lane_id} id={point_id}");
        }
        removed
    }

    pub fn delete_selected_automation_points(&mut self, track_id: &str) -> usize {
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return 0;
        };
        let mut removed = 0;
        for lane in track.automation_lanes.iter_mut() {
            let before = lane.points.len();
            lane.points.retain(|p| !p.selected);
            removed += before - lane.points.len();
        }
        if removed > 0 && automation_debug_enabled() {
            eprintln!(
                "[automation] delete_points track={} count={}",
                track_id, removed
            );
        }
        removed
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
        if let (Some(target), Some(track)) =
            (target, self.tracks.iter_mut().find(|t| t.id == track_id))
        {
            track.selected_automation_target = Some(target);
        }
    }

    /// Toggle a lane's enabled flag (read on/off). Committed edit — when off the
    /// lane keeps its points but stops driving the target. Returns the new state.
    pub fn toggle_automation_lane_enabled(
        &mut self,
        track_id: &str,
        lane_id: &str,
    ) -> Option<bool> {
        let lane = self.lane_mut(track_id, lane_id)?;
        lane.enabled = !lane.enabled;
        let enabled = lane.enabled;
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "lane_enable_toggle");
        Some(enabled)
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
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        let before = track.automation_lanes.len();
        track.automation_lanes.retain(|l| l.id != lane_id);
        if track.automation_lanes.len() == before {
            return false;
        }
        if track
            .selected_automation_target
            .as_ref()
            .is_some_and(|t| !track.automation_lanes.iter().any(|l| l.target == *t))
        {
            track.selected_automation_target =
                track.automation_lanes.first().map(|l| l.target.clone());
        }
        let playhead = self.transport.playhead_beats;
        self.recompute_effective_volumes(playhead, "lane_remove");
        true
    }

    /// Remove every automation lane on a track. Committed edit. Returns how
    /// many lanes were removed.
    pub fn clear_all_automation_lanes(&mut self, track_id: &str) -> usize {
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return 0;
        };
        let removed = track.automation_lanes.len();
        if removed == 0 {
            return 0;
        }
        track.automation_lanes.clear();
        track.selected_automation_target = None;
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
        self.tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.automation_lanes.clone())
            .unwrap_or_default()
    }

    /// Replace a track's automation lanes wholesale. The undo/redo counterpart
    /// to [`Self::capture_automation_lanes`]; keeps the selected target valid
    /// and recomputes effective volumes so playback follows the restored curve.
    pub fn set_track_automation_lanes(&mut self, track_id: &str, lanes: Vec<AutomationLaneState>) {
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        track.automation_lanes = lanes;
        // A restored lane set may no longer contain the selected target (undoing
        // the creation of the lane that was selected), so re-point it rather
        // than leaving a selection that resolves to nothing.
        let selected_still_present = track
            .selected_automation_target
            .as_ref()
            .is_some_and(|t| track.automation_lanes.iter().any(|l| l.target == *t));
        if !selected_still_present {
            track.selected_automation_target =
                track.automation_lanes.first().map(|l| l.target.clone());
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
