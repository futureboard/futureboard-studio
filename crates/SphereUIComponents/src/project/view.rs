//! Arrangement view state, split by who it belongs to.
//!
//! - [`ProjectViewState`] is saved in the project file (the v54 view section):
//!   the loop, the snap grid, which conductor lanes are shown, which tracks have
//!   their automation expanded, and the mixer tree's "defaults applied" latch.
//!   These describe how the music is being worked on, so they travel with the
//!   file and come back for anyone who opens it.
//! - [`ViewSidecar`] is per user and per machine: horizontal zoom, the scroll
//!   position, the playhead and the selected track. It is kept under the app
//!   data folder, keyed by project id, and written when a session ends and after
//!   a save, so zooming or scrolling never makes a project "Unsaved".
//!
//! Neither reaches the engine or the undo history.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{desc_to_target, target_to_desc, AutomationTargetDesc};
use crate::components::timeline::timeline_state::{
    AutoScrollMode, PendingViewRestore, SnapDivision, SnapShape, TimelineState, TrackLaneMode,
};
use crate::settings::{AutoScrollPreference, SettingsSchema};

// ── Saved in the project ─────────────────────────────────────────────────────

/// The project's own view state (v54+). See the module docs for what is and is
/// not in here.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectViewState {
    pub loop_range: ProjectLoopRange,
    /// `None` when the file has no snap record, as in every pre-v54 file. The
    /// session then starts on the user's snap defaults from Settings.
    pub snap: Option<ProjectSnap>,
    /// `None` when the file has no lane record, as in every pre-v54 file. The
    /// lanes then follow the factory defaults, except that Chord and Song Text
    /// are shown when they have content: hidden lyrics read as lost work.
    pub lanes: Option<ProjectLaneVisibility>,
    /// Tracks whose lane shows automation rather than clips.
    pub automation_expanded: Vec<ProjectAutomationExpansion>,
    /// The mixer tree has had its default groups expanded once, so an empty
    /// expanded set is a deliberate collapse-all, not a tree never set up.
    pub mixer_tree_initialized: bool,
}

impl Default for ProjectViewState {
    fn default() -> Self {
        Self {
            loop_range: ProjectLoopRange::default(),
            snap: None,
            lanes: None,
            automation_expanded: Vec::new(),
            mixer_tree_initialized: false,
        }
    }
}

/// The transport loop, in beats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectLoopRange {
    pub enabled: bool,
    pub start_beat: f64,
    pub end_beat: f64,
}

impl Default for ProjectLoopRange {
    /// The timeline's own default loop: off, over the first four bars of 4/4.
    fn default() -> Self {
        Self {
            enabled: false,
            start_beat: 0.0,
            end_beat: 16.0,
        }
    }
}

impl ProjectLoopRange {
    /// A range that cannot be played (not finite, negative, empty or reversed)
    /// loads as the default rather than as a loop the transport cannot honour.
    fn sanitized(self) -> Self {
        let playable = self.start_beat.is_finite()
            && self.end_beat.is_finite()
            && self.start_beat >= 0.0
            && self.end_beat > self.start_beat
            && self.end_beat <= f32::MAX as f64;
        if playable {
            self
        } else {
            Self::default()
        }
    }
}

/// Snap switch plus the grid it snaps to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectSnap {
    pub enabled: bool,
    pub division: SnapDivision,
    pub shape: SnapShape,
}

/// Which conductor lanes are shown above the arrangement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectLaneVisibility {
    pub tempo: bool,
    pub time_signature: bool,
    pub marker: bool,
    pub region: bool,
    pub song_text: bool,
    pub chord: bool,
}

impl ProjectLaneVisibility {
    const TEMPO: u8 = 1 << 0;
    const TIME_SIGNATURE: u8 = 1 << 1;
    const MARKER: u8 = 1 << 2;
    const REGION: u8 = 1 << 3;
    const SONG_TEXT: u8 = 1 << 4;
    const CHORD: u8 = 1 << 5;

    /// Stable bit layout used in the project file.
    pub fn to_bits(self) -> u8 {
        let mut bits = 0;
        for (shown, bit) in [
            (self.tempo, Self::TEMPO),
            (self.time_signature, Self::TIME_SIGNATURE),
            (self.marker, Self::MARKER),
            (self.region, Self::REGION),
            (self.song_text, Self::SONG_TEXT),
            (self.chord, Self::CHORD),
        ] {
            if shown {
                bits |= bit;
            }
        }
        bits
    }

    /// Inverse of [`Self::to_bits`]. Bits a newer build may add are ignored.
    pub fn from_bits(bits: u8) -> Self {
        Self {
            tempo: bits & Self::TEMPO != 0,
            time_signature: bits & Self::TIME_SIGNATURE != 0,
            marker: bits & Self::MARKER != 0,
            region: bits & Self::REGION != 0,
            song_text: bits & Self::SONG_TEXT != 0,
            chord: bits & Self::CHORD != 0,
        }
    }
}

/// One track whose lane is expanded to automation, with the automation lane its
/// editor was focused on.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectAutomationExpansion {
    pub track_id: String,
    /// The focused target, flattened like a saved automation lane's. `None`
    /// follows the track's first lane, as it did when it was saved.
    pub selected_target: Option<AutomationTargetDesc>,
}

impl ProjectViewState {
    /// What of `tl`'s view is saved with the project.
    pub fn capture(tl: &TimelineState) -> Self {
        Self {
            loop_range: ProjectLoopRange {
                enabled: tl.transport.loop_enabled,
                start_beat: tl.transport.loop_start_beats as f64,
                end_beat: tl.transport.loop_end_beats as f64,
            },
            snap: Some(ProjectSnap {
                enabled: tl.snap_to_grid,
                division: tl.grid_division,
                shape: tl.snap_shape,
            }),
            lanes: Some(ProjectLaneVisibility {
                tempo: tl.show_tempo_track,
                time_signature: tl.show_time_signature_track,
                marker: tl.show_marker_track,
                region: tl.show_region_track,
                song_text: tl.show_song_text_track,
                chord: tl.show_chord_track,
            }),
            automation_expanded: tl
                .tracks
                .iter()
                .filter(|track| track.lane_mode == TrackLaneMode::Automation)
                .map(|track| ProjectAutomationExpansion {
                    track_id: track.id.clone(),
                    selected_target: track
                        .selected_automation_target
                        .as_ref()
                        .map(target_to_desc),
                })
                .collect(),
            mixer_tree_initialized: tl.mixer_tree_initialized,
        }
    }

    /// Restore onto a timeline whose tracks, chords and song text are already
    /// loaded. Snap is left alone when the file has none, for the session to
    /// seed from Settings (see [`seed_session_preferences`]).
    pub fn apply(&self, tl: &mut TimelineState) {
        let loop_range = self.loop_range.sanitized();
        tl.transport.loop_enabled = loop_range.enabled;
        tl.transport.loop_start_beats = loop_range.start_beat as f32;
        tl.transport.loop_end_beats = loop_range.end_beat as f32;

        if let Some(snap) = self.snap {
            tl.snap_to_grid = snap.enabled;
            tl.grid_division = snap.division;
            tl.snap_shape = snap.shape;
        }

        // Assigned directly rather than through the `show_*_lane` helpers,
        // which also seed content (a tempo anchor) as a side effect.
        let lanes = self.lanes.unwrap_or(ProjectLaneVisibility {
            tempo: true,
            time_signature: true,
            marker: true,
            region: true,
            song_text: !tl.song_text_events.is_empty(),
            chord: !tl.chord_events.is_empty(),
        });
        tl.show_tempo_track = lanes.tempo;
        tl.show_time_signature_track = lanes.time_signature;
        tl.show_marker_track = lanes.marker;
        tl.show_region_track = lanes.region;
        tl.show_song_text_track = lanes.song_text;
        tl.show_chord_track = lanes.chord;

        for entry in &self.automation_expanded {
            let Some(track) = tl.tracks.iter_mut().find(|t| t.id == entry.track_id) else {
                continue;
            };
            // Assigned directly: `toggle_track_lane_mode` would also create an
            // automation lane, and loading must not add content to a project.
            track.lane_mode = TrackLaneMode::Automation;
            track.selected_automation_target = entry
                .selected_target
                .as_ref()
                .map(|desc| desc_to_target(desc, &desc.parameter_name))
                .filter(|target| {
                    track
                        .automation_lanes
                        .iter()
                        .any(|lane| &lane.target == target)
                });
        }

        tl.mixer_tree_initialized = self.mixer_tree_initialized;
    }
}

// ── Per-user preferences seeded into every session ───────────────────────────

/// The per-user preferences a session's timeline takes from Settings whenever
/// its state is built or replaced (startup, open, switch, New Project,
/// templates): the metronome switch and the Follow / auto-scroll choice. With
/// `snap_from_settings` it also takes the snap defaults, for a project that has
/// none of its own (a new one, or one saved before v54).
///
/// Without this, installing a session replaced the timeline with its factory
/// defaults and the metronome came back off after every open.
pub fn seed_session_preferences(
    state: &mut TimelineState,
    schema: &SettingsSchema,
    snap_from_settings: bool,
) {
    state.transport.metronome_enabled = schema.recording.metronome.enabled;
    state.follow_playhead = schema.playback.follow_playhead;
    state.auto_scroll_mode = auto_scroll_mode_from_preference(schema.playback.auto_scroll_mode);
    if snap_from_settings {
        state.snap_to_grid = schema.editing.snap.snap_to_grid;
        state.grid_division = snap_division_from_setting(&schema.editing.snap.default_snap_value);
        state.snap_shape = SnapShape::Straight;
    }
}

/// Settings → Editing → Snap stores the grid as a fraction ("1/16"), which is
/// not the grid menu's command id ("16"), so the strings are mapped here one by
/// one. An unknown value keeps the factory 1/16.
pub fn snap_division_from_setting(value: &str) -> SnapDivision {
    match value.trim().to_ascii_lowercase().as_str() {
        "1/1" => SnapDivision::Div1_1,
        "1/2" => SnapDivision::Div1_2,
        "1/4" => SnapDivision::Div1_4,
        "1/8" => SnapDivision::Div1_8,
        "1/16" => SnapDivision::Div1_16,
        "1/32" => SnapDivision::Div1_32,
        "1/64" => SnapDivision::Div1_64,
        "bar" | "1 bar" => SnapDivision::Bar1,
        "auto" => SnapDivision::Auto,
        "off" => SnapDivision::Off,
        _ => SnapDivision::Div1_16,
    }
}

pub fn auto_scroll_mode_from_preference(preference: AutoScrollPreference) -> AutoScrollMode {
    match preference {
        AutoScrollPreference::Page => AutoScrollMode::Page,
        AutoScrollPreference::Continuous => AutoScrollMode::Continuous,
        AutoScrollPreference::Off => AutoScrollMode::Off,
    }
}

pub fn auto_scroll_preference_from_mode(mode: AutoScrollMode) -> AutoScrollPreference {
    match mode {
        AutoScrollMode::Page => AutoScrollPreference::Page,
        AutoScrollMode::Continuous => AutoScrollPreference::Continuous,
        AutoScrollMode::Off => AutoScrollPreference::Off,
    }
}

// ── Per user, outside the project ────────────────────────────────────────────

const VIEW_SIDECAR_VERSION: u32 = 1;
const VIEW_SIDECAR_DIR: &str = "View State";

fn view_sidecar_version() -> u32 {
    VIEW_SIDECAR_VERSION
}

/// Where one user left one project: zoom, scroll, playhead and selected track.
/// Stored as JSON under the app data folder, keyed by project id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSidecar {
    #[serde(default = "view_sidecar_version")]
    pub version: u32,
    /// Horizontal zoom. The zoom setting itself, not the derived pixels per
    /// beat, which is recomputed from it and the tempo on every frame.
    pub pixels_per_second: f32,
    /// Beat at the left edge of the arrangement. A beat rather than pixels, so
    /// it lands on the same music after a tempo edit or at another zoom.
    pub left_edge_beat: f64,
    pub scroll_y: f32,
    pub playhead_beat: f64,
    #[serde(default)]
    pub selected_track_id: Option<String>,
}

impl ViewSidecar {
    pub fn capture(tl: &TimelineState) -> Self {
        // A restore that has not reached a frame yet is still where the view
        // is; the viewport has not moved there so far.
        let (left_edge_beat, scroll_y) = match tl.pending_view_restore {
            Some(pending) => (pending.left_edge_beat, pending.scroll_y),
            None => (tl.x_to_beat(0.0), tl.viewport.scroll_y),
        };
        Self {
            version: VIEW_SIDECAR_VERSION,
            pixels_per_second: tl.viewport.pixels_per_second,
            left_edge_beat,
            scroll_y,
            playhead_beat: tl.transport.playhead_beats as f64,
            selected_track_id: tl.selection.selected_track_id.clone(),
        }
    }

    /// Restore onto a freshly installed session. Zoom goes through the wheel's
    /// own zoom, which clamps it to the arrangement's limits. The scroll waits
    /// in [`TimelineState::pending_view_restore`] for the first frame with a
    /// real viewport. The playhead is set in state only; playback starts from
    /// it.
    pub fn apply(&self, tl: &mut TimelineState) {
        if self.pixels_per_second.is_finite() && self.pixels_per_second > 0.0 {
            let current = tl.viewport.pixels_per_second.max(0.0001);
            tl.zoom_by(self.pixels_per_second / current, 0.0);
        }
        if self.playhead_beat.is_finite() {
            tl.transport.playhead_beats = self.playhead_beat.clamp(0.0, f32::MAX as f64) as f32;
        }
        if let Some(track_id) = self.selected_track_id.as_deref() {
            if tl.tracks.iter().any(|track| track.id == track_id) {
                tl.select_track(track_id);
            }
        }
        let finite_or_zero = |value: f64| {
            if value.is_finite() {
                value.max(0.0)
            } else {
                0.0
            }
        };
        tl.pending_view_restore = Some(PendingViewRestore {
            left_edge_beat: finite_or_zero(self.left_edge_beat),
            scroll_y: finite_or_zero(self.scroll_y as f64) as f32,
        });
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// `None` for anything that is not a sidecar this build can read.
    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

/// `<app_data>/View State/<project id>.json`. `None` for a project without an
/// id. Characters that cannot appear in a file name are replaced.
pub fn view_sidecar_path(app_data: &Path, project_id: &str) -> Option<PathBuf> {
    let id = project_id.trim();
    if id.is_empty() {
        return None;
    }
    let file_stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(
        app_data
            .join(VIEW_SIDECAR_DIR)
            .join(format!("{file_stem}.json")),
    )
}

pub fn read_view_sidecar(app_data: &Path, project_id: &str) -> Option<ViewSidecar> {
    let path = view_sidecar_path(app_data, project_id)?;
    ViewSidecar::from_json(&std::fs::read_to_string(path).ok()?)
}

/// Write through a temp file and a rename, so a crash mid-write leaves the
/// previous sidecar rather than half of one.
pub fn write_view_sidecar(
    app_data: &Path,
    project_id: &str,
    sidecar: &ViewSidecar,
) -> std::io::Result<()> {
    let Some(path) = view_sidecar_path(app_data, project_id) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, sidecar.to_json())?;
    std::fs::rename(&temp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        AutomationTarget, InsertPluginFormat, TempoCurve, TempoMap, TempoPoint,
    };
    use crate::project::{apply_to_timeline, decode_project, encode_project, FutureboardProject};

    fn roundtrip(state: &TimelineState) -> TimelineState {
        let bytes = encode_project(&FutureboardProject::from(state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);
        restored
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "futureboard-view-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn the_default_loop_is_the_timelines_default() {
        let transport = TimelineState::default().transport;
        let range = ProjectLoopRange::default();
        assert_eq!(range.enabled, transport.loop_enabled);
        assert_eq!(range.start_beat, transport.loop_start_beats as f64);
        assert_eq!(range.end_beat, transport.loop_end_beats as f64);
    }

    /// Loop, snap, lanes, automation expansion and the mixer latch come back
    /// from the file exactly as they were left.
    #[test]
    fn a_v54_project_restores_its_view_state() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let expanded = state.create_audio_track();
        let plain = state.create_audio_track();
        state.transport.loop_enabled = true;
        state.transport.loop_start_beats = 8.0;
        state.transport.loop_end_beats = 12.5;
        state.snap_to_grid = false;
        state.grid_division = SnapDivision::Div1_8;
        state.snap_shape = SnapShape::Triplet;
        state.show_tempo_track = false;
        state.show_marker_track = false;
        state.show_song_text_track = true;
        state
            .ensure_automation_lane(&expanded, AutomationTarget::TrackPan)
            .expect("lane");
        state.tracks[0].lane_mode = TrackLaneMode::Automation;
        state.tracks[0].selected_automation_target = Some(AutomationTarget::TrackPan);
        state.mixer_tree_initialized = true;
        state.mixer_tree.collapse_all();

        let mut restored = roundtrip(&state);

        assert!(restored.transport.loop_enabled);
        assert_eq!(restored.transport.loop_start_beats, 8.0);
        assert_eq!(restored.transport.loop_end_beats, 12.5);
        assert!(!restored.snap_to_grid);
        assert_eq!(restored.grid_division, SnapDivision::Div1_8);
        assert_eq!(restored.snap_shape, SnapShape::Triplet);
        assert!(!restored.show_tempo_track);
        assert!(restored.show_time_signature_track);
        assert!(!restored.show_marker_track);
        assert!(restored.show_region_track);
        assert!(restored.show_song_text_track, "shown although it is empty");
        assert!(!restored.show_chord_track);
        let track = restored.find_track(&expanded).expect("track");
        assert_eq!(track.lane_mode, TrackLaneMode::Automation);
        assert_eq!(
            track.selected_automation_target,
            Some(AutomationTarget::TrackPan)
        );
        assert_eq!(
            restored.find_track(&plain).expect("track").lane_mode,
            TrackLaneMode::Clips
        );

        // A deliberate collapse-all is not re-expanded to the defaults.
        crate::components::mixer_tree_model::ensure_timeline_mixer_tree_defaults(&mut restored, 2);
        assert!(restored.mixer_tree.expanded_node_ids.is_empty());
    }

    /// Restoring an expanded track assigns the mode; it never creates the
    /// automation lane that toggling the mode by hand would.
    #[test]
    fn restoring_automation_expansion_adds_no_lanes() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let id = state.create_audio_track();
        state.tracks[0].lane_mode = TrackLaneMode::Automation;
        // A focused target whose lane is gone falls back to the first lane.
        state.tracks[0].selected_automation_target = Some(AutomationTarget::TrackMute);

        let restored = roundtrip(&state);
        let track = restored.find_track(&id).expect("track");
        assert_eq!(track.lane_mode, TrackLaneMode::Automation);
        assert!(track.automation_lanes.is_empty());
        assert_eq!(track.selected_automation_target, None);
    }

    #[test]
    fn an_expansion_for_a_missing_track_is_ignored() {
        let mut project = FutureboardProject::new("stale");
        project
            .view
            .automation_expanded
            .push(ProjectAutomationExpansion {
                track_id: "gone".to_string(),
                selected_target: None,
            });
        let mut state = TimelineState::default();
        let _ = apply_to_timeline(&project, &mut state);
        assert!(state
            .tracks
            .iter()
            .all(|track| track.lane_mode == TrackLaneMode::Clips));
    }

    /// A plug-in switched off with its power button stays off after reopening.
    #[test]
    fn a_switched_off_insert_stays_off() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let slot = state.add_insert(&track).expect("slot");
        state.set_insert_plugin(
            &track,
            &slot,
            "comp".to_string(),
            None,
            InsertPluginFormat::Unknown,
            None,
            "Comp".to_string(),
        );
        assert_eq!(state.toggle_insert_enabled(&track, &slot), Some(false));

        let restored = roundtrip(&state);
        let insert = restored
            .find_insert_slot(&track, &slot)
            .expect("insert restored");
        assert!(!insert.enabled);
        assert!(!insert.bypassed);
    }

    /// Pre-v54 files (a default view) keep today's lanes, and show Song Text and
    /// Chord only when they have content. Snap is left for Settings.
    #[test]
    fn a_project_without_a_view_record_uses_the_content_rule() {
        use crate::project::{ProjectSongTextEvent, ProjectSongTextEventKind};
        let mut project = FutureboardProject::new("legacy");
        project
            .settings
            .song_text_events
            .push(ProjectSongTextEvent {
                id: "lyric-1".to_string(),
                beat: 0.0,
                kind: ProjectSongTextEventKind::Lyric {
                    text: "la".to_string(),
                    syllable_mode: crate::project::ProjectLyricSyllableMode::Phrase,
                    continuation: false,
                    duration_beats: None,
                    syllables: Vec::new(),
                },
            });
        let mut state = TimelineState::default();
        state.snap_to_grid = false;
        let _ = apply_to_timeline(&project, &mut state);
        assert!(state.show_song_text_track, "lyrics must not load hidden");
        assert!(!state.show_chord_track);
        assert!(state.show_tempo_track && state.show_marker_track && state.show_region_track);
        assert!(!state.snap_to_grid, "snap is left for the Settings seed");
        assert!(!state.transport.loop_enabled);
        assert_eq!(state.transport.loop_end_beats, 16.0);

        project.settings.song_text_events.clear();
        let mut empty = TimelineState::default();
        empty.show_song_text_track = true;
        let _ = apply_to_timeline(&project, &mut empty);
        assert!(!empty.show_song_text_track);
    }

    #[test]
    fn an_unplayable_loop_loads_as_the_default() {
        for (start, end) in [(4.0, 4.0), (8.0, 2.0), (-1.0, 4.0), (f64::NAN, 4.0)] {
            let mut project = FutureboardProject::new("loop");
            project.view.loop_range = ProjectLoopRange {
                enabled: true,
                start_beat: start,
                end_beat: end,
            };
            let mut state = TimelineState::default();
            let _ = apply_to_timeline(&project, &mut state);
            assert!(!state.transport.loop_enabled, "{start}..{end}");
            assert_eq!(state.transport.loop_start_beats, 0.0);
            assert_eq!(state.transport.loop_end_beats, 16.0);
        }
    }

    #[test]
    fn lane_bits_round_trip() {
        let lanes = ProjectLaneVisibility {
            tempo: true,
            time_signature: false,
            marker: true,
            region: false,
            song_text: true,
            chord: false,
        };
        assert_eq!(ProjectLaneVisibility::from_bits(lanes.to_bits()), lanes);
        assert_eq!(
            ProjectLaneVisibility::from_bits(lanes.to_bits() | 0b1100_0000),
            lanes,
            "bits from a newer build are ignored"
        );
    }

    #[test]
    fn settings_snap_strings_map_to_grid_divisions() {
        assert_eq!(snap_division_from_setting("1/4"), SnapDivision::Div1_4);
        assert_eq!(snap_division_from_setting("1/8"), SnapDivision::Div1_8);
        assert_eq!(snap_division_from_setting("1/16"), SnapDivision::Div1_16);
        assert_eq!(snap_division_from_setting(" 1/32 "), SnapDivision::Div1_32);
        assert_eq!(snap_division_from_setting("Bar"), SnapDivision::Bar1);
        // The grid menu's command id is not a Settings value.
        assert_eq!(snap_division_from_setting("16"), SnapDivision::Div1_16);
        assert_eq!(
            snap_division_from_setting("nonsense"),
            SnapDivision::Div1_16
        );
    }

    #[test]
    fn a_session_is_seeded_from_the_users_preferences() {
        let mut schema = SettingsSchema::default();
        schema.recording.metronome.enabled = true;
        schema.playback.follow_playhead = false;
        schema.playback.auto_scroll_mode = AutoScrollPreference::Continuous;
        schema.editing.snap.snap_to_grid = false;
        schema.editing.snap.default_snap_value = "1/8".to_string();

        let mut state = TimelineState::default();
        state.snap_shape = SnapShape::Dotted;
        seed_session_preferences(&mut state, &schema, true);
        assert!(state.transport.metronome_enabled);
        assert!(!state.follow_playhead);
        assert_eq!(state.auto_scroll_mode, AutoScrollMode::Continuous);
        assert!(!state.snap_to_grid);
        assert_eq!(state.grid_division, SnapDivision::Div1_8);
        assert_eq!(state.snap_shape, SnapShape::Straight);

        // A project with its own snap keeps it; the per-user rest still applies.
        let mut own = TimelineState::default();
        own.grid_division = SnapDivision::Div1_32;
        seed_session_preferences(&mut own, &schema, false);
        assert!(own.transport.metronome_enabled);
        assert!(own.snap_to_grid);
        assert_eq!(own.grid_division, SnapDivision::Div1_32);
    }

    #[test]
    fn the_follow_preference_survives_a_manual_scroll() {
        let mut schema = SettingsSchema::default();
        schema.playback.follow_playhead = true;
        let mut state = TimelineState::default();
        state.note_user_scrolled();
        assert!(!state.follow_playhead);
        // The runtime pause is not the preference: a new session follows again.
        seed_session_preferences(&mut state, &schema, false);
        assert!(state.follow_playhead);
    }

    #[test]
    fn a_sidecar_round_trips_through_json() {
        let sidecar = ViewSidecar {
            version: VIEW_SIDECAR_VERSION,
            pixels_per_second: 320.5,
            left_edge_beat: 37.25,
            scroll_y: 144.0,
            playhead_beat: 12.0,
            selected_track_id: Some("track-2".to_string()),
        };
        assert_eq!(ViewSidecar::from_json(&sidecar.to_json()), Some(sidecar));
        // Unknown fields from a newer build are ignored, and a missing
        // selection reads as none.
        let newer = r#"{"version":7,"pixels_per_second":80.0,"left_edge_beat":4.0,
            "scroll_y":0.0,"playhead_beat":1.0,"piano_roll":{"zoom":2}}"#;
        let read = ViewSidecar::from_json(newer).expect("tolerant read");
        assert_eq!(read.selected_track_id, None);
        assert_eq!(read.pixels_per_second, 80.0);
        assert_eq!(ViewSidecar::from_json("{not json"), None);
    }

    #[test]
    fn a_sidecar_is_written_and_read_by_project_id() {
        let app_data = temp_dir("sidecar");
        let sidecar = ViewSidecar {
            version: VIEW_SIDECAR_VERSION,
            pixels_per_second: 90.0,
            left_edge_beat: 8.0,
            scroll_y: 20.0,
            playhead_beat: 3.0,
            selected_track_id: None,
        };
        write_view_sidecar(&app_data, "abc/..\\def", &sidecar).expect("write");
        let path = view_sidecar_path(&app_data, "abc/..\\def").expect("path");
        assert_eq!(
            path.parent(),
            Some(app_data.join(VIEW_SIDECAR_DIR).as_path())
        );
        assert_eq!(read_view_sidecar(&app_data, "abc/..\\def"), Some(sidecar));
        assert_eq!(read_view_sidecar(&app_data, "other"), None);
        assert_eq!(view_sidecar_path(&app_data, "  "), None);
        let _ = std::fs::remove_dir_all(app_data);
    }

    #[test]
    fn applying_a_sidecar_zooms_selects_and_queues_the_scroll() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let _first = state.create_audio_track();
        let second = state.create_audio_track();
        let sidecar = ViewSidecar {
            version: VIEW_SIDECAR_VERSION,
            pixels_per_second: 1.0e9,
            left_edge_beat: 16.0,
            scroll_y: 90.0,
            playhead_beat: 6.5,
            selected_track_id: Some(second.clone()),
        };
        sidecar.apply(&mut state);
        assert_eq!(
            state.viewport.pixels_per_second, 4000.0,
            "clamped to the zoom limit"
        );
        assert_eq!(state.transport.playhead_beats, 6.5);
        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(second.as_str())
        );
        assert_eq!(
            state.pending_view_restore,
            Some(PendingViewRestore {
                left_edge_beat: 16.0,
                scroll_y: 90.0
            })
        );
        // The capture of a session whose restore has not rendered yet keeps it.
        let captured = ViewSidecar::capture(&state);
        assert_eq!(captured.left_edge_beat, 16.0);
        assert_eq!(captured.scroll_y, 90.0);

        // A track deleted since is not selected; the selection is left alone.
        let mut other = TimelineState::default();
        other.tracks.clear();
        let _ = other.create_audio_track();
        let before = other.selection.selected_track_id.clone();
        ViewSidecar {
            selected_track_id: Some("deleted-track".to_string()),
            ..sidecar
        }
        .apply(&mut other);
        assert_eq!(other.selection.selected_track_id, before);
    }

    #[test]
    fn a_pending_restore_waits_for_a_real_viewport() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_second = 100.0;
        state.sync_pixels_per_beat();
        state.pending_view_restore = Some(PendingViewRestore {
            left_edge_beat: 8.0,
            scroll_y: 60.0,
        });
        assert_eq!(state.viewport.viewport_width, 0.0);
        assert!(!state.apply_pending_view_restore());
        assert_eq!(state.viewport.scroll_x, 0.0);
        assert!(state.pending_view_restore.is_some());

        state.update_viewport_size(1200.0, 500.0);
        assert!(state.apply_pending_view_restore());
        // 120 BPM: 8 beats are 4 s, at 100 px/s.
        assert!((state.viewport.scroll_x - 400.0).abs() < 1e-3);
        assert_eq!(state.viewport.target_scroll_x, state.viewport.scroll_x);
        assert_eq!(state.viewport.scroll_y, 60.0);
        assert_eq!(state.viewport.target_scroll_y, 60.0);
        assert!(state.pending_view_restore.is_none());
        assert!((state.x_to_beat(0.0) - 8.0).abs() < 1e-3);
        assert!(!state.apply_pending_view_restore(), "applied once");
    }

    /// With tempo automation the arrangement is laid out in real time; the left
    /// edge beat goes through the same warp the ruler draws with.
    #[test]
    fn a_pending_restore_follows_the_tempo_map() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_second = 100.0;
        state.tempo_map = TempoMap::with_points(vec![
            TempoPoint::with_id("a", 0.0, 120.0, TempoCurve::Hold),
            TempoPoint::with_id("b", 4.0, 60.0, TempoCurve::Hold),
        ]);
        state.update_viewport_size(1200.0, 500.0);
        assert!(!state.viewport.time_warp.is_linear());
        state.pending_view_restore = Some(PendingViewRestore {
            left_edge_beat: 6.0,
            scroll_y: 0.0,
        });
        assert!(state.apply_pending_view_restore());
        // Four beats at 120 (2 s) plus two at 60 (2 s): 4 s at 100 px/s.
        assert!((state.viewport.scroll_x - 400.0).abs() < 1e-2);
        assert!((state.x_to_beat(0.0) - 6.0).abs() < 1e-3);
        // And the sidecar captures that beat back.
        assert!((ViewSidecar::capture(&state).left_edge_beat - 6.0).abs() < 1e-3);
    }
}
