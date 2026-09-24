use super::*;

/// `FUTUREBOARD_AUTOSCROLL_DEBUG=1` — trace follow-playhead scrolling.
///
/// Cached: in Continuous mode this is on the playback tick, so an uncached
/// `var_os` here was an environment-block lookup per frame.
fn autoscroll_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_AUTOSCROLL_DEBUG").is_some())
}

/// Arrangement overview zoom floor. At 120 BPM this is 0.25 px/beat, enough to
/// fit very long MIDI songs on screen while the grid LOD thins bar lines.
const TIMELINE_MIN_PIXELS_PER_SECOND: f32 = 0.5;
const TIMELINE_MAX_PIXELS_PER_SECOND: f32 = 4000.0;

/// Playback follow / auto-scroll behavior for the timeline viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoScrollMode {
    /// Never auto-scroll. User controls the viewport entirely.
    Off,
    /// When the playhead reaches the right edge, page forward by ~90% of
    /// the viewport width so the playhead lands near the left side again.
    /// Cheap, predictable, and friendly to low-end GPUs.
    Page,
    /// Keep the playhead at a roughly fixed fraction of the viewport while
    /// playing. Smoother, but more scroll churn — left as an opt-in.
    Continuous,
}

impl Default for AutoScrollMode {
    fn default() -> Self {
        AutoScrollMode::Page
    }
}

/// Fraction of the viewport width at which `Continuous` mode pins the playhead.
/// 0.5 keeps it centered so there is equal lookahead/look-behind context.
const CONTINUOUS_PIN_FRACTION: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineTool {
    Pointer,
    Pen,
    Cut,
    Glue,
    Mute,
    Time,
    Automation,
}

/// How the arrangement's x axis bends with tempo.
///
/// The arrangement is laid out in **real time**: content x is elapsed seconds
/// × `pixels_per_second`. With a constant tempo that is exactly
/// `beat × pixels_per_beat`, so nothing moves; with tempo automation a bar
/// played at 90 BPM is wider than one at 120 and one at 150 narrower, the way
/// it sounds. The warp is the project's resolved engine tempo map — the same
/// one the transport plays — or `None` for a constant tempo, which keeps the
/// old linear arithmetic bit for bit.
#[derive(Debug, Clone, Default)]
pub struct TimeWarp {
    map: Option<std::sync::Arc<DirectAudio::TempoMap>>,
    /// Hash of what `map` was built from (see `TimelineState::tempo_cache_key`).
    key: u64,
}

impl PartialEq for TimeWarp {
    /// By content key: two warps built from the same tempo state are equal,
    /// without comparing their segment lists.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.map.is_some() == other.map.is_some()
    }
}

impl TimeWarp {
    pub fn new(map: std::sync::Arc<DirectAudio::TempoMap>, key: u64) -> Self {
        Self {
            map: Some(map),
            key,
        }
    }

    /// `true` for a constant tempo: x is linear in beats.
    pub fn is_linear(&self) -> bool {
        self.map.is_none()
    }

    /// Stable identity for caches keyed on geometry.
    pub fn key(&self) -> u64 {
        if self.map.is_some() {
            self.key
        } else {
            0
        }
    }

    /// Unscrolled content x of `beat`.
    #[inline]
    pub fn content_x(&self, beat: f64, pixels_per_second: f32, pixels_per_beat: f32) -> f64 {
        match &self.map {
            Some(map) => map.seconds_at_beat(beat.max(0.0)) * pixels_per_second as f64,
            None => beat.max(0.0) * pixels_per_beat as f64,
        }
    }

    /// Beat at unscrolled content x. Inverse of [`Self::content_x`].
    #[inline]
    pub fn beat_at_content_x(&self, x: f64, pixels_per_second: f32, pixels_per_beat: f32) -> f64 {
        match &self.map {
            Some(map) => map.beat_at_seconds((x / pixels_per_second.max(0.0001) as f64).max(0.0)),
            None => (x / pixels_per_beat.max(0.0001) as f64).max(0.0),
        }
    }

    /// Clip-local x of a clip-local beat, for content drawn inside a clip
    /// that starts on `clip_start`. Linear warps reduce to `local × ppb`.
    #[inline]
    pub fn local_x(
        &self,
        clip_start: f64,
        local_beat: f64,
        pixels_per_second: f32,
        pixels_per_beat: f32,
    ) -> f32 {
        match &self.map {
            Some(_) => {
                (self.content_x(clip_start + local_beat, pixels_per_second, pixels_per_beat)
                    - self.content_x(clip_start, pixels_per_second, pixels_per_beat))
                    as f32
            }
            None => local_beat as f32 * pixels_per_beat,
        }
    }

    /// Clip-local beat at a clip-local x. Inverse of [`Self::local_x`].
    #[inline]
    pub fn local_beat(
        &self,
        clip_start: f64,
        local_x: f32,
        pixels_per_second: f32,
        pixels_per_beat: f32,
    ) -> f32 {
        match &self.map {
            Some(_) => {
                let origin = self.content_x(clip_start, pixels_per_second, pixels_per_beat);
                (self.beat_at_content_x(
                    origin + local_x as f64,
                    pixels_per_second,
                    pixels_per_beat,
                ) - clip_start) as f32
            }
            None => local_x / pixels_per_beat.max(0.0001),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineViewport {
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub target_scroll_x: f32,
    pub target_scroll_y: f32,
    pub pixels_per_second: f32,
    pub pixels_per_beat: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub track_area_height: f32,
    /// Window-space x of the timeline panel's left edge, i.e. the width of the
    /// chrome to its left (the collapsible browser panel). Pushed by the shell
    /// through `Timeline::set_chrome_metrics`; never a fixed constant, because
    /// hiding the browser panel slides the whole timeline left. Every
    /// window-x → beat mapping must go through
    /// [`TimelineState::lane_origin_x`] so gestures and drawing stay on one
    /// coordinate space.
    pub panel_origin_x: f32,
    /// Measured window x of the lane content column's left edge, published by
    /// the ruler's origin probe each frame.
    ///
    /// [`Self::panel_origin_x`] is derived from chrome constants and is only an
    /// estimate: it assumes the browser panel is either exactly its design
    /// width or absent, and it does not know about the window's left rail at
    /// all. Pointer math reads this instead whenever it is available, so the
    /// transform that resolves a click is the one that drew the pixel.
    pub lane_origin_x_measured: Option<f32>,
    /// Real-time warp of the x axis; see [`TimeWarp`]. Kept current by
    /// [`TimelineState::sync_time_warp`].
    pub time_warp: TimeWarp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackLayout {
    pub track_ids: Vec<TrackId>,
    /// Uniform fallback height for legacy callers.
    pub track_height: f32,
    pub scroll_y: f32,
    pub rows: Vec<TrackRowLayoutEntry>,
    pub total_height: f32,
}

impl TrackLayout {
    pub fn from_state(state: &TimelineState) -> Self {
        let layout = state.track_row_layout();
        Self {
            track_ids: layout.rows.iter().map(|r| r.track_id.clone()).collect(),
            track_height: DEFAULT_TRACK_HEIGHT,
            scroll_y: state.viewport.scroll_y,
            rows: layout.rows,
            total_height: layout.total_height,
        }
    }

    pub fn from_tracks(tracks: &[TrackState], scroll_y: f32) -> Self {
        Self {
            track_ids: tracks.iter().map(|track| track.id.clone()).collect(),
            track_height: DEFAULT_TRACK_HEIGHT,
            scroll_y,
            rows: tracks
                .iter()
                .enumerate()
                .scan(0.0_f32, |y, (index, track)| {
                    let entry = TrackRowLayoutEntry {
                        track_id: track.id.clone(),
                        index,
                        y: *y,
                        height: DEFAULT_TRACK_HEIGHT,
                        automation_height: 0.0,
                    };
                    *y += DEFAULT_TRACK_HEIGHT;
                    Some(entry)
                })
                .collect(),
            total_height: tracks.len() as f32 * DEFAULT_TRACK_HEIGHT,
        }
    }

    pub fn row_for_track(&self, track_id: &str) -> Option<&TrackRowLayoutEntry> {
        self.rows.iter().find(|row| row.track_id == track_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapSettings {
    pub enabled: bool,
    pub division: SnapDivision,
    pub shape: SnapShape,
    pub beats_per_bar: f64,
    pub auto_step_beats: f64,
}

impl SnapSettings {
    pub fn from_timeline(state: &TimelineState) -> Self {
        // A time-based timebase draws a clock grid instead of a musical one, so
        // snapping has to follow it: a clip must land on the line the user is
        // looking at. The step comes from the same resolver the grid uses, so
        // the two cannot disagree.
        //
        // Dotted/triplet are musical shapes with no meaning on a clock grid and
        // are dropped rather than silently scaling a second by 1.5.
        if state.time_display_format.is_time_based() {
            let step_seconds = state.time_grid_step().minor.max(1.0e-6);
            let step_beats = (step_seconds * state.bpm.max(1.0) as f64 / 60.0).max(1.0e-6);
            return Self {
                enabled: state.snap_to_grid,
                division: SnapDivision::Auto,
                shape: SnapShape::Straight,
                beats_per_bar: state.beats_per_bar() as f64,
                auto_step_beats: step_beats,
            };
        }
        Self {
            enabled: state.snap_to_grid,
            division: state.grid_division,
            shape: state.snap_shape,
            beats_per_bar: state.beats_per_bar() as f64,
            auto_step_beats: state.get_grid_sub_beats(state.pixels_per_beat()) as f64,
        }
    }

    pub fn to_musical(self) -> MusicalSnap {
        MusicalSnap {
            enabled: self.enabled,
            division: self.division,
            shape: self.shape,
            beats_per_bar: self.beats_per_bar,
            auto_step_beats: self.auto_step_beats,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapDivision {
    Auto,
    Off,
    Bar1,
    Div1_1,
    Div1_2,
    Div1_4,
    Div1_8,
    Div1_16,
    Div1_32,
    Div1_64,
}

impl SnapDivision {
    pub fn label(&self) -> &'static str {
        match self {
            SnapDivision::Auto => "Auto",
            SnapDivision::Off => "Off",
            SnapDivision::Bar1 => "1 bar",
            SnapDivision::Div1_1 => "1/1",
            SnapDivision::Div1_2 => "1/2",
            SnapDivision::Div1_4 => "1/4",
            SnapDivision::Div1_8 => "1/8",
            SnapDivision::Div1_16 => "1/16",
            SnapDivision::Div1_32 => "1/32",
            SnapDivision::Div1_64 => "1/64",
        }
    }

    /// Label including straight / dotted / triplet shaping.
    pub fn label_with_shape(&self, shape: SnapShape) -> String {
        let base = self.label();
        match shape {
            SnapShape::Straight => base.to_string(),
            SnapShape::Dotted => format!("{base}."),
            SnapShape::Triplet => format!("{base}T"),
        }
    }

    pub fn step_beats(&self, bpb: f32) -> f32 {
        match self {
            SnapDivision::Auto => 0.0,
            SnapDivision::Off => 0.0,
            SnapDivision::Bar1 => bpb,
            SnapDivision::Div1_1 => 4.0,
            SnapDivision::Div1_2 => 2.0,
            SnapDivision::Div1_4 => 1.0,
            SnapDivision::Div1_8 => 0.5,
            SnapDivision::Div1_16 => 0.25,
            SnapDivision::Div1_32 => 0.125,
            SnapDivision::Div1_64 => 0.0625,
        }
    }
}

impl TimelineState {
    pub fn pixels_per_beat(&self) -> f32 {
        self.viewport.pixels_per_second * self.seconds_per_beat()
    }

    pub(crate) fn sync_pixels_per_beat(&mut self) {
        self.viewport.pixels_per_beat = self.pixels_per_beat();
        self.sync_time_warp();
    }

    /// Bring the x-axis warp in line with the tempo map. Cheap when nothing
    /// changed (a hash of the marker list); the map itself is the cached
    /// resolved one the transport reads.
    pub fn sync_time_warp(&mut self) {
        if self.tempo_map.points.is_empty() {
            if !self.viewport.time_warp.is_linear() {
                self.viewport.time_warp = TimeWarp::default();
            }
            return;
        }
        let key = self.tempo_cache_key();
        if self.viewport.time_warp.key() == key && !self.viewport.time_warp.is_linear() {
            return;
        }
        self.viewport.time_warp = TimeWarp::new(self.resolved_tempo_map(), key);
    }

    /// Width in px of `duration` beats starting at `start`, through the warp.
    pub fn beat_span_px(&self, start: f32, duration: f32) -> f32 {
        let v = &self.viewport;
        (v.time_warp.content_x(
            (start + duration.max(0.0)) as f64,
            v.pixels_per_second,
            v.pixels_per_beat,
        ) - v
            .time_warp
            .content_x(start as f64, v.pixels_per_second, v.pixels_per_beat)) as f32
    }

    pub fn update_viewport_size(&mut self, width: f32, height: f32) {
        self.viewport.viewport_width = width.max(0.0);
        self.viewport.viewport_height = height.max(0.0);
        self.viewport.track_area_height = height.max(0.0);
        self.sync_pixels_per_beat();
    }

    pub fn get_visible_beat_range(&self, width: f32) -> (f32, f32) {
        let start = self.x_to_beats(0.0);
        let end = self.x_to_beats(width);
        (start, end)
    }

    pub fn visible_beat_range(&self, viewport_width: f32) -> (f32, f32) {
        self.get_visible_beat_range(viewport_width)
    }

    /// Multiplicative zoom around a content-space x anchor. Updates
    /// `pixels_per_second` and shifts `scroll_x` so the time under `anchor_x`
    /// (a screen-space x inside the track lane, already net of header) stays
    /// fixed under the cursor. Smooth — accepts arbitrary positive `factor`.
    pub fn zoom_by(&mut self, factor: f32, anchor_x: f32) {
        let factor = factor.max(0.0001);
        let old_pps = self.viewport.pixels_per_second.max(0.0001);
        let new_pps = (old_pps * factor).clamp(
            TIMELINE_MIN_PIXELS_PER_SECOND,
            TIMELINE_MAX_PIXELS_PER_SECOND,
        );
        if (new_pps - old_pps).abs() < 0.0001 {
            return;
        }
        // Time under anchor before the change.
        let anchor_time = (anchor_x + self.viewport.scroll_x) / old_pps;
        self.viewport.pixels_per_second = new_pps;
        self.sync_pixels_per_beat();
        // Re-solve scroll_x so the same anchor_time lands under anchor_x.
        let new_scroll = (anchor_time * new_pps - anchor_x).max(0.0);
        self.viewport.scroll_x = new_scroll;
        self.viewport.target_scroll_x = new_scroll;
    }

    pub fn scroll_by(&mut self, delta_x: f32, delta_y: f32, max_x: f32, max_y: f32) {
        self.viewport.target_scroll_x =
            (self.viewport.target_scroll_x + delta_x).clamp(0.0, max_x.max(0.0));
        self.viewport.target_scroll_y =
            (self.viewport.target_scroll_y + delta_y).clamp(0.0, max_y.max(0.0));
        self.smooth_scroll_towards_target();
    }

    pub fn set_scroll_immediate(&mut self, x: f32, y: f32, max_x: f32, max_y: f32) {
        self.viewport.scroll_x = x.clamp(0.0, max_x.max(0.0));
        self.viewport.scroll_y = y.clamp(0.0, max_y.max(0.0));
        self.viewport.target_scroll_x = self.viewport.scroll_x;
        self.viewport.target_scroll_y = self.viewport.scroll_y;
    }

    pub fn clamp_scroll(&mut self, max_x: f32, max_y: f32) {
        let max_x = max_x.max(0.0);
        let max_y = max_y.max(0.0);
        self.viewport.scroll_x = self.viewport.scroll_x.clamp(0.0, max_x);
        self.viewport.scroll_y = self.viewport.scroll_y.clamp(0.0, max_y);
        self.viewport.target_scroll_x = self.viewport.target_scroll_x.clamp(0.0, max_x);
        self.viewport.target_scroll_y = self.viewport.target_scroll_y.clamp(0.0, max_y);
    }

    /// Keep the playhead inside the visible viewport while playing.
    /// Returns true when the viewport scrolled (caller should `cx.notify`).
    /// Cheap — no allocation, just a couple of float comparisons.
    pub fn update_auto_scroll_for_playhead(&mut self, playhead_beats: f32) -> bool {
        if !self.follow_playhead || self.auto_scroll_mode == AutoScrollMode::Off {
            return false;
        }
        let viewport_width = self.viewport.viewport_width;
        if viewport_width <= 1.0 {
            return false;
        }
        let playhead_content_x = self.viewport.time_warp.content_x(
            playhead_beats.max(0.0) as f64,
            self.viewport.pixels_per_second,
            self.viewport.pixels_per_beat,
        ) as f32;
        let scroll_x = self.viewport.scroll_x;

        // Page mode pages forward once the playhead nears the right edge.
        let right_trigger = scroll_x + viewport_width * 0.85;

        let new_scroll_x = match self.auto_scroll_mode {
            AutoScrollMode::Off => return false,
            AutoScrollMode::Page => {
                if playhead_content_x > right_trigger {
                    // Page forward so the playhead lands ~10% into the new viewport.
                    (playhead_content_x - viewport_width * 0.10).max(0.0)
                } else if playhead_content_x < scroll_x {
                    // Playhead seeked or wrapped back behind the viewport — recenter.
                    (playhead_content_x - viewport_width * 0.10).max(0.0)
                } else {
                    return false;
                }
            }
            AutoScrollMode::Continuous => {
                // Pin the playhead at a fixed fraction of the viewport and let
                // the timeline flow underneath it. Recomputed every playback
                // tick, so the playhead stays locked in place while the content
                // scrolls smoothly. Before the playhead reaches the pin point
                // (early in the project) `max(0.0)` holds scroll at 0 so it
                // travels in from the left edge first.
                (playhead_content_x - viewport_width * CONTINUOUS_PIN_FRACTION).max(0.0)
            }
        };

        if (new_scroll_x - scroll_x).abs() < 0.5 {
            return false;
        }
        if autoscroll_debug_enabled() {
            eprintln!(
                "[autoscroll] playhead_x={:.1} viewport=[{:.1}..{:.1}] scroll_x: {:.1} -> {:.1} mode={:?}",
                playhead_content_x,
                scroll_x,
                scroll_x + viewport_width,
                scroll_x,
                new_scroll_x,
                self.auto_scroll_mode
            );
        }
        self.viewport.scroll_x = new_scroll_x;
        self.viewport.target_scroll_x = new_scroll_x;
        true
    }

    /// Called when the user manually scrolls/drags the viewport — temporarily
    /// disables follow-playhead so playback won't fight the user. Re-enable
    /// by toggling the Follow control.
    pub fn note_user_scrolled(&mut self) {
        if self.follow_playhead {
            self.follow_playhead = false;
        }
    }

    pub fn set_follow_playhead(&mut self, follow: bool) {
        self.follow_playhead = follow;
    }

    pub fn set_auto_scroll_mode(&mut self, mode: AutoScrollMode) {
        self.auto_scroll_mode = mode;
    }

    /// Switch between paged and continuous auto-scroll, returning the new mode.
    /// `Off` maps to `Continuous` so the toggle always lands on a visible mode.
    /// The caller decides whether to also enable `follow_playhead` so the change
    /// takes effect immediately.
    pub fn toggle_auto_scroll_mode(&mut self) -> AutoScrollMode {
        self.auto_scroll_mode = match self.auto_scroll_mode {
            AutoScrollMode::Continuous => AutoScrollMode::Page,
            _ => AutoScrollMode::Continuous,
        };
        self.auto_scroll_mode
    }

    pub fn smooth_scroll_towards_target(&mut self) -> bool {
        let dx = self.viewport.target_scroll_x - self.viewport.scroll_x;
        let dy = self.viewport.target_scroll_y - self.viewport.scroll_y;
        if dx.abs() < 0.35 && dy.abs() < 0.35 {
            self.viewport.scroll_x = self.viewport.target_scroll_x;
            self.viewport.scroll_y = self.viewport.target_scroll_y;
            return false;
        }
        self.viewport.scroll_x += dx * 0.42;
        self.viewport.scroll_y += dy * 0.42;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn following_view_recenters_when_a_seek_moves_behind_it() {
        let mut state = TimelineState::default();
        state.update_viewport_size(1000.0, 400.0);
        state.set_scroll_immediate(1000.0, 0.0, 5000.0, 0.0);

        assert!(state.update_auto_scroll_for_playhead(0.0));
        assert_eq!(state.viewport.scroll_x, 0.0);
        assert_eq!(state.viewport.target_scroll_x, 0.0);
    }
}

#[cfg(test)]
mod time_warp_tests {
    use super::*;

    /// 120 BPM for bar 1, 90 for bar 2, 150 from bar 3, 4/4, no ramps.
    fn warped() -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.viewport.pixels_per_second = 100.0;
        state.tempo_map = TempoMap::with_points(vec![
            TempoPoint::with_id("a", 0.0, 120.0, TempoCurve::Hold),
            TempoPoint::with_id("b", 4.0, 90.0, TempoCurve::Hold),
            TempoPoint::with_id("c", 8.0, 150.0, TempoCurve::Hold),
        ]);
        state.sync_pixels_per_beat();
        state
    }

    #[test]
    fn a_constant_tempo_keeps_the_linear_layout() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_second = 100.0;
        state.sync_pixels_per_beat();
        assert!(state.viewport.time_warp.is_linear());
        let ppb = state.viewport.pixels_per_beat;
        assert_eq!(state.beats_to_x(8.0), (8.0 * ppb).round());
    }

    #[test]
    fn slow_bars_stretch_and_fast_bars_shrink() {
        let state = warped();
        assert!(!state.viewport.time_warp.is_linear());
        let bar = |n: f32| state.beats_to_x(4.0 * (n + 1.0)) - state.beats_to_x(4.0 * n);
        let normal = bar(0.0);
        // 120 BPM at 100 px/s: 2 s per bar.
        assert!((normal - 200.0).abs() <= 1.0, "{normal}");
        // 90 BPM: 120/90 as wide.
        assert!(
            (bar(1.0) / normal - 120.0 / 90.0).abs() < 0.01,
            "{}",
            bar(1.0)
        );
        // 150 BPM: 120/150 as wide.
        assert!(
            (bar(2.0) / normal - 120.0 / 150.0).abs() < 0.01,
            "{}",
            bar(2.0)
        );
    }

    #[test]
    fn x_to_beat_inverts_the_warp() {
        let state = warped();
        for beat in [0.0f32, 2.5, 4.0, 6.25, 9.0, 13.5] {
            let x = state.beats_to_x(beat);
            let back = state.x_to_beats(x);
            // One px of rounding in the forward direction.
            assert!((back - beat).abs() < 0.02, "{beat} -> {x} -> {back}");
        }
    }

    #[test]
    fn a_clip_is_as_wide_as_the_time_it_spans() {
        let state = warped();
        // Beats 4..8 play at 90 BPM: 8/3 s = 266.7 px.
        let width = state.beat_span_px(4.0, 4.0);
        assert!((width - 800.0 / 3.0).abs() < 1.0, "{width}");
        assert!(state.densest_pixels_per_beat(0.0, 12.0) < state.pixels_per_beat());
    }

    #[test]
    fn notes_inside_a_clip_land_on_the_grid() {
        use crate::components::timeline::render::clip_geometry::ClipAxis;
        let state = warped();
        let clip_start = 2.0f32;
        let axis = ClipAxis::new(
            state.viewport.time_warp.clone(),
            clip_start,
            state.viewport.pixels_per_second,
            state.viewport.pixels_per_beat,
        );
        let left = state.beats_to_x(clip_start);
        for local in [0.0f32, 1.5, 2.0, 5.0, 7.75] {
            let arranged = state.beats_to_x(clip_start + local) - left;
            assert!((axis.x(local) - arranged).abs() <= 1.0, "{local}");
            assert!((axis.beat(axis.x(local)) - local).abs() < 1e-3);
        }
    }
}
