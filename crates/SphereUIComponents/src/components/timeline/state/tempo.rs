use super::*;

/// In-flight tempo-point move on the global Tempo Track lane.
#[derive(Debug, Clone)]
pub struct TempoPointDrag {
    pub point_id: String,
    /// Set once the point has actually moved so a pure click never marks dirty.
    pub moved: bool,
}

/// In-flight bend of one tempo ramp: vertical travel from the grab point
/// becomes the left marker's tension; the markers themselves never move.
#[derive(Debug, Clone)]
pub struct TempoCurveDrag {
    /// Marker the ramp starts at (tension lives on the left marker).
    pub left_id: String,
    pub start_tension: f32,
    pub start_window_y: f32,
    /// Whether the ramp rises to the next marker. Decides which way "up"
    /// bends it, so dragging up always lifts the line under the pointer.
    pub rising: bool,
    /// Set once the tension actually changed, so a pure click never dirties.
    pub changed: bool,
}

/// What a press on the Tempo lane is dragging.
#[derive(Debug, Clone)]
pub enum TempoLaneDrag {
    Point(TempoPointDrag),
    Curve(TempoCurveDrag),
}

/// Tension change per lane height of vertical drag.
pub const TEMPO_TENSION_GAIN: f32 = 2.4;
/// Tension change per lane height with Shift held, for fine shaping.
pub const TEMPO_TENSION_GAIN_FINE: f32 = 0.6;

/// Tension for a ramp bend dragged from `start_y` to `y` (window px) on a
/// lane `lane_height` tall. Up lifts the line: on a rising ramp that is an
/// ease-out (negative tension), on a falling ramp an ease-in.
pub fn tempo_drag_tension(drag: &TempoCurveDrag, y: f32, lane_height: f32, fine: bool) -> f32 {
    let gain = if fine {
        TEMPO_TENSION_GAIN_FINE
    } else {
        TEMPO_TENSION_GAIN
    };
    let up = (drag.start_window_y - y) / lane_height.max(1.0);
    let direction = if drag.rising { -1.0 } else { 1.0 };
    clamp_tempo_tension(drag.start_tension + direction * up * gain)
}

// ── Tempo map ─────────────────────────────────────────────────────────────────

/// Interpolation shape between a tempo point and the next one.
///
/// The tag is the wire format shared with [`DirectAudio::TempoCurve`], which is
/// where the curve is actually evaluated: the lane draws, the transport plays,
/// and every beat/second/sample conversion resolves through that one map, so a
/// curve here is a curve the engine renders rather than a label on a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempoCurve {
    #[default]
    Hold,
    Linear,
    Smooth,
}

impl TempoCurve {
    pub fn to_tag(self) -> u8 {
        match self {
            TempoCurve::Hold => 0,
            TempoCurve::Linear => 1,
            TempoCurve::Smooth => 2,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => TempoCurve::Linear,
            2 => TempoCurve::Smooth,
            _ => TempoCurve::Hold,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TempoCurve::Hold => "Hold",
            TempoCurve::Linear => "Linear",
            TempoCurve::Smooth => "Smooth",
        }
    }

    /// The engine's matching curve. One conversion point, so the lane and the
    /// transport can never disagree about what shape was drawn.
    pub fn to_engine(self) -> DirectAudio::TempoCurve {
        DirectAudio::TempoCurve::from_tag(self.to_tag())
    }
}

/// A tempo change anchored at a musical beat. The `curve` describes how tempo
/// moves from this point to the next one. Marker labels and the tempo-track
/// editor read `bpm` directly — transport BPM uses [`TempoMap::bpm_at_beat`].
#[derive(Debug, Clone, PartialEq)]
pub struct TempoPoint {
    pub id: String,
    pub beat: f64,
    pub bpm: f64,
    pub curve: TempoCurve,
    /// Bend of a Linear ramp to the next marker, `-1.0..=1.0`: `> 0` eases in,
    /// `< 0` eases out. The same power curve automation lanes use, evaluated
    /// by the engine ([`DirectAudio::TempoCurve::shape`]).
    pub tension: f32,
}

impl TempoPoint {
    pub fn new(beat: f64, bpm: f64, curve: TempoCurve) -> Self {
        Self {
            id: next_tempo_point_id(),
            beat: beat.max(0.0),
            bpm: bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX),
            curve,
            tension: 0.0,
        }
    }

    pub fn with_id(id: impl Into<String>, beat: f64, bpm: f64, curve: TempoCurve) -> Self {
        Self {
            id: id.into(),
            beat: beat.max(0.0),
            bpm: bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX),
            curve,
            tension: 0.0,
        }
    }

    pub fn with_tension(mut self, tension: f32) -> Self {
        self.tension = clamp_tempo_tension(tension);
        self
    }
}

/// Tempo curve tension range, shared with automation lanes.
pub fn clamp_tempo_tension(tension: f32) -> f32 {
    if tension.is_finite() {
        tension.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Project-level tempo automation. This is global state owned by the project,
/// not by any track — the TempoTrack is only a view/controller over this map.
/// When `points` is empty the project plays at the timeline's base BPM; the
/// base BPM is supplied by the caller (`TimelineState::bpm`) so this map stays
/// self-contained and cheap to clone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TempoMap {
    /// Sorted (by beat) tempo markers in addition to the implicit base point at
    /// beat 0. Kept small; UI-thread only, so a linear scan is fine.
    pub points: Vec<TempoPoint>,
    /// Bumped on every edit so UI/engine caches can invalidate.
    revision: u64,
}

impl TempoMap {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            revision: 0,
        }
    }

    pub fn with_points(points: Vec<TempoPoint>) -> Self {
        let mut map = Self::new();
        map.points = points;
        map.sort();
        map.bump_revision();
        map
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// True when the map carries any tempo automation (one or more markers).
    /// With no markers the project is a single static tempo.
    pub fn has_automation(&self) -> bool {
        !self.points.is_empty()
    }

    /// The engine's map for these markers at `base_bpm`.
    ///
    /// The one place the UI resolves musical time, and it resolves it by asking
    /// the same type the audio callback asks. Two implementations of the same
    /// integral is how a curve ends up drawn in one place and played in
    /// another; there is only one, and it lives in the engine.
    ///
    /// Built per call rather than cached: the marker list is small, the result
    /// depends on `base_bpm` which is not part of this map, and a stale cache
    /// here is a playhead that disagrees with the transport. Callers converting
    /// many positions in one pass (grid generation, a clip sweep) should build
    /// it once and query it directly instead of going through the wrappers.
    pub fn to_engine_map(&self, base_bpm: f64) -> DirectAudio::TempoMap {
        DirectAudio::TempoMap::from_points(
            base_bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX),
            self.points
                .iter()
                .map(|p| {
                    DirectAudio::TempoPoint::new(p.beat, p.bpm, p.curve.to_engine())
                        .with_tension(p.tension as f64)
                })
                .collect(),
        )
    }

    /// Elapsed seconds at `beat`, following every marker's curve.
    pub fn seconds_at_beat(&self, beat: f64, base_bpm: f64) -> f64 {
        let beat = beat.max(0.0);
        if self.points.is_empty() {
            return beat * 60.0 / base_bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        }
        self.to_engine_map(base_bpm).seconds_at_beat(beat)
    }

    /// Inverse of [`Self::seconds_at_beat`].
    pub fn beat_at_seconds(&self, seconds: f64, base_bpm: f64) -> f64 {
        let seconds = seconds.max(0.0);
        if self.points.is_empty() {
            return seconds * base_bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX) / 60.0;
        }
        self.to_engine_map(base_bpm).beat_at_seconds(seconds)
    }

    pub fn samples_at_beat(&self, beat: f64, base_bpm: f64, sample_rate: f64) -> u64 {
        (self.seconds_at_beat(beat, base_bpm) * sample_rate.max(1.0))
            .round()
            .max(0.0) as u64
    }

    pub fn beat_at_samples(&self, samples: u64, base_bpm: f64, sample_rate: f64) -> f64 {
        let seconds = samples as f64 / sample_rate.max(1.0);
        self.beat_at_seconds(seconds, base_bpm)
    }

    /// Effective BPM at `beat`, evaluating curves between markers. `base_bpm`
    /// is the implicit tempo at beat 0 (the timeline's nominal BPM).
    pub fn bpm_at_beat(&self, beat: f64, base_bpm: f64) -> f64 {
        if self.points.is_empty() {
            return base_bpm;
        }
        let beat = beat.max(0.0);
        // Build the effective point preceding `beat` and its successor without
        // allocating: walk the implicit base point followed by the markers.
        let first = &self.points[0];
        if beat < first.beat {
            // Before the first marker we sit on the implicit base point. The
            // base point holds (Hold) up to the first marker.
            return base_bpm;
        }
        // Find the last marker at or before `beat`.
        let mut idx = 0usize;
        for (i, p) in self.points.iter().enumerate() {
            if p.beat <= beat {
                idx = i;
            } else {
                break;
            }
        }
        let cur = &self.points[idx];
        let next = self.points.get(idx + 1);
        match (cur.curve, next) {
            (TempoCurve::Hold, _) | (_, None) => cur.bpm,
            (curve, Some(next)) => {
                let span = (next.beat - cur.beat).max(1e-9);
                let t = ((beat - cur.beat) / span).clamp(0.0, 1.0);
                // The engine's shape, so the lane draws the curve that plays.
                let t = curve.to_engine().shape(cur.tension as f64, t);
                cur.bpm + (next.bpm - cur.bpm) * t
            }
        }
    }

    /// Ruler/tempo-track label for a stored marker BPM — never the transport
    /// playhead-evaluated tempo.
    pub fn format_marker_label(bpm: f64) -> String {
        if bpm.fract().abs() < 0.05 {
            format!("{bpm:.0}")
        } else {
            format!("{bpm:.1}")
        }
    }

    /// Assign generated ids to legacy points loaded without one.
    pub fn ensure_point_ids(&mut self) {
        for point in &mut self.points {
            if point.id.is_empty() {
                point.id = next_tempo_point_id();
            }
        }
    }

    /// Id of the marker governing `beat` (last point at or before `beat`).
    pub fn point_id_at_or_before_beat(&self, beat: f64) -> Option<&str> {
        let beat = beat.max(0.0);
        self.points
            .iter()
            .filter(|p| p.beat <= beat)
            .last()
            .map(|p| p.id.as_str())
    }

    /// Update only the matching tempo point's stored BPM by stable id.
    pub fn update_point_bpm_by_id(&mut self, id: &str, bpm: f64) -> bool {
        let bpm = bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        if let Some(point) = self.points.iter_mut().find(|p| p.id == id) {
            point.bpm = bpm;
            self.bump_revision();
            true
        } else {
            false
        }
    }

    /// Insert a tempo marker, replacing any existing marker within a small beat
    /// epsilon. Keeps `points` sorted by beat.
    pub fn add_or_update_point(&mut self, beat: f64, bpm: f64, curve: TempoCurve) {
        let beat = beat.max(0.0);
        let bpm = bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        if let Some(existing) = self
            .points
            .iter_mut()
            .find(|p| (p.beat - beat).abs() < 1e-6)
        {
            existing.bpm = bpm;
            existing.curve = curve;
        } else {
            self.points.push(TempoPoint::new(beat, bpm, curve));
        }
        self.sort();
        self.bump_revision();
    }

    /// Remove the marker nearest `beat` within `epsilon` beats. Returns whether
    /// a marker was removed.
    pub fn remove_point_near(&mut self, beat: f64, epsilon: f64) -> bool {
        if let Some(idx) = self
            .points
            .iter()
            .position(|p| (p.beat - beat).abs() <= epsilon)
        {
            self.points.remove(idx);
            self.bump_revision();
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.points.clear();
        self.bump_revision();
    }

    /// Replace the map with exactly one marker (fixed tempo automation).
    pub fn reset_to_single_point(&mut self, beat: f64, bpm: f64, curve: TempoCurve) {
        self.points.clear();
        self.points.push(TempoPoint::new(beat, bpm, curve));
        self.sort();
        self.bump_revision();
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

    pub fn move_point_by_id(&mut self, id: &str, beat: f64, bpm: f64) -> bool {
        let beat = beat.max(0.0);
        let bpm = bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        if self
            .points
            .iter()
            .any(|p| p.id != id && (p.beat - beat).abs() < TEMPO_BEAT_EPSILON)
        {
            return false;
        }
        if let Some(point) = self.points.iter_mut().find(|p| p.id == id) {
            point.beat = beat;
            point.bpm = bpm;
            self.sort();
            self.bump_revision();
            true
        } else {
            false
        }
    }

    /// Set the bend of a marker's ramp to the next marker.
    pub fn update_point_tension_by_id(&mut self, id: &str, tension: f32) -> bool {
        if let Some(point) = self.points.iter_mut().find(|p| p.id == id) {
            point.tension = clamp_tempo_tension(tension);
            self.bump_revision();
            true
        } else {
            false
        }
    }

    pub fn update_point_curve_by_id(&mut self, id: &str, curve: TempoCurve) -> bool {
        if let Some(point) = self.points.iter_mut().find(|p| p.id == id) {
            point.curve = curve;
            self.bump_revision();
            true
        } else {
            false
        }
    }

    /// Hold a constant tempo from `beat` onward by removing later markers.
    pub fn set_fixed_from_beat(&mut self, beat: f64, bpm: f64, base_bpm: f64) {
        let beat = beat.max(0.0);
        let bpm = bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        self.points.retain(|p| p.beat < beat - TEMPO_BEAT_EPSILON);
        if beat <= TEMPO_BEAT_EPSILON {
            self.reset_to_single_point(0.0, bpm, TempoCurve::Hold);
            return;
        }
        if !self
            .points
            .iter()
            .any(|p| (p.beat - beat).abs() < TEMPO_BEAT_EPSILON)
        {
            if self.points.is_empty() {
                self.add_or_update_point(0.0, base_bpm, TempoCurve::Hold);
            }
            self.add_or_update_point(beat, bpm, TempoCurve::Hold);
        } else {
            self.add_or_update_point(beat, bpm, TempoCurve::Hold);
        }
    }

    fn sort(&mut self) {
        self.points.sort_by(|a, b| {
            a.beat
                .partial_cmp(&b.beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Overwrite the marker list from an undo/redo snapshot, bumping the
    /// revision forward so downstream caches still invalidate.
    pub fn restore_points(&mut self, points: Vec<TempoPoint>) {
        self.points = points;
        self.bump_revision();
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// The resolved engine tempo map for one `(tempo_map, bpm)` pair.
///
/// Every audio clip's geometry resolves through the tempo map — where it
/// starts on the clock, how many beats its wall-clock length covers — and the
/// arrangement asks for that once per clip per frame. Building the map each
/// time made a frame cost `clips × tempo markers`, so adding markers to a
/// project slowed down scrolling in proportion to how many were added.
///
/// This is derived state, so it is deliberately outside the value: it is never
/// persisted, and [`PartialEq`] ignores it — two timelines with the same tempo
/// are the same timeline whether or not either has resolved it yet.
///
/// It refreshes itself on read rather than being refreshed by whoever last
/// edited the tempo. A cache that has to be remembered is a cache that will be
/// forgotten by exactly one code path, and the symptom — a project that grows
/// slower the more tempo markers it has — is invisible until someone measures
/// it. The key is checked on every read, so a stale build simply rebuilds.
#[derive(Debug, Default)]
pub struct ResolvedTempo {
    /// `None` until first use. Behind a lock because the accessor takes
    /// `&self`: it is read once per audio clip per frame from the UI thread,
    /// and an uncontended lock is nothing against building the map.
    cell: std::sync::Mutex<Option<(TempoCacheKey, std::sync::Arc<DirectAudio::TempoMap>)>>,
}

/// What a resolved map was built from: a hash of the base BPM and every
/// marker's beat, BPM, curve and tension.
type TempoCacheKey = u64;

thread_local! {
    /// Engine tempo maps actually built on this thread.
    ///
    /// The arrangement resolves an audio clip's geometry through the tempo map
    /// once per clip per frame, so counting builds against the *tempo* rather
    /// than the *clips* is the invariant the cache exists for — and a test can
    /// assert it without a stopwatch. Per thread, so a caller counts only the
    /// builds it caused.
    static TEMPO_MAPS_BUILT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// See [`TEMPO_MAPS_BUILT`].
pub fn tempo_maps_built() -> u64 {
    TEMPO_MAPS_BUILT.with(|built| built.get())
}

impl Clone for ResolvedTempo {
    /// A clone starts empty rather than copying the entry. Cloning a timeline
    /// is not a hot path, and carrying a cache into a clone that may be edited
    /// independently is how a cache and its subject part company.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for ResolvedTempo {
    /// Derived state is not part of the value.
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl TimelineState {
    /// A hash of everything the resolved map is built from. Content rather
    /// than the revision counter: `points` is public and a freshly built map
    /// restarts its revision, so a counter can repeat for a different map
    /// (open another project with as many markers) and serve a stale one.
    /// The marker list is small, so hashing it costs far less than a rebuild.
    pub(crate) fn tempo_cache_key(&self) -> TempoCacheKey {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.bpm.to_bits().hash(&mut hasher);
        for point in &self.tempo_map.points {
            point.beat.to_bits().hash(&mut hasher);
            point.bpm.to_bits().hash(&mut hasher);
            point.curve.to_tag().hash(&mut hasher);
            point.tension.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }

    /// The engine tempo map for the project's current tempo.
    ///
    /// One `Arc` clone when the cache is current, which is the state the
    /// arrangement reads it in — the map is otherwise rebuilt once per audio
    /// clip per frame, which is what made a project's frame cost scale with
    /// its tempo-marker count.
    pub fn resolved_tempo_map(&self) -> std::sync::Arc<DirectAudio::TempoMap> {
        let key = self.tempo_cache_key();
        let build = || {
            TEMPO_MAPS_BUILT.with(|built| built.set(built.get().saturating_add(1)));
            std::sync::Arc::new(self.tempo_map.to_engine_map(self.bpm.max(1.0) as f64))
        };
        // A poisoned lock is not a reason to be wrong: build and move on.
        let Ok(mut cell) = self.resolved_tempo.cell.lock() else {
            return build();
        };
        if let Some((cached_key, map)) = cell.as_ref() {
            if *cached_key == key {
                return std::sync::Arc::clone(map);
            }
        }
        let map = build();
        *cell = Some((key, std::sync::Arc::clone(&map)));
        map
    }

    /// Resolve the tempo map now, so the next read is a cache hit.
    ///
    /// Not required for correctness — [`Self::resolved_tempo_map`] refreshes
    /// itself — but calling it after a tempo edit keeps the rebuild off the
    /// first frame that draws with it.
    pub fn refresh_tempo_cache(&mut self) {
        let _ = self.resolved_tempo_map();
        self.sync_time_warp();
    }
}

/// BPM clamp range for tempo points (matches the audio engine spec).
pub const TEMPO_BPM_MIN: f64 = 20.0;

pub const TEMPO_BPM_MAX: f64 = 999.0;

/// Default expanded height for the global Tempo Track lane (px).
///
/// The conductor lanes are secondary utility rows: they say what the tempo and
/// meter are, not what the music is. At 72 the Tempo lane took more vertical
/// space than a whole compact track row, so it now sits in the same 40-44 band
/// as the other global lanes and gives the height back to the arrangement.
pub const TEMPO_TRACK_HEIGHT: f32 = 44.0;

/// Collapsed/minimal Tempo Track lane height (px).
pub const TEMPO_TRACK_HEIGHT_COLLAPSED: f32 = 32.0;

/// Vertical padding inside the tempo lane curve area (px).
pub const TEMPO_LANE_PAD: f32 = 5.0;

/// Two tempo points within this many beats are treated as the same slot.
pub const TEMPO_BEAT_EPSILON: f64 = 1e-6;

impl TimelineState {
    /// Effective BPM at a given beat, honoring tempo automation. Falls back to
    /// the static `bpm` when the tempo map has no markers.
    pub fn effective_bpm_at_beat(&self, beat: f64) -> f64 {
        self.tempo_map.bpm_at_beat(beat, self.bpm as f64)
    }

    /// Effective BPM at the current playhead position.
    pub fn effective_bpm_at_playhead(&self) -> f64 {
        self.effective_bpm_at_beat(self.transport.playhead_beats as f64)
    }

    /// Whether tempo automation is active (one or more markers present).
    pub fn tempo_has_automation(&self) -> bool {
        self.tempo_map.has_automation()
    }

    /// Height of the global Tempo Track lane when visible, else 0.
    pub fn tempo_track_height(&self) -> f32 {
        if !self.show_tempo_track {
            return 0.0;
        }
        self.global_lane_height(GlobalLaneKind::Tempo)
    }

    /// Replace the whole tempo state from an undo/redo snapshot.
    ///
    /// Restores the marker list *and* the fixed project BPM together: clearing
    /// automation folds the effective BPM back into `bpm`, so undoing it has to
    /// put both halves back. The map revision keeps moving forward (it is a
    /// cache-invalidation counter, not part of the value) and a selection that
    /// no longer names a live marker is dropped.
    pub fn restore_tempo_state(&mut self, points: Vec<TempoPoint>, bpm: f32) {
        // Where the Linear-timebase clips sit on the clock, read under the map
        // that is about to be replaced. Not carried in the snapshot for the same
        // reason clip lengths are not: it is a function of the position and the
        // tempo, and a second stored copy is how the two drift apart.
        let linear_anchors = self.capture_linear_clip_anchors();
        self.tempo_map.restore_points(points);
        self.bpm = bpm;
        // Audio clip lengths are derived from the tempo, so undoing the tempo
        // has to re-derive them. They are not carried in the snapshot for the
        // same reason: the value is a function of the source window and the
        // tempo, and storing a second copy is how the two drift apart.
        self.reconcile_audio_clip_lengths();
        // Linear tracks hold wall-clock time, so their clips move in beats to
        // stay where they were on the clock. Runs after the length pass, which
        // owns audio clip durations.
        self.reapply_linear_clip_anchors(&linear_anchors);
        if let Some(id) = self.selected_tempo_point_id.clone() {
            if !self.tempo_map.points.iter().any(|p| p.id == id) {
                self.selected_tempo_point_id = None;
            }
        }
    }

    /// Secondary label for the Tempo lane header (fixed BPM or automation range).
    pub fn tempo_lane_header_subtitle(&self) -> String {
        let bpm = self.effective_bpm_at_playhead();
        if self.tempo_map.points.len() <= 1 {
            if bpm.fract().abs() < 0.05 {
                format!("Fixed {:.0} BPM", bpm)
            } else {
                format!("Fixed {:.1} BPM", bpm)
            }
        } else {
            let mut min = bpm;
            let mut max = bpm;
            for p in &self.tempo_map.points {
                min = min.min(p.bpm);
                max = max.max(p.bpm);
            }
            if (max - min).abs() < 0.5 {
                if bpm.fract().abs() < 0.05 {
                    format!("{:.0} BPM", bpm)
                } else {
                    format!("{:.1} BPM", bpm)
                }
            } else {
                format!("{:.0}–{:.0} BPM", min.round(), max.round())
            }
        }
    }

    /// Scroll/zoom the arrangement so all tempo automation points are visible.
    pub fn fit_tempo_automation_in_view(&mut self) {
        if self.tempo_map.points.is_empty() {
            return;
        }
        let min_beat = self
            .tempo_map
            .points
            .iter()
            .map(|p| p.beat)
            .fold(f64::INFINITY, f64::min)
            .max(0.0);
        let max_beat = self
            .tempo_map
            .points
            .iter()
            .map(|p| p.beat)
            .fold(0.0, f64::max);
        let pad = 8.0;
        let span_beats = (max_beat - min_beat + pad * 2.0).max(16.0);
        let width = self.viewport.viewport_width.max(200.0);
        let needed_ppb = width / span_beats as f32;
        let current_ppb = self.pixels_per_beat().max(0.0001);
        if needed_ppb < current_ppb {
            let factor = (needed_ppb / current_ppb).clamp(0.05, 1.0);
            self.zoom_by(factor, width * 0.5);
        }
        let scroll = self
            .viewport
            .time_warp
            .content_x(
                (min_beat - pad).max(0.0),
                self.viewport.pixels_per_second,
                self.viewport.pixels_per_beat,
            )
            .max(0.0) as f32;
        self.viewport.scroll_x = scroll;
        self.viewport.target_scroll_x = scroll;
    }

    /// Auto-fit BPM range for the Tempo Track curve with padding.
    pub fn tempo_lane_bpm_range(&self) -> (f64, f64) {
        let mut min = self.bpm as f64;
        let mut max = self.bpm as f64;
        for p in &self.tempo_map.points {
            min = min.min(p.bpm);
            max = max.max(p.bpm);
        }
        let pad = ((max - min) * 0.15).max(10.0);
        let mut min_bpm = (min - pad).clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        let mut max_bpm = (max + pad).clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        if (max_bpm - min_bpm) < 20.0 {
            let mid = (min_bpm + max_bpm) * 0.5;
            min_bpm = (mid - 10.0).max(TEMPO_BPM_MIN);
            max_bpm = (mid + 10.0).min(TEMPO_BPM_MAX);
        }
        (min_bpm, max_bpm)
    }

    /// Show the Tempo Track lane and ensure at least one anchor point exists.
    pub fn show_tempo_track_lane(&mut self) {
        self.show_tempo_track = true;
        self.ensure_tempo_anchor_point();
    }

    pub fn hide_tempo_track_lane(&mut self) {
        self.show_tempo_track = false;
        self.selected_tempo_point_id = None;
    }

    /// Seed beat-0 marker when the map is empty so the lane always has data.
    pub fn ensure_tempo_anchor_point(&mut self) {
        if self.tempo_map.points.is_empty() {
            let bpm = self.bpm as f64;
            self.tempo_map
                .add_or_update_point(0.0, bpm, TempoCurve::Hold);
        }
        self.tempo_map.ensure_point_ids();
    }

    pub fn select_tempo_point(&mut self, id: &str) {
        self.selected_tempo_point_id = Some(id.to_string());
    }

    pub fn clear_tempo_point_selection(&mut self) {
        self.selected_tempo_point_id = None;
    }

    pub fn tempo_point_at(
        &self,
        beat: f64,
        bpm: f64,
        beat_tol: f64,
        bpm_tol: f64,
    ) -> Option<String> {
        self.tempo_map
            .points
            .iter()
            .find(|p| (p.beat - beat).abs() <= beat_tol && (p.bpm - bpm).abs() <= bpm_tol)
            .map(|p| p.id.clone())
    }

    pub fn add_tempo_point(&mut self, beat: f64, bpm: f64) -> Option<String> {
        let beat = beat.max(0.0);
        let bpm = bpm.clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX);
        if self.tempo_map.points.is_empty() && beat > TEMPO_BEAT_EPSILON {
            self.tempo_map
                .add_or_update_point(0.0, self.bpm as f64, TempoCurve::Hold);
        }
        self.tempo_map
            .add_or_update_point(beat, bpm, TempoCurve::Hold);
        self.tempo_map.ensure_point_ids();
        self.tempo_map
            .points
            .iter()
            .find(|p| (p.beat - beat).abs() < TEMPO_BEAT_EPSILON)
            .map(|p| p.id.clone())
    }

    pub fn move_tempo_point(&mut self, id: &str, beat: f64, bpm: f64) -> bool {
        self.tempo_map.move_point_by_id(id, beat, bpm)
    }

    pub fn delete_tempo_point(&mut self, id: &str) -> bool {
        if self.tempo_map.remove_point_by_id(id) {
            if self.selected_tempo_point_id.as_deref() == Some(id) {
                self.selected_tempo_point_id = None;
            }
            true
        } else {
            false
        }
    }

    pub fn set_tempo_point_curve(&mut self, id: &str, curve: TempoCurve) -> bool {
        self.tempo_map.update_point_curve_by_id(id, curve)
    }

    pub fn set_tempo_point_tension(&mut self, id: &str, tension: f32) -> bool {
        self.tempo_map.update_point_tension_by_id(id, tension)
    }

    pub fn set_fixed_tempo_from_beat(&mut self, beat: f64, bpm: f64) {
        let base = self.bpm as f64;
        self.tempo_map.set_fixed_from_beat(beat, bpm, base);
        self.tempo_map.ensure_point_ids();
    }

    /// BPM values rendered as tempo-track point handles (for tests/debug).
    pub fn tempo_track_render_bpm_values(&self) -> Vec<f64> {
        if self.tempo_map.points.is_empty() {
            vec![self.bpm as f64]
        } else {
            self.tempo_map.points.iter().map(|p| p.bpm).collect()
        }
    }

    /// Effective BPM across the visible beat range (flat line check for tests).
    pub fn tempo_track_bpm_samples(&self, viewport_width: f32) -> Vec<f64> {
        let (start, end) = self.visible_beat_range(viewport_width);
        let cols = viewport_width.ceil().max(1.0) as usize;
        (0..=cols)
            .map(|col| {
                let beat = start as f64 + (end - start) as f64 * (col as f64 / cols as f64);
                self.effective_bpm_at_beat(beat)
            })
            .collect()
    }
}

#[cfg(test)]
mod curve_tests {
    use super::*;

    fn drag(rising: bool) -> TempoCurveDrag {
        TempoCurveDrag {
            left_id: "a".into(),
            start_tension: 0.0,
            start_window_y: 100.0,
            rising,
            changed: false,
        }
    }

    #[test]
    fn dragging_up_lifts_the_line_either_way() {
        // Rising ramp: lifting the line means reaching speed early (ease-out).
        assert!(tempo_drag_tension(&drag(true), 60.0, 80.0, false) < 0.0);
        // Falling ramp: lifting the line means staying fast longer (ease-in).
        assert!(tempo_drag_tension(&drag(false), 60.0, 80.0, false) > 0.0);
        // Fine drag moves less, and the result stays in range.
        let coarse = tempo_drag_tension(&drag(true), 90.0, 80.0, false);
        let fine = tempo_drag_tension(&drag(true), 90.0, 80.0, true);
        assert!(fine.abs() < coarse.abs());
        assert_eq!(
            tempo_drag_tension(&drag(true), -10_000.0, 80.0, false),
            -1.0
        );
    }

    #[test]
    fn the_lane_draws_the_bend_the_engine_plays() {
        let map = TempoMap::with_points(vec![
            TempoPoint::with_id("a", 0.0, 60.0, TempoCurve::Linear).with_tension(0.6),
            TempoPoint::with_id("b", 8.0, 180.0, TempoCurve::Hold),
        ]);
        let engine = map.to_engine_map(120.0);
        for beat in [1.0, 4.0, 7.0] {
            let drawn = map.bpm_at_beat(beat, 120.0);
            let played = engine.bpm_at_beat(beat);
            assert!(
                (drawn - played).abs() < 0.05,
                "beat {beat}: {drawn} vs {played}"
            );
        }
        // Eased in: still well under the straight ramp's 120 at the midpoint.
        assert!(map.bpm_at_beat(4.0, 120.0) < 110.0);
    }

    #[test]
    fn the_resolved_map_never_serves_a_stale_curve() {
        let mut state = TimelineState::default();
        state.tempo_map = TempoMap::with_points(vec![
            TempoPoint::with_id("a", 0.0, 60.0, TempoCurve::Linear),
            TempoPoint::with_id("b", 8.0, 180.0, TempoCurve::Hold),
        ]);
        let straight = state.seconds_at_beat(8.0);
        // Written straight into the public field: no revision bump.
        state.tempo_map.points[0].tension = 0.8;
        let bent = state.seconds_at_beat(8.0);
        assert!(bent > straight + 0.1, "{straight} → {bent}");
    }
}
