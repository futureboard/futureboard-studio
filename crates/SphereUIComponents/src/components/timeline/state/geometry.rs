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
