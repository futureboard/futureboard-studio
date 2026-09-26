use super::*;

/// Lane x of `beat`. The arrangement's one beat → x transform; it follows
/// the tempo map through [`TimeWarp`] (real-time layout).
pub fn beat_to_x(beat: f64, viewport: &TimelineViewport) -> f32 {
    if viewport.time_warp.is_linear() {
        return ((beat.max(0.0) as f32) * viewport.pixels_per_beat - viewport.scroll_x).round();
    }
    (viewport
        .time_warp
        .content_x(beat, viewport.pixels_per_second, viewport.pixels_per_beat) as f32
        - viewport.scroll_x)
        .round()
}

/// Beat at lane x. Inverse of [`beat_to_x`].
pub fn x_to_beat(x: f32, viewport: &TimelineViewport) -> f64 {
    if viewport.time_warp.is_linear() {
        return ((x + viewport.scroll_x) / viewport.pixels_per_beat.max(0.0001)).max(0.0) as f64;
    }
    viewport.time_warp.beat_at_content_x(
        (x + viewport.scroll_x) as f64,
        viewport.pixels_per_second,
        viewport.pixels_per_beat,
    )
}

/// Window-space x of the lane content column's left edge.
///
/// Prefers the value measured from the rendered ruler over the chrome-derived
/// estimate. The estimate is `browser_width + HEADER_WIDTH`, which is only
/// right when the browser panel happens to be exactly its design width and the
/// window has no left rail — neither holds in the shipped shell, which is why
/// clicks used to land a rail's width away from where they were drawn.
pub fn lane_origin_x(viewport: &TimelineViewport) -> f32 {
    viewport
        .lane_origin_x_measured
        .unwrap_or(viewport.panel_origin_x + HEADER_WIDTH)
}

/// Window-space y of the timeline's top edge: the ruler's top, which every
/// arrangement y (conductor lanes, track rows, automation) is measured from.
///
/// Prefers the value measured from the rendered timeline root. The fallback,
/// [`crate::shell_metrics::APP_CHROME_HEIGHT`], is a hand-tuned constant a few
/// pixels taller than the chrome actually drawn above the timeline; it only
/// stands in until the first frame has been measured.
pub fn timeline_origin_y(viewport: &TimelineViewport) -> f32 {
    viewport
        .timeline_origin_y_measured
        .unwrap_or(crate::shell_metrics::APP_CHROME_HEIGHT)
}

/// Vertical inset of a clip inside its track row, above and below.
pub const CLIP_LANE_PAD: f32 = 7.0;

/// Narrowest a clip is ever drawn, however short it is, so it stays visible
/// and grabbable when zoomed out.
pub const CLIP_MIN_DRAWN_WIDTH: f32 = 10.0;

/// The band every clip in a lane row `row_height` tall is drawn in, as
/// `(top, height)` from the row's top: the pad bands above and below are
/// excluded. [`TimelineState::clip_lane_rect`] places each clip in it, and
/// the marquee tests it once per row.
pub fn clip_lane_band(row_height: f32) -> (f32, f32) {
    (CLIP_LANE_PAD, row_height - CLIP_LANE_PAD * 2.0)
}

/// Where a clip is drawn inside its lane row. `left` is lane x (the
/// arrangement's beat transform, horizontal scroll included); `top` is
/// row-local.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipLaneRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl ClipLaneRect {
    pub fn right(&self) -> f32 {
        self.left + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.top + self.height
    }
}

pub fn snap_beat(beat: f64, snap: SnapSettings) -> f64 {
    // Arrangement clips historically clamp to ≥ 0; pre-roll-capable callers
    // should use [`super::musical_snap::snap_beat`] directly.
    super::musical_snap::snap_beat(beat, snap.to_musical(), false).max(0.0)
}

/// Snap a beat against `snap`, resolving bar length from the meter marker in
/// force *at that beat* rather than at the playhead.
///
/// Shared by [`TimelineState::snap_beats_with_bypass`] and
/// [`TimelineGestureContext`] so a gesture closure snaps identically whether it
/// captured the full state or only this frame's geometry.
pub fn snap_beat_against_meter(
    beats: f32,
    snap: SnapSettings,
    time_signature_map: &TimeSignatureMap,
    bypass: bool,
) -> f32 {
    let mut snap = snap;
    snap.beats_per_bar = time_signature_map.beats_per_bar_at_beat(beats as f64);
    super::musical_snap::snap_beat(beats as f64, snap.to_musical(), bypass) as f32
}

/// Snap a wall-clock second offset to the current grid. Shared by
/// [`TimelineState::snap_time`] and [`TimelineGestureContext::snap_time`].
pub fn snap_seconds(seconds: f32, seconds_per_beat: f32, snap: SnapSettings) -> f32 {
    if !snap.enabled || snap.division == SnapDivision::Off {
        return seconds;
    }
    let beats_per_bar = snap.beats_per_bar as f32;
    let sub_div = match snap.division {
        SnapDivision::Auto => snap.auto_step_beats as f32,
        SnapDivision::Bar1 => beats_per_bar,
        other => other.step_beats(beats_per_bar),
    };
    if sub_div <= 0.0 {
        return seconds;
    }
    let spb = seconds_per_beat.max(1.0e-6);
    let total_beats = seconds / spb;
    ((total_beats / sub_div).round() * sub_div * spb).max(0.0)
}

/// Per-frame coordinate + snap inputs for pointer gestures.
///
/// GPUI event closures must be `'static`, so a lane / clip / automation / ruler
/// handler cannot borrow `TimelineState` — it has to own what it reads. Owning
/// it by `state.clone()` deep-copies every track, clip, MIDI note, controller
/// lane, and plugin chain in the project, **per rendered row, per frame**; on a
/// dense arrangement that alone dominates the frame budget.
///
/// This carries only what a gesture actually resolves — the viewport transform,
/// the snap grid, and the meter map — so cloning it is O(meter markers) instead
/// of O(project). Build it once per repaint (see `Timeline::render`) and share
/// it with `Rc`.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineGestureContext {
    pub viewport: TimelineViewport,
    pub bpm: f32,
    pub snap: SnapSettings,
    pub time_signature_map: TimeSignatureMap,
    /// Precomputed [`TimelineState::arrangement_content_top`].
    pub content_top: f32,
}

impl TimelineGestureContext {
    pub fn from_state(state: &TimelineState) -> Self {
        Self {
            viewport: state.viewport.clone(),
            bpm: state.bpm,
            snap: SnapSettings::from_timeline(state),
            time_signature_map: state.time_signature_map.clone(),
            content_top: state.arrangement_content_top(),
        }
    }

    pub fn seconds_per_beat(&self) -> f32 {
        60.0 / self.bpm.max(1.0)
    }

    pub fn beats_to_x(&self, beats: f32) -> f32 {
        beat_to_x(beats as f64, &self.viewport)
    }

    pub fn x_to_beats(&self, x: f32) -> f32 {
        x_to_beat(x, &self.viewport) as f32
    }

    pub fn x_to_beat(&self, x: f32) -> f64 {
        x_to_beat(x, &self.viewport)
    }

    pub fn lane_origin_x(&self) -> f32 {
        lane_origin_x(&self.viewport)
    }

    pub fn lane_x_from_window_x(&self, window_x: f32) -> f32 {
        window_x - self.lane_origin_x()
    }

    pub fn beats_from_window_x(&self, window_x: f32) -> f32 {
        self.x_to_beats(self.lane_x_from_window_x(window_x))
    }

    pub fn snap_beats(&self, beats: f32) -> f32 {
        self.snap_beats_with_bypass(beats, false)
    }

    pub fn snap_beats_with_bypass(&self, beats: f32, bypass: bool) -> f32 {
        snap_beat_against_meter(beats, self.snap, &self.time_signature_map, bypass)
    }

    pub fn snap_time(&self, seconds: f32) -> f32 {
        snap_seconds(seconds, self.seconds_per_beat(), self.snap)
    }

    pub fn arrangement_content_top(&self) -> f32 {
        self.content_top
    }

    pub fn timeline_origin_y(&self) -> f32 {
        timeline_origin_y(&self.viewport)
    }

    /// Window y -> arrangement content y: 0 at the top of the first track row,
    /// vertical scroll included. The space track rows are laid out in.
    pub fn content_y_from_window_y(&self, window_y: f32) -> f32 {
        window_y - self.timeline_origin_y() - self.content_top + self.viewport.scroll_y
    }
}

pub fn track_at_y(y: f32, layout: &TrackLayout) -> Option<TrackId> {
    let content_y = y + layout.scroll_y;
    layout
        .rows
        .iter()
        .find(|row| content_y >= row.y && content_y < row.y + row.height)
        .map(|row| row.track_id.clone())
}

pub fn clip_rect(
    clip: &ClipState,
    viewport: &TimelineViewport,
    layout: &TrackLayout,
    track_id: &str,
) -> gpui::Bounds<gpui::Pixels> {
    let x = beat_to_x(clip.start_beat as f64, viewport);
    let end = beat_to_x(
        (clip.start_beat + clip.duration_beats.max(0.0)) as f64,
        viewport,
    );
    let w = (end - x).max(1.0);
    let row = layout
        .row_for_track(track_id)
        .map(|row| (row.y, row.height))
        .unwrap_or((0.0, layout.track_height));
    let y = row.0 - layout.scroll_y;
    gpui::bounds(
        gpui::point(gpui::px(x), gpui::px(y)),
        gpui::size(gpui::px(w), gpui::px(row.1)),
    )
}

impl TimelineState {
    pub fn time_to_content_x(&self, time_sec: f32) -> f32 {
        (time_sec * self.viewport.pixels_per_second - self.viewport.scroll_x).round()
    }

    pub fn content_x_to_time(&self, x: f32) -> f32 {
        ((x + self.viewport.scroll_x) / self.viewport.pixels_per_second).max(0.0)
    }

    pub fn beats_to_x(&self, beats: f32) -> f32 {
        beat_to_x(beats as f64, &self.viewport)
    }

    pub fn x_to_beats(&self, x: f32) -> f32 {
        x_to_beat(x, &self.viewport) as f32
    }

    pub fn beat_to_x(&self, beat: f32) -> f32 {
        self.beats_to_x(beat)
    }

    pub fn x_to_beat(&self, x: f32) -> f64 {
        x_to_beat(x, &self.viewport)
    }

    /// Window-space x of the arrangement lane origin — the left edge of the
    /// scrollable clip area, i.e. past the browser panel and the track headers.
    ///
    /// Every gesture that resolves a window-space pointer x (clip move, clip
    /// edge-resize, ruler click and scrub, lane tools, automation, tempo,
    /// markers, regions, song text) must map through this so pointer
    /// coordinates and drawing share one transform.
    pub fn lane_origin_x(&self) -> f32 {
        lane_origin_x(&self.viewport)
    }

    /// Convert a window-space x into arrangement-lane content x.
    pub fn lane_x_from_window_x(&self, window_x: f32) -> f32 {
        window_x - self.lane_origin_x()
    }

    /// Convert a window-space x straight to timeline beats.
    pub fn beats_from_window_x(&self, window_x: f32) -> f32 {
        self.x_to_beats(self.lane_x_from_window_x(window_x))
    }

    /// Window-space y of the timeline's top edge; see [`timeline_origin_y`].
    /// Every gesture that resolves a window-space pointer y into the
    /// arrangement goes through this, the vertical twin of
    /// [`Self::lane_origin_x`].
    pub fn timeline_origin_y(&self) -> f32 {
        timeline_origin_y(&self.viewport)
    }

    /// Window y -> y inside the track area's viewport (0 at the top of the
    /// visible track area, no scroll). Unclamped.
    pub fn track_viewport_y_from_window_y(&self, window_y: f32) -> f32 {
        window_y - self.timeline_origin_y() - self.arrangement_content_top()
    }

    /// Window y -> arrangement content y: 0 at the top of the first track row,
    /// vertical scroll included. The space track rows are laid out in.
    pub fn content_y_from_window_y(&self, window_y: f32) -> f32 {
        self.track_viewport_y_from_window_y(window_y) + self.viewport.scroll_y
    }

    /// Horizontal extent a clip is drawn at: `(left, width)` in lane x.
    ///
    /// An audio clip's end comes from [`Self::audio_clip_end_beat`], the same
    /// derivation `reconcile_audio_clip_lengths` writes into the model, so a
    /// tempo ramp bends the drawn clip exactly as much as the grid under it.
    pub fn clip_lane_x_span(&self, clip: &ClipState) -> (f32, f32) {
        let left = self.beats_to_x(clip.start_beat);
        let width = if matches!(clip.clip_type, ClipType::Audio { .. }) {
            let end_beat = self
                .audio_clip_end_beat(clip)
                // Pending and legacy clips may not have decoded source bounds yet.
                .unwrap_or_else(|| (clip.start_beat + clip.duration_beats.max(0.0)) as f64);
            self.beats_to_x(end_beat as f32) - left
        } else {
            self.beat_span_px(clip.start_beat, clip.duration_beats)
        };
        (left, width.max(CLIP_MIN_DRAWN_WIDTH))
    }

    /// The rectangle a clip is drawn at inside a lane row `row_height` tall.
    ///
    /// The one clip geometry: the audio, MIDI and video clip renderers draw
    /// through it and the arrangement marquee hit-tests through it, so a clip
    /// is selected exactly where it is painted — its minimum drawn width
    /// included, and the pad bands above and below it excluded.
    pub fn clip_lane_rect(&self, clip: &ClipState, row_height: f32) -> ClipLaneRect {
        let (left, width) = self.clip_lane_x_span(clip);
        let (top, height) = clip_lane_band(row_height);
        ClipLaneRect {
            left,
            top,
            width,
            height,
        }
    }

    pub fn arrangement_track_layout(&self) -> TrackLayout {
        TrackLayout::from_state(self)
    }

    pub fn snap_time(&self, seconds: f32) -> f32 {
        snap_seconds(
            seconds,
            self.seconds_per_beat(),
            SnapSettings::from_timeline(self),
        )
    }

    /// Snap a beat value to the current grid (or return it unchanged when snap is off).
    pub fn snap_beats(&self, beats: f32) -> f32 {
        self.snap_beats_with_bypass(beats, false)
    }

    /// Snap a beat value, optionally bypassing the grid (Shift held during drag).
    pub fn snap_beats_with_bypass(&self, beats: f32, bypass: bool) -> f32 {
        snap_beat_against_meter(
            beats,
            SnapSettings::from_timeline(self),
            &self.time_signature_map,
            bypass,
        )
    }

    /// This frame's gesture geometry — see [`TimelineGestureContext`].
    pub fn gesture_context(&self) -> TimelineGestureContext {
        TimelineGestureContext::from_state(self)
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0e-3
    }

    /// Every arrangement pointer y — track rows, automation, the marquee, the
    /// tempo lane — resolves through the measured timeline top once there is
    /// one. The chrome constant only stands in before the first frame, and it
    /// is taller than the chrome actually drawn, which put every row lookup a
    /// few pixels above the row the pointer was on.
    #[test]
    fn pointer_y_resolves_through_the_measured_timeline_top() {
        let mut state = TimelineState::default();
        state.viewport.scroll_y = 30.0;
        let content_top = state.arrangement_content_top();

        let fallback = crate::shell_metrics::APP_CHROME_HEIGHT;
        assert!(close(state.timeline_origin_y(), fallback));
        assert!(close(
            state.content_y_from_window_y(fallback + content_top),
            30.0
        ));

        state.viewport.timeline_origin_y_measured = Some(72.0);
        let first_row_top = 72.0 + content_top;
        assert!(close(
            state.track_viewport_y_from_window_y(first_row_top),
            0.0
        ));
        assert!(close(
            state.content_y_from_window_y(first_row_top + 5.0),
            35.0
        ));
        // The lanes' press handlers resolve through the same origin.
        let gestures = state.gesture_context();
        assert!(close(
            gestures.content_y_from_window_y(first_row_top + 5.0),
            35.0
        ));
        // So does the tempo lane under the ruler.
        assert!(close(
            state.tempo_lane_origin_y(),
            72.0 + RULER_HEIGHT + state.global_lane_top(GlobalLaneKind::Tempo)
        ));
    }
}
