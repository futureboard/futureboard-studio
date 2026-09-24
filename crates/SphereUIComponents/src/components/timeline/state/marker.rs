use super::*;

/// Default height of the global Marker lane (px).
///
/// A single row of anchored flags — it needs the flag body plus breathing room,
/// not a full track row.
pub const MARKER_TRACK_HEIGHT: f32 = 30.0;
pub const MARKER_TRACK_HEIGHT_COLLAPSED: f32 = 22.0;

/// Default height of the global Region (arranger) lane (px). One row taller
/// than the Marker lane because a region is a *block* with a readable name
/// inside it, not a flag hanging off a beat.
pub const REGION_TRACK_HEIGHT: f32 = 34.0;
pub const REGION_TRACK_HEIGHT_COLLAPSED: f32 = 22.0;

/// Smallest region length a drag may produce, in beats. Below this the block
/// stops being clickable and the two edge handles overlap.
pub const MIN_REGION_BEATS: f64 = 0.25;

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineMarkerState {
    pub id: String,
    pub beat: f64,
    pub name: String,
    pub color_hex: String,
    /// SysEx messages sent when playback crosses the marker, each a complete
    /// `F0 … F7` message. Sent to every hardware MIDI output a MIDI track
    /// routes to — SysEx names its manufacturer, so a GS reset reaches the
    /// Roland module and is ignored by the rest — and written to the
    /// conductor track on MIDI export.
    pub sysex: Vec<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_midi_markers_are_added_at_clip_relative_beats() {
        let mut state = TimelineState::default();
        state.import_midi_markers(
            8.0,
            &[
                crate::components::timeline::midi_import::ImportedMidiMarker {
                    text: "Chorus".to_string(),
                    absolute_tick: 960,
                    beat: 2.0,
                },
            ],
        );

        assert_eq!(state.markers.len(), 1);
        assert_eq!(state.markers[0].name, "Chorus");
        assert!((state.markers[0].beat - 10.0).abs() < 1.0e-6);
    }
}

impl TimelineMarkerState {
    pub fn new(beat: f64, name: impl Into<String>, color_hex: impl Into<String>) -> Self {
        Self::with_id("", beat, name, color_hex)
    }

    pub fn with_id(
        id: impl Into<String>,
        beat: f64,
        name: impl Into<String>,
        color_hex: impl Into<String>,
    ) -> Self {
        let mut id = id.into();
        if id.is_empty() {
            id = next_timeline_marker_id();
        }
        Self {
            id,
            beat: beat.max(0.0),
            name: name.into(),
            color_hex: color_hex.into(),
            sysex: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineRegionState {
    pub id: String,
    pub start_beat: f64,
    pub end_beat: f64,
    pub name: String,
    pub color_hex: String,
}

impl TimelineRegionState {
    pub fn new(
        start_beat: f64,
        end_beat: f64,
        name: impl Into<String>,
        color_hex: impl Into<String>,
    ) -> Self {
        Self::with_id("", start_beat, end_beat, name, color_hex)
    }

    pub fn with_id(
        id: impl Into<String>,
        start_beat: f64,
        end_beat: f64,
        name: impl Into<String>,
        color_hex: impl Into<String>,
    ) -> Self {
        let mut id = id.into();
        if id.is_empty() {
            id = next_timeline_region_id();
        }
        let (start, end) = if start_beat <= end_beat {
            (start_beat.max(0.0), end_beat.max(0.0))
        } else {
            (end_beat.max(0.0), start_beat.max(0.0))
        };
        Self {
            id,
            start_beat: start,
            end_beat: end.max(start + 1.0e-3),
            name: name.into(),
            color_hex: color_hex.into(),
        }
    }

    pub fn normalized_range(&self) -> (f64, f64) {
        if self.start_beat <= self.end_beat {
            (self.start_beat, self.end_beat)
        } else {
            (self.end_beat, self.start_beat)
        }
    }
}

impl TimelineState {
    pub fn add_marker_at_beat(&mut self, beat: f64) -> String {
        let label = format!("Marker {}", self.markers.len() + 1);
        let marker = TimelineMarkerState::new(
            beat,
            label,
            crate::color::rgba_to_hex(crate::theme::Colors::automation_curve()),
        );
        let id = marker.id.clone();
        self.markers.push(marker);
        self.sort_markers();
        id
    }

    pub fn import_midi_markers(
        &mut self,
        clip_start_beat: f32,
        markers: &[crate::components::timeline::midi_import::ImportedMidiMarker],
    ) {
        if markers.is_empty() {
            return;
        }
        for marker in markers {
            let name = if marker.text.trim().is_empty() {
                format!("MIDI Marker {}", self.markers.len() + 1)
            } else {
                marker.text.clone()
            };
            self.markers.push(TimelineMarkerState::new(
                (clip_start_beat + marker.beat) as f64,
                name,
                crate::color::rgba_to_hex(crate::theme::Colors::automation_curve()),
            ));
        }
        self.sort_markers();
    }

    pub fn add_region_at_beat(&mut self, beat: f64) -> String {
        let start = beat.max(0.0);
        let length = self.beats_per_bar_at_beat(start).max(1.0);
        let label = format!("Region {}", self.regions.len() + 1);
        let region = TimelineRegionState::new(start, start + length, label, "#42C7A3");
        let id = region.id.clone();
        self.regions.push(region);
        self.sort_regions();
        id
    }

    pub fn delete_marker(&mut self, id: &str) -> bool {
        let before = self.markers.len();
        self.markers.retain(|marker| marker.id != id);
        if self.selected_marker_id.as_deref() == Some(id) {
            self.selected_marker_id = None;
        }
        before != self.markers.len()
    }

    pub fn delete_region(&mut self, id: &str) -> bool {
        let before = self.regions.len();
        self.regions.retain(|region| region.id != id);
        if self.selected_region_id.as_deref() == Some(id) {
            self.selected_region_id = None;
        }
        before != self.regions.len()
    }

    pub fn update_region_range(&mut self, id: &str, start_beat: f64, end_beat: f64) -> bool {
        let Some(region) = self.regions.iter_mut().find(|region| region.id == id) else {
            return false;
        };
        let updated = TimelineRegionState::with_id(
            region.id.clone(),
            start_beat,
            end_beat,
            region.name.clone(),
            region.color_hex.clone(),
        );
        if (region.start_beat - updated.start_beat).abs() < 1.0e-6
            && (region.end_beat - updated.end_beat).abs() < 1.0e-6
        {
            return false;
        }
        region.start_beat = updated.start_beat;
        region.end_beat = updated.end_beat.max(updated.start_beat + MIN_REGION_BEATS);
        self.sort_regions();
        true
    }
}

// ── Marker lane ──────────────────────────────────────────────────────────────

impl TimelineState {
    /// Height of the global Marker lane when visible, else 0.
    pub fn marker_track_height(&self) -> f32 {
        if !self.show_marker_track {
            return 0.0;
        }
        self.global_lane_height(GlobalLaneKind::Marker)
    }

    pub fn show_marker_track_lane(&mut self) {
        self.show_marker_track = true;
    }

    pub fn hide_marker_track_lane(&mut self) {
        self.show_marker_track = false;
        self.selected_marker_id = None;
    }

    pub fn select_marker(&mut self, id: &str) {
        self.selected_marker_id = Some(id.to_string());
    }

    pub fn clear_marker_selection(&mut self) {
        self.selected_marker_id = None;
    }

    pub fn marker(&self, id: &str) -> Option<&TimelineMarkerState> {
        self.markers.iter().find(|marker| marker.id == id)
    }

    /// Marker whose beat is within `tolerance_beats` of `beat`, nearest first.
    pub fn marker_at(&self, beat: f64, tolerance_beats: f64) -> Option<String> {
        self.markers
            .iter()
            .filter(|marker| (marker.beat - beat).abs() <= tolerance_beats)
            .min_by(|a, b| (a.beat - beat).abs().total_cmp(&(b.beat - beat).abs()))
            .map(|marker| marker.id.clone())
    }

    /// Move one marker to `beat`. Returns `false` when it did not actually move,
    /// so a click that only selected never records an edit.
    /// Beat a Marker lane move resolves to for a pointer at `lane_x`.
    ///
    /// `grab_offset_beats` is how far into the flag the marker was grabbed, so
    /// a flag caught by its label keeps that offset instead of snapping its
    /// beat line under the cursor. Snapping happens *after* the offset is
    /// removed, which is what puts the marker itself on the grid rather than
    /// the pointer.
    pub fn marker_drag_beat(&self, lane_x: f32, grab_offset_beats: f64) -> f64 {
        let beat = (self.x_to_beat(lane_x) - grab_offset_beats).max(0.0);
        self.snap_beats(beat as f32).max(0.0) as f64
    }

    pub fn move_marker(&mut self, id: &str, beat: f64) -> bool {
        let beat = beat.max(0.0);
        let Some(marker) = self.markers.iter_mut().find(|marker| marker.id == id) else {
            return false;
        };
        if (marker.beat - beat).abs() < 1.0e-9 {
            return false;
        }
        marker.beat = beat;
        self.sort_markers();
        true
    }

    pub fn rename_marker(&mut self, id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(marker) = self.markers.iter_mut().find(|marker| marker.id == id) else {
            return false;
        };
        if marker.name == name {
            return false;
        }
        marker.name = name.to_string();
        true
    }

    /// Markers are kept beat-ordered so the lane, the ruler, and MIDI export
    /// all read the same sequence. Ties break on id so the order is stable.
    pub(crate) fn sort_markers(&mut self) {
        self.markers
            .sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.id.cmp(&b.id)));
    }

    /// Regions are kept start-ordered, matching [`Self::sort_markers`].
    pub(crate) fn sort_regions(&mut self) {
        self.regions.sort_by(|a, b| {
            a.start_beat
                .total_cmp(&b.start_beat)
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    /// Secondary label for the Marker lane header.
    pub fn marker_lane_header_subtitle(&self) -> String {
        match self.markers.len() {
            0 => "No markers".to_string(),
            1 => self.markers[0].name.clone(),
            n => format!("{n} markers"),
        }
    }
}

// ── Region (arranger) lane ───────────────────────────────────────────────────

impl TimelineState {
    /// Height of the global Region lane when visible, else 0.
    pub fn region_track_height(&self) -> f32 {
        if !self.show_region_track {
            return 0.0;
        }
        self.global_lane_height(GlobalLaneKind::Arranger)
    }

    pub fn show_region_track_lane(&mut self) {
        self.show_region_track = true;
    }

    pub fn hide_region_track_lane(&mut self) {
        self.show_region_track = false;
        self.selected_region_id = None;
    }

    pub fn select_region(&mut self, id: &str) {
        self.selected_region_id = Some(id.to_string());
    }

    pub fn clear_region_selection(&mut self) {
        self.selected_region_id = None;
    }

    pub fn region(&self, id: &str) -> Option<&TimelineRegionState> {
        self.regions.iter().find(|region| region.id == id)
    }

    /// Region containing `beat`. Later regions win an overlap, matching the
    /// paint order in the lane.
    pub fn region_at(&self, beat: f64) -> Option<String> {
        self.regions
            .iter()
            .rev()
            .find(|region| {
                let (start, end) = region.normalized_range();
                beat >= start && beat <= end
            })
            .map(|region| region.id.clone())
    }

    /// Move a region to start at `start_beat`, keeping its length.
    pub fn move_region(&mut self, id: &str, start_beat: f64) -> bool {
        let Some(region) = self.region(id) else {
            return false;
        };
        let (start, end) = region.normalized_range();
        let length = (end - start).max(MIN_REGION_BEATS);
        let next_start = start_beat.max(0.0);
        self.update_region_range(id, next_start, next_start + length)
    }

    pub fn rename_region(&mut self, id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(region) = self.regions.iter_mut().find(|region| region.id == id) else {
            return false;
        };
        if region.name == name {
            return false;
        }
        region.name = name.to_string();
        true
    }

    pub fn region_lane_header_subtitle(&self) -> String {
        match self.regions.len() {
            0 => "No regions".to_string(),
            1 => self.regions[0].name.clone(),
            n => format!("{n} regions"),
        }
    }
}
