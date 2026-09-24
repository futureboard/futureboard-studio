use super::*;

/// Global/system lanes rendered between the ruler and normal tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalLaneKind {
    Tempo,
    TimeSignature,
    SongText,
    Marker,
    Arranger,
    Chord,
}

/// Shortest a global lane may be dragged. Below this the header controls stop
/// fitting and the lane stops being a usable hit target.
pub const GLOBAL_LANE_MIN_HEIGHT: f32 = 28.0;

/// Tallest a global lane may be dragged. The conductor lanes sit above the
/// arrangement and never scroll, so an unbounded lane could push every track
/// off screen.
pub const GLOBAL_LANE_MAX_HEIGHT: f32 = 240.0;

/// Hit height of the drag strip on a global lane's bottom edge.
pub const GLOBAL_LANE_RESIZE_HANDLE_HITBOX: f32 = 5.0;

/// User-set heights for the resizable global lanes. `None` means "use the
/// lane's default height".
///
/// View state, like the lane visibility and collapse flags: it is not part of
/// the audio graph and never reaches the engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalLaneHeights {
    pub tempo: Option<f32>,
    pub time_signature: Option<f32>,
    pub song_text: Option<f32>,
    pub marker: Option<f32>,
    pub region: Option<f32>,
    pub chord: Option<f32>,
}

impl GlobalLaneHeights {
    pub fn get(&self, kind: GlobalLaneKind) -> Option<f32> {
        match kind {
            GlobalLaneKind::Tempo => self.tempo,
            GlobalLaneKind::TimeSignature => self.time_signature,
            GlobalLaneKind::SongText => self.song_text,
            GlobalLaneKind::Marker => self.marker,
            GlobalLaneKind::Arranger => self.region,
            GlobalLaneKind::Chord => self.chord,
        }
    }

    pub fn set(&mut self, kind: GlobalLaneKind, height: Option<f32>) {
        let height = height.map(clamp_global_lane_height);
        match kind {
            GlobalLaneKind::Tempo => self.tempo = height,
            GlobalLaneKind::TimeSignature => self.time_signature = height,
            GlobalLaneKind::SongText => self.song_text = height,
            GlobalLaneKind::Marker => self.marker = height,
            GlobalLaneKind::Arranger => self.region = height,
            GlobalLaneKind::Chord => self.chord = height,
        }
    }
}

pub fn clamp_global_lane_height(height: f32) -> f32 {
    height.clamp(GLOBAL_LANE_MIN_HEIGHT, GLOBAL_LANE_MAX_HEIGHT)
}

/// In-flight global-lane height drag. Mirrors `TrackHeightResizeSession`: the
/// start height is captured once so every move maps an absolute pointer delta
/// instead of accumulating per-frame rounding.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalLaneResizeSession {
    pub kind: GlobalLaneKind,
    /// Rendered height at gesture start — the base every move offsets from.
    pub start_height: f32,
    /// The lane's stored height entry at gesture start. `None` means it was on
    /// its default, and that is what undo restores: pinning an explicit value
    /// equal to the default would silently opt the lane out of future default
    /// changes.
    pub start_custom_height: Option<f32>,
    pub start_mouse_y: f32,
}

/// Map a BPM value to a lane-local y coordinate (high BPM near the top).
pub fn bpm_to_y(bpm: f64, lane_height: f32, min_bpm: f64, max_bpm: f64) -> f32 {
    let pad = TEMPO_LANE_PAD;
    let usable = (lane_height - 2.0 * pad).max(1.0);
    let span = (max_bpm - min_bpm).max(1e-9);
    let t = ((bpm - min_bpm) / span).clamp(0.0, 1.0);
    pad + ((1.0 - t) as f32) * usable
}

/// Inverse of [`bpm_to_y`]: lane-local y → BPM.
pub fn y_to_bpm(y: f32, lane_height: f32, min_bpm: f64, max_bpm: f64) -> f64 {
    let pad = TEMPO_LANE_PAD;
    let usable = (lane_height - 2.0 * pad).max(1.0);
    let t = ((y - pad) / usable).clamp(0.0, 1.0);
    let span = (max_bpm - min_bpm).max(1e-9);
    (max_bpm - t as f64 * span).clamp(TEMPO_BPM_MIN, TEMPO_BPM_MAX)
}

impl TimelineState {
    /// Total height of the visible conductor lanes.
    ///
    /// `arrangement_content_top()` is built from this and drives every
    /// window-y -> arrangement-y conversion, so a lane counted here but not
    /// drawn (or vice versa) offsets all arrangement hit-testing.
    pub fn global_lanes_height(&self) -> f32 {
        self.visible_global_lanes()
            .into_iter()
            .map(|kind| self.global_lane_height(kind))
            .sum()
    }

    /// Offset of one lane's top edge from the top of the conductor block.
    ///
    /// Any lane that maps a pointer y to a value (the Tempo curve) has to build
    /// its origin from this rather than assuming it sits directly under the
    /// ruler — the block's order is not fixed.
    pub fn global_lane_top(&self, kind: GlobalLaneKind) -> f32 {
        let mut top = 0.0;
        for visible in self.visible_global_lanes() {
            if visible == kind {
                break;
            }
            top += self.global_lane_height(visible);
        }
        top
    }

    /// Height of the global Song Text lane when visible, else 0.
    pub fn song_text_track_height(&self) -> f32 {
        if !self.show_song_text_track {
            return 0.0;
        }
        self.global_lane_height(GlobalLaneKind::SongText)
    }

    /// Default (un-resized) height for a global lane.
    pub fn global_lane_default_height(kind: GlobalLaneKind) -> f32 {
        match kind {
            GlobalLaneKind::Tempo => TEMPO_TRACK_HEIGHT,
            GlobalLaneKind::TimeSignature => Self::TIME_SIGNATURE_TRACK_HEIGHT,
            GlobalLaneKind::SongText => {
                crate::components::timeline::song_text_track::SONG_TEXT_LANE_HEIGHT
            }
            GlobalLaneKind::Marker => MARKER_TRACK_HEIGHT,
            GlobalLaneKind::Arranger => REGION_TRACK_HEIGHT,
            GlobalLaneKind::Chord => CHORD_TRACK_HEIGHT,
        }
    }

    /// Live height of one global lane, ignoring visibility. Collapse wins over a
    /// custom height so the collapse button always produces the compact row.
    pub fn global_lane_height(&self, kind: GlobalLaneKind) -> f32 {
        match kind {
            GlobalLaneKind::Tempo if self.tempo_track_collapsed => TEMPO_TRACK_HEIGHT_COLLAPSED,
            GlobalLaneKind::TimeSignature if self.time_signature_track_collapsed => {
                Self::TIME_SIGNATURE_TRACK_HEIGHT_COLLAPSED
            }
            GlobalLaneKind::Marker if self.marker_track_collapsed => MARKER_TRACK_HEIGHT_COLLAPSED,
            GlobalLaneKind::Arranger if self.region_track_collapsed => {
                REGION_TRACK_HEIGHT_COLLAPSED
            }
            GlobalLaneKind::Chord if self.chord_track_collapsed => CHORD_TRACK_HEIGHT_COLLAPSED,
            _ => self
                .global_lane_heights
                .get(kind)
                .unwrap_or_else(|| Self::global_lane_default_height(kind)),
        }
    }

    /// Window-space y of the Tempo lane's content top.
    ///
    /// The lane maps a pointer y onto a BPM axis, so this is the one number its
    /// hit test and its drag both have to agree on. It used to be spelled
    /// `APP_CHROME_HEIGHT + RULER_HEIGHT` inline in two files, which silently
    /// became wrong the moment another conductor lane was allowed above it.
    pub fn tempo_lane_origin_y(&self) -> f32 {
        crate::shell_metrics::APP_CHROME_HEIGHT
            + RULER_HEIGHT
            + self.global_lane_top(GlobalLaneKind::Tempo)
    }

    /// Window-space y -> BPM for the Tempo lane.
    ///
    /// The single inverse of [`bpm_to_y`] for pointer input: the lane's hit
    /// test and the drag that follows it both go through this, so a click on a
    /// dot and the move that grabs it cannot resolve different BPMs.
    ///
    /// Note there is no extra [`TEMPO_LANE_PAD`] term here. `bpm_to_y` already
    /// puts the pad inside the mapping and `y_to_bpm` already takes it back
    /// out; both call sites used to subtract it a second time, which pushed the
    /// whole BPM axis a pad's height away from the pointer.
    pub fn tempo_bpm_at_window_y(&self, window_y: f32) -> f64 {
        let (min_bpm, max_bpm) = self.tempo_lane_bpm_range();
        y_to_bpm(
            window_y - self.tempo_lane_origin_y(),
            self.tempo_track_height(),
            min_bpm,
            max_bpm,
        )
    }

    /// Inverse of [`Self::tempo_bpm_at_window_y`]: the window-space y a marker
    /// at `bpm` is drawn at.
    pub fn tempo_window_y_at_bpm(&self, bpm: f64) -> f32 {
        let (min_bpm, max_bpm) = self.tempo_lane_bpm_range();
        self.tempo_lane_origin_y() + bpm_to_y(bpm, self.tempo_track_height(), min_bpm, max_bpm)
    }

    /// Arm a lane resize at pointer-down. Promoted to a live session on the
    /// first drag-move, so a plain click on the handle never resizes.
    pub fn arm_global_lane_resize(&mut self, kind: GlobalLaneKind, start_mouse_y: f32) {
        self.global_lane_resize_arm = Some((kind, start_mouse_y));
    }

    pub fn clear_global_lane_resize_arm(&mut self) {
        self.global_lane_resize_arm = None;
    }

    pub fn ensure_global_lane_resize_from_arm(&mut self, mouse_y: f32) -> bool {
        if self.global_lane_resize.is_some() {
            return true;
        }
        let Some((kind, start_y)) = self.global_lane_resize_arm.take() else {
            return false;
        };
        self.global_lane_resize = Some(GlobalLaneResizeSession {
            kind,
            start_height: self.global_lane_height(kind),
            start_custom_height: self.global_lane_heights.get(kind),
            start_mouse_y: start_y,
        });
        self.update_global_lane_resize(mouse_y)
    }

    pub fn update_global_lane_resize(&mut self, mouse_y: f32) -> bool {
        let Some(session) = self.global_lane_resize.clone() else {
            return false;
        };
        let next =
            clamp_global_lane_height(session.start_height + (mouse_y - session.start_mouse_y));
        if (next - self.global_lane_height(session.kind)).abs() < 0.01 {
            return false;
        }
        // Dragging a lane taller un-collapses it: the collapsed height is a
        // separate presentation, and leaving the flag set would snap the lane
        // back the moment the drag ended.
        match session.kind {
            GlobalLaneKind::Tempo => self.tempo_track_collapsed = false,
            GlobalLaneKind::TimeSignature => self.time_signature_track_collapsed = false,
            _ => {}
        }
        self.global_lane_heights.set(session.kind, Some(next));
        true
    }

    /// Abandon the gesture and put the lane back where it started.
    pub fn cancel_global_lane_resize(&mut self) -> bool {
        self.global_lane_resize_arm = None;
        let Some(session) = self.global_lane_resize.take() else {
            return false;
        };
        self.global_lane_heights
            .set(session.kind, session.start_custom_height);
        true
    }

    /// End the gesture. Returns the before/after height maps for one undo entry,
    /// or `None` when the lane ended up where it started.
    pub fn finish_global_lane_resize(&mut self) -> Option<(GlobalLaneHeights, GlobalLaneHeights)> {
        let session = self.global_lane_resize.take()?;
        let next = self.global_lane_heights.clone();
        let mut prev = next.clone();
        prev.set(session.kind, session.start_custom_height);
        if prev == next {
            return None;
        }
        Some((prev, next))
    }

    /// Double-click on the handle: back to the lane's default height.
    pub fn reset_global_lane_height(
        &mut self,
        kind: GlobalLaneKind,
    ) -> Option<(GlobalLaneHeights, GlobalLaneHeights)> {
        if self.global_lane_heights.get(kind).is_none() {
            return None;
        }
        let prev = self.global_lane_heights.clone();
        self.global_lane_heights.set(kind, None);
        Some((prev, self.global_lane_heights.clone()))
    }

    pub fn show_song_text_track_lane(&mut self) {
        self.show_song_text_track = true;
    }

    pub fn hide_song_text_track_lane(&mut self) {
        self.show_song_text_track = false;
    }

    /// Visible global/system lanes, top to bottom.
    ///
    /// Structure first (regions, then markers), then the conductor data the
    /// structure is measured in (tempo, meter), then annotation. Section names
    /// are what the eye tracks against the ruler while arranging, so they sit
    /// closest to it; the tempo curve needs the most room and is happiest
    /// against the arrangement it drives.
    pub fn visible_global_lanes(&self) -> Vec<GlobalLaneKind> {
        let mut lanes = Vec::new();
        if self.show_region_track {
            lanes.push(GlobalLaneKind::Arranger);
        }
        if self.show_marker_track {
            lanes.push(GlobalLaneKind::Marker);
        }
        // Harmony reads with the structure it belongs to: sections and cues
        // above, the conductor data below.
        if self.show_chord_track {
            lanes.push(GlobalLaneKind::Chord);
        }
        if self.show_tempo_track {
            lanes.push(GlobalLaneKind::Tempo);
        }
        if self.show_time_signature_track {
            lanes.push(GlobalLaneKind::TimeSignature);
        }
        if self.show_song_text_track {
            lanes.push(GlobalLaneKind::SongText);
        }
        lanes
    }
}
