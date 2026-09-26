use super::*;

/// In-flight time-signature marker drag on the global Time Signature lane.
#[derive(Debug, Clone)]
pub struct TimeSignaturePointDrag {
    pub point_id: String,
    pub moved: bool,
}

// ── Time signature map ───────────────────────────────────────────────────────

pub type TimeSignaturePointId = String;

pub const TS_NUMERATOR_MIN: u16 = 1;

pub const TS_NUMERATOR_MAX: u16 = 64;

pub const TS_ALLOWED_DENOMINATORS: [u16; 6] = [1, 2, 4, 8, 16, 32];

pub const TS_BEAT_EPSILON: f64 = 1e-6;

/// Normalize a denominator to one of the allowed note values.
pub fn normalize_time_signature_denominator(denominator: u16) -> u16 {
    TS_ALLOWED_DENOMINATORS
        .iter()
        .copied()
        .min_by_key(|allowed| (denominator as i32 - *allowed as i32).unsigned_abs())
        .unwrap_or(4)
}

/// Quarter-note beats per bar for a time signature.
pub fn beats_per_bar_from_sig(numerator: u16, denominator: u16) -> f64 {
    let num = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX) as f64;
    let den = normalize_time_signature_denominator(denominator).max(1) as f64;
    num * (4.0 / den)
}

/// One denominator-note unit expressed in quarter-note beats (N/D => 4/D).
pub fn denominator_unit_quarter_beats(denominator: u16) -> f64 {
    4.0 / normalize_time_signature_denominator(denominator).max(1) as f64
}

/// Default accent grouping for a meter. Sum always equals `numerator`.
pub fn default_time_signature_grouping(numerator: u16, denominator: u16) -> Vec<u16> {
    let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
    let denominator = normalize_time_signature_denominator(denominator);
    match (numerator, denominator) {
        (2, 4) => vec![2],
        (3, 4) => vec![3],
        (4, 4) => vec![4],
        (5, 8) => vec![2, 3],
        (6, 8) => vec![3, 3],
        (7, 8) => vec![2, 2, 3],
        (9, 8) => vec![3, 3, 3],
        (12, 8) => vec![3, 3, 3, 3],
        (n, 8) if n % 2 == 1 && n > 3 => {
            let pairs = ((n - 3) / 2) as usize;
            let mut groups = vec![2; pairs];
            groups.push(3);
            groups
        }
        _ => vec![numerator],
    }
}

pub fn normalize_time_signature_grouping(
    numerator: u16,
    denominator: u16,
    grouping: &[u16],
) -> Vec<u16> {
    let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
    if grouping.is_empty()
        || grouping.contains(&0)
        || grouping.iter().map(|&g| g as u32).sum::<u32>() != numerator as u32
    {
        default_time_signature_grouping(numerator, denominator)
    } else {
        grouping.to_vec()
    }
}

/// Cumulative denominator-beat indices (0-based) where each accent group begins.
pub fn time_signature_group_starts(grouping: &[u16]) -> Vec<u16> {
    let mut starts = vec![0u16];
    let mut acc = 0u16;
    for (i, &grp) in grouping.iter().enumerate() {
        if i > 0 {
            starts.push(acc);
        }
        acc = acc.saturating_add(grp);
    }
    starts
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeSignaturePoint {
    pub id: TimeSignaturePointId,
    pub beat: f64,
    pub numerator: u16,
    pub denominator: u16,
    /// Accent grouping in denominator-beat units (e.g. 5/8 => [2, 3]).
    pub grouping: Vec<u16>,
}

impl TimeSignaturePoint {
    pub fn new(beat: f64, numerator: u16, denominator: u16) -> Self {
        let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
        let denominator = normalize_time_signature_denominator(denominator);
        Self {
            id: next_time_signature_point_id(),
            beat: beat.max(0.0),
            numerator,
            denominator,
            grouping: default_time_signature_grouping(numerator, denominator),
        }
    }

    pub fn with_id(id: impl Into<String>, beat: f64, numerator: u16, denominator: u16) -> Self {
        let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
        let denominator = normalize_time_signature_denominator(denominator);
        Self {
            id: id.into(),
            beat: beat.max(0.0),
            numerator,
            denominator,
            grouping: default_time_signature_grouping(numerator, denominator),
        }
    }

    pub fn with_grouping(
        id: impl Into<String>,
        beat: f64,
        numerator: u16,
        denominator: u16,
        grouping: Vec<u16>,
    ) -> Self {
        let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
        let denominator = normalize_time_signature_denominator(denominator);
        Self {
            id: id.into(),
            beat: beat.max(0.0),
            numerator,
            denominator,
            grouping: normalize_time_signature_grouping(numerator, denominator, &grouping),
        }
    }

    pub fn effective_grouping(&self) -> Vec<u16> {
        normalize_time_signature_grouping(self.numerator, self.denominator, &self.grouping)
    }

    pub fn group_starts(&self) -> Vec<u16> {
        time_signature_group_starts(&self.effective_grouping())
    }

    pub fn label(&self) -> String {
        TimeSignatureMap::format_marker_label(self.numerator, self.denominator)
    }
}

/// One arrangement bar background span in quarter-note beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarBackgroundRect {
    pub bar: i64,
    pub start_beat: f64,
    pub end_beat: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BarBeat {
    pub bar: i64,
    /// 1-based denominator-beat index within the bar (1 = downbeat).
    pub beat_in_bar: u16,
    /// Fractional position within the current denominator beat (0..1).
    pub sub_beat_fraction: f64,
    pub numerator: u16,
    pub denominator: u16,
}

/// Project-level time signature markers. Global timing data — not owned by any
/// track. Ruler labels, grid grouping, transport display, and metronome accents
/// all evaluate this map at the relevant beat.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeSignatureMap {
    pub points: Vec<TimeSignaturePoint>,
    revision: u64,
}

/// The map every project starts with: one implicit 4/4 at bar one.
///
/// Built once. It used to be allocated fresh — `String` id and all — on every
/// call that needed the point list of a project with no meter changes, which is
/// most projects and dozens of calls a frame.
fn implicit_time_signature_points() -> &'static [TimeSignaturePoint] {
    static IMPLICIT: std::sync::OnceLock<Vec<TimeSignaturePoint>> = std::sync::OnceLock::new();
    IMPLICIT
        .get_or_init(|| vec![TimeSignaturePoint::with_id("implicit-4-4", 0.0, 4, 4)])
        .as_slice()
}

impl TimeSignatureMap {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            revision: 0,
        }
    }

    pub fn with_default_4_4() -> Self {
        let mut map = Self::new();
        map.points.push(TimeSignaturePoint::new(0.0, 4, 4));
        map.bump_revision();
        map
    }

    pub fn with_points(points: Vec<TimeSignaturePoint>) -> Self {
        let mut map = Self::new();
        map.points = points;
        map.sort();
        map.bump_revision();
        map
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Overwrite the marker list from an undo/redo snapshot, bumping the
    /// revision forward so downstream caches still invalidate.
    pub fn restore_points(&mut self, points: Vec<TimeSignaturePoint>) {
        self.points = points;
        self.bump_revision();
    }

    pub fn has_markers(&self) -> bool {
        !self.points.is_empty()
    }

    pub fn format_marker_label(numerator: u16, denominator: u16) -> String {
        format!(
            "{}/{}",
            numerator,
            normalize_time_signature_denominator(denominator)
        )
    }

    pub fn ensure_point_ids(&mut self) {
        for point in &mut self.points {
            if point.id.is_empty() {
                point.id = next_time_signature_point_id();
            }
        }
    }

    /// Seed beat-0 4/4 when empty (legacy projects / first show).
    pub fn ensure_default_point(&mut self) {
        if self.points.is_empty() {
            self.points.push(TimeSignaturePoint::new(0.0, 4, 4));
            self.bump_revision();
        }
        self.ensure_point_ids();
    }

    pub fn time_signature_at_beat(&self, beat: f64) -> TimeSignaturePoint {
        let beat = beat.max(0.0);
        if self.points.is_empty() {
            return TimeSignaturePoint::with_id("implicit-4-4", 0.0, 4, 4);
        }
        let mut idx = 0usize;
        for (i, p) in self.points.iter().enumerate() {
            if p.beat <= beat + TS_BEAT_EPSILON {
                idx = i;
            } else {
                break;
            }
        }
        self.points[idx].clone()
    }

    /// The meter in force at `beat`, without materialising a point.
    ///
    /// `time_signature_at_beat` clones the point — and so its `String` id — and
    /// the grid builder calls it once per sub-beat slot while laying out the
    /// ruler. Everything that only wants the numbers should come here.
    pub fn time_signature_values_at_beat(&self, beat: f64) -> (u16, u16) {
        if self.points.is_empty() {
            return (4, 4);
        }
        let mut idx = 0usize;
        for (i, p) in self.points.iter().enumerate() {
            if p.beat <= beat + TS_BEAT_EPSILON {
                idx = i;
            } else {
                break;
            }
        }
        let point = &self.points[idx];
        (point.numerator, point.denominator)
    }

    pub fn beats_per_bar_at_beat(&self, beat: f64) -> f64 {
        let (numerator, denominator) = self.time_signature_values_at_beat(beat);
        beats_per_bar_from_sig(numerator, denominator)
    }

    pub fn bar_beat_at_beat(&self, beat: f64) -> BarBeat {
        let beat = beat.max(0.0);
        let points = self.sorted_points();
        let mut global_bar: i64 = 1;

        for (i, pt) in points.iter().enumerate() {
            let seg_start = pt.beat;
            let seg_end = points.get(i + 1).map(|p| p.beat).unwrap_or(f64::INFINITY);
            if beat + TS_BEAT_EPSILON < seg_start {
                continue;
            }
            let bpb = beats_per_bar_from_sig(pt.numerator, pt.denominator);
            let denom_unit = denominator_unit_quarter_beats(pt.denominator);
            if beat < seg_end - TS_BEAT_EPSILON || i + 1 == points.len() {
                let rel = (beat - seg_start).max(0.0);
                let bar_offset = (rel / bpb).floor() as i64;
                let beat_in_bar_q = rel - bar_offset as f64 * bpb;
                let denom_idx = (beat_in_bar_q / denom_unit).floor();
                let sub_frac = if denom_unit > TS_BEAT_EPSILON {
                    (beat_in_bar_q / denom_unit).fract()
                } else {
                    0.0
                };
                return BarBeat {
                    bar: global_bar + bar_offset,
                    beat_in_bar: (denom_idx as u16).saturating_add(1),
                    sub_beat_fraction: sub_frac,
                    numerator: pt.numerator,
                    denominator: pt.denominator,
                };
            }
            let bars_in_seg = ((seg_end - seg_start) / bpb).floor() as i64;
            global_bar += bars_in_seg.max(0);
        }

        BarBeat {
            bar: 1,
            beat_in_bar: 1,
            sub_beat_fraction: 0.0,
            numerator: 4,
            denominator: 4,
        }
    }

    pub fn beat_at_bar_beat(&self, bar: i64, beat_in_bar: u16) -> f64 {
        let bar = bar.max(1);
        let beat_in_bar = beat_in_bar.max(1);
        let points = self.sorted_points();
        let mut global_bar: i64 = 1;

        for (i, pt) in points.iter().enumerate() {
            let seg_start = pt.beat;
            let seg_end = points.get(i + 1).map(|p| p.beat).unwrap_or(f64::INFINITY);
            let bpb = beats_per_bar_from_sig(pt.numerator, pt.denominator);
            let denom_unit = denominator_unit_quarter_beats(pt.denominator);
            let bars_in_seg = if seg_end.is_finite() {
                ((seg_end - seg_start) / bpb).floor() as i64
            } else {
                i64::MAX / 2
            };

            if bar < global_bar + bars_in_seg || i + 1 == points.len() {
                let bar_offset = (bar - global_bar).max(0);
                return seg_start
                    + bar_offset as f64 * bpb
                    + (beat_in_bar.saturating_sub(1) as f64) * denom_unit;
            }
            global_bar += bars_in_seg;
        }
        0.0
    }

    /// Snap a marker beat to the start of its current bar (MVP bar-boundary insert).
    pub fn snap_marker_beat_to_bar_boundary(&self, beat: f64) -> f64 {
        let bb = self.bar_beat_at_beat(beat.max(0.0));
        self.bar_start_beat(bb.bar)
    }

    pub fn bar_start_beat(&self, bar: i64) -> f64 {
        self.beat_at_bar_beat(bar, 1)
    }

    pub fn next_bar_beat(&self, beat: f64) -> f64 {
        let bb = self.bar_beat_at_beat(beat);
        let bpb = beats_per_bar_from_sig(bb.numerator, bb.denominator);
        self.bar_start_beat(bb.bar) + bpb
    }

    pub fn previous_bar_beat(&self, beat: f64) -> f64 {
        let bb = self.bar_beat_at_beat(beat);
        if bb.bar <= 1 {
            return 0.0;
        }
        self.bar_start_beat(bb.bar - 1)
    }

    pub fn format_position_at_beat(&self, beat: f64) -> String {
        let bb = self.bar_beat_at_beat(beat);
        format!("{}.{}", bb.bar, bb.beat_in_bar)
    }

    /// Global bar number containing `beat`.
    pub fn bar_at_beat(&self, beat: f64) -> i64 {
        self.bar_beat_at_beat(beat).bar
    }

    /// Enumerate bar spans intersecting a visible beat range for background paint.
    pub fn visible_bar_rects(
        &self,
        visible_start: f64,
        visible_end: f64,
    ) -> Vec<BarBackgroundRect> {
        const MAX_BARS: i64 = 4096;
        let visible_start = visible_start.max(0.0);
        let visible_end = visible_end.max(visible_start);
        let start_bar = self.bar_at_beat(visible_start);
        let mut rects = Vec::new();
        for offset in 0..MAX_BARS {
            let bar = start_bar + offset;
            let start_beat = self.bar_start_beat(bar);
            if start_beat >= visible_end - TS_BEAT_EPSILON {
                break;
            }
            let end_beat = self.bar_start_beat(bar + 1);
            if end_beat > visible_start + TS_BEAT_EPSILON
                && start_beat < visible_end - TS_BEAT_EPSILON
            {
                rects.push(BarBackgroundRect {
                    bar,
                    start_beat,
                    end_beat,
                });
            }
        }
        rects
    }

    pub fn add_or_update_point(&mut self, beat: f64, numerator: u16, denominator: u16) {
        let beat = self.snap_marker_beat_to_bar_boundary(beat.max(0.0));
        let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
        let denominator = normalize_time_signature_denominator(denominator);
        if let Some(existing) = self
            .points
            .iter_mut()
            .find(|p| (p.beat - beat).abs() < TS_BEAT_EPSILON)
        {
            existing.numerator = numerator;
            existing.denominator = denominator;
            existing.grouping = default_time_signature_grouping(numerator, denominator);
        } else {
            self.points
                .push(TimeSignaturePoint::new(beat, numerator, denominator));
        }
        self.sort();
        self.bump_revision();
    }

    pub fn update_point_by_id(&mut self, id: &str, numerator: u16, denominator: u16) -> bool {
        let numerator = numerator.clamp(TS_NUMERATOR_MIN, TS_NUMERATOR_MAX);
        let denominator = normalize_time_signature_denominator(denominator);
        if let Some(point) = self.points.iter_mut().find(|p| p.id == id) {
            point.numerator = numerator;
            point.denominator = denominator;
            point.grouping = default_time_signature_grouping(numerator, denominator);
            self.bump_revision();
            true
        } else {
            false
        }
    }

    pub fn move_point_by_id(&mut self, id: &str, beat: f64) -> bool {
        let beat = self.snap_marker_beat_to_bar_boundary(beat.max(0.0));
        if self
            .points
            .iter()
            .any(|p| p.id != id && (p.beat - beat).abs() < TS_BEAT_EPSILON)
        {
            return false;
        }
        if let Some(point) = self.points.iter_mut().find(|p| p.id == id) {
            point.beat = beat;
            self.sort();
            self.bump_revision();
            true
        } else {
            false
        }
    }

    pub fn remove_point_by_id(&mut self, id: &str) -> bool {
        if let Some(idx) = self.points.iter().position(|p| p.id == id) {
            self.points.remove(idx);
            self.bump_revision();
            true
        } else {
            false
        }
    }

    pub fn reset_to_single_point(&mut self, beat: f64, numerator: u16, denominator: u16) {
        self.points.clear();
        self.points
            .push(TimeSignaturePoint::new(beat, numerator, denominator));
        self.sort();
        self.bump_revision();
    }

    fn sort(&mut self) {
        self.points.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// The meter map in beat order, borrowed whenever it already is.
    ///
    /// `bar_beat_at_beat` runs once per ruler label and twice per visible bar
    /// shade — dozens of times per frame — and this used to clone the point
    /// list (each point owning a `String` id) and sort it on every one of them.
    /// A project with no meter changes paid a heap allocation per call just to
    /// materialise the implicit 4/4.
    ///
    /// The list is kept sorted by every mutator, so the sorted-check is the
    /// path taken; the owned branch is the safety net for a list that arrived
    /// out of order (an older project file, a hand-edited one).
    fn sorted_points(&self) -> std::borrow::Cow<'_, [TimeSignaturePoint]> {
        if self.points.is_empty() {
            return std::borrow::Cow::Borrowed(implicit_time_signature_points());
        }
        if self
            .points
            .windows(2)
            .all(|pair| pair[0].beat <= pair[1].beat)
        {
            return std::borrow::Cow::Borrowed(self.points.as_slice());
        }
        let mut points = self.points.clone();
        points.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        std::borrow::Cow::Owned(points)
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

impl TimelineState {
    pub fn beats_per_bar(&self) -> f32 {
        self.beats_per_bar_at_beat(self.transport.playhead_beats as f64) as f32
    }

    pub fn beats_per_bar_at_beat(&self, beat: f64) -> f64 {
        self.time_signature_map.beats_per_bar_at_beat(beat)
    }

    pub fn time_signature_at_playhead(&self) -> TimeSignaturePoint {
        self.time_signature_map
            .time_signature_at_beat(self.transport.playhead_beats as f64)
    }

    pub fn time_signature_has_markers(&self) -> bool {
        self.time_signature_map.points.len() > 1
            || self
                .time_signature_map
                .points
                .first()
                .is_some_and(|p| p.beat > TS_BEAT_EPSILON)
    }

    pub fn sync_legacy_time_signature_fields(&mut self) {
        let pt = self.time_signature_map.time_signature_at_beat(0.0);
        self.time_signature_num = pt.numerator as u32;
        self.time_signature_den = pt.denominator as u32;
    }

    pub const TIME_SIGNATURE_TRACK_HEIGHT: f32 = 40.0;
    pub const TIME_SIGNATURE_TRACK_HEIGHT_COLLAPSED: f32 = 28.0;

    pub fn time_signature_track_height(&self) -> f32 {
        if !self.show_time_signature_track {
            return 0.0;
        }
        self.global_lane_height(GlobalLaneKind::TimeSignature)
    }

    /// Replace the whole meter map from an undo/redo snapshot. Keeps the legacy
    /// `time_signature_num`/`_den` mirror in step and drops a selection that no
    /// longer names a live marker.
    pub fn restore_time_signature_state(&mut self, points: Vec<TimeSignaturePoint>) {
        self.time_signature_map.restore_points(points);
        self.sync_legacy_time_signature_fields();
        if let Some(id) = self.selected_time_signature_point_id.clone() {
            if !self.time_signature_map.points.iter().any(|p| p.id == id) {
                self.selected_time_signature_point_id = None;
            }
        }
    }

    /// Returns whether the project changed: the lane was hidden (its visibility
    /// is saved with the project) or the default 4/4 point was seeded.
    pub fn show_time_signature_track_lane(&mut self) -> bool {
        let changed = !self.show_time_signature_track || self.time_signature_map.points.is_empty();
        self.show_time_signature_track = true;
        self.time_signature_map.ensure_default_point();
        changed
    }

    /// Returns whether the lane was shown, i.e. whether the saved view changed.
    pub fn hide_time_signature_track_lane(&mut self) -> bool {
        let changed = self.show_time_signature_track;
        self.show_time_signature_track = false;
        self.selected_time_signature_point_id = None;
        changed
    }

    pub fn select_time_signature_point(&mut self, id: &str) {
        self.selected_time_signature_point_id = Some(id.to_string());
    }

    pub fn add_time_signature_point(
        &mut self,
        beat: f64,
        numerator: u16,
        denominator: u16,
    ) -> Option<String> {
        self.time_signature_map
            .add_or_update_point(beat, numerator, denominator);
        self.time_signature_map.ensure_point_ids();
        self.sync_legacy_time_signature_fields();
        self.time_signature_map
            .points
            .iter()
            .find(|p| (p.beat - beat).abs() < TS_BEAT_EPSILON)
            .map(|p| p.id.clone())
    }

    pub fn move_time_signature_point(&mut self, id: &str, beat: f64) -> bool {
        if self.time_signature_map.move_point_by_id(id, beat) {
            self.sync_legacy_time_signature_fields();
            true
        } else {
            false
        }
    }

    pub fn update_time_signature_point(
        &mut self,
        id: &str,
        numerator: u16,
        denominator: u16,
    ) -> bool {
        if self
            .time_signature_map
            .update_point_by_id(id, numerator, denominator)
        {
            self.sync_legacy_time_signature_fields();
            true
        } else {
            false
        }
    }

    pub fn delete_time_signature_point(&mut self, id: &str) -> bool {
        if self.time_signature_map.remove_point_by_id(id) {
            if self.selected_time_signature_point_id.as_deref() == Some(id) {
                self.selected_time_signature_point_id = None;
            }
            self.time_signature_map.ensure_default_point();
            self.sync_legacy_time_signature_fields();
            true
        } else {
            false
        }
    }

    pub fn clear_time_signature_markers(&mut self, playhead_beat: f64) {
        let pt = self
            .time_signature_map
            .time_signature_at_beat(playhead_beat);
        self.time_signature_map
            .reset_to_single_point(0.0, pt.numerator, pt.denominator);
        self.sync_legacy_time_signature_fields();
        self.selected_time_signature_point_id = None;
    }

    pub fn time_signature_point_at(&self, beat: f64, beat_tol: f64) -> Option<String> {
        self.time_signature_map
            .points
            .iter()
            .find(|p| (p.beat - beat).abs() <= beat_tol)
            .map(|p| p.id.clone())
    }

    /// Secondary label for the Time Signature lane header.
    pub fn time_signature_lane_header_subtitle(&self) -> String {
        let pt = self.time_signature_at_playhead();
        if !self.time_signature_has_markers() {
            format!("Fixed {}", pt.label())
        } else {
            let count = self.time_signature_map.points.len();
            if count > 1 {
                format!("{} · {} markers", pt.label(), count)
            } else {
                pt.label()
            }
        }
    }

    pub fn clear_time_signature_point_selection(&mut self) {
        self.selected_time_signature_point_id = None;
    }
}
