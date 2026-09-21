pub mod format;
pub mod import;
pub mod io;
pub mod recent;
pub mod routing_migration;
pub mod session;
pub mod template;

pub use format::{decode_project, decode_project_with_options, encode_project, ProjectError, PROJECT_MAGIC, PROJECT_VERSION};
pub use io::{
    create_project_folder, default_projects_dir, import_audio_file_to_project, load_project,
    load_project_strict, project_backup_path, project_temp_path, sanitize_project_name, save_project,
    validate_project_file, verify_project_file, LEGACY_PROJECT_FILE_EXT, PROJECT_FILE_EXT,
    SUPPORTED_PROJECT_FILE_EXTS,
};
pub use import::{is_import_path, IMPORT_PROJECT_FILE_EXTS};
pub use recent::{RecentProject, RecentProjectsStore};
pub use session::ProjectSession;
pub use template::{ProjectCreateOptions, ProjectTemplate};

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::solfege::SolfegeTrackState;
use sphere_midi_service::mpe::MpeTrackConfiguration;
use sphere_midi_service::NoteExpression;
pub use sphere_soundfont_player::{SoundfontEnvelope, SoundfontRenderQuality};

// ── Identifiers ───────────────────────────────────────────────────────────────

fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Cheap non-crypto ID: timestamp + stack address mix.
    let addr = &ts as *const _ as u64;
    format!("{:016x}{:016x}", ts as u64, addr ^ 0xDEAD_BEEF_CAFE_BABE)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ── Enumerations ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrackType {
    Audio,
    Midi,
    Instrument,
    Bus,
    Return,
    Group,
    Master,
    /// Reference/preview video lane (v33+).
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMonitorMode {
    #[default]
    Off,
    /// Monitor input whenever this mode is selected (Input).
    Always,
    /// Monitor input whenever the track is record-armed (Auto).
    WhenRecordArmed,
}

impl InputMonitorMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::WhenRecordArmed,
            Self::WhenRecordArmed => Self::Always,
            Self::Always => Self::Off,
        }
    }

    pub fn is_active(self, armed: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Always => true,
            Self::WhenRecordArmed => armed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::WhenRecordArmed => "Auto",
            Self::Always => "Input",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ClipSource {
    Audio {
        asset_id: String,
        source_path: Option<PathBuf>,
    },
    Rauf {
        asset_id: String,
        source_path: PathBuf,
        metadata_path: Option<PathBuf>,
        sample_format: String,
        sample_rate: u32,
        channels: u16,
        start_frame: u64,
        length_frames: u64,
    },
    Midi {
        notes: Vec<MidiNote>,
        controller_lanes: Vec<MidiControllerLane>,
        sysex_events: Vec<MidiSysExEvent>,
        /// Direction articulation events (v25+). Older projects have none.
        articulations: Vec<MidiArticulation>,
    },
    /// Reference video placed on the Video track (v33+). Only the media
    /// reference is stored; frames are always decoded from the source file.
    Video {
        asset_id: String,
        source_path: Option<PathBuf>,
    },
    Empty,
}

#[derive(Debug, Clone)]
pub struct MidiNote {
    /// Stable note identity (v26+). `0` on older files means "mint on load".
    pub id: u64,
    pub pitch: u8,
    pub start_beats: f32,
    pub duration_beats: f32,
    pub velocity: u8,
    /// Note Off velocity 1..=127, or `0` when unset (v26+).
    pub release_velocity: u8,
    pub muted: bool,
    /// UI-facing channel number, 1..=16. Older projects have no per-note
    /// channel data and default to 1 on load.
    pub channel: u8,
    /// Per-note articulation tag ([`ArticulationId::to_tag`]); `0` = none.
    /// Older projects (< v25) have no articulation data and default to `0`.
    pub articulation: u8,
    /// Continuous pitch performance (v38+). Empty when the note sounds at its
    /// notated pitch. Points are cent deviations keyed by beats from the note
    /// start, so they survive transposition and moves.
    pub pitch_curve: Vec<MidiPitchPoint>,
    /// Protocol-neutral per-note expression.  MPE/MIDI 2.0 are transport
    /// adapters; their channels are never part of this project data.
    pub expression: NoteExpression,
    /// Musical accent (v39+). `None` when the note has never been analysed or
    /// drawn, which is how a pre-v39 project and a freshly drawn note both
    /// load — an absent accent and a neutral one are different states and the
    /// re-analysis policy depends on telling them apart.
    pub accent: Option<MidiAccent>,
}

/// Serialized per-note accent (project format v39+). Mirrors
/// [`timeline_state::AccentState`].
///
/// Stored as five `f32` and a provenance tag rather than as a single value,
/// because the four components are independently editable and independently
/// consumed: writing only `prominence` and re-deriving the rest on load would
/// silently discard a hand-shaped accent every time the project was saved.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MidiAccent {
    pub prominence: f32,
    pub attack: f32,
    pub agogic: f32,
    pub timbre: f32,
    pub confidence: f32,
    /// [`timeline_state::AccentSource::to_tag`].
    pub source: u8,
}

/// Serialized pitch-curve breakpoint (project format v38+). Mirrors
/// [`timeline_state::PitchPoint`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MidiPitchPoint {
    /// Stable point identity; `0` on a legacy/foreign writer means "mint".
    pub id: u64,
    /// Beats from the owning note's start.
    pub beat: f32,
    /// Cent deviation from the owning note's pitch.
    pub cents: f32,
    /// [`timeline_state::PitchSegmentShape::to_tag`].
    pub shape: u8,
}

/// Serialized direction articulation event. Mirrors
/// [`timeline_state::MidiArticulationEvent`] minus the transient editor id
/// (fresh ids are minted on load, like MIDI note ids).
#[derive(Debug, Clone, PartialEq)]
pub struct MidiArticulation {
    /// Beats relative to the clip start.
    pub beat: f32,
    /// [`ArticulationId::to_tag`] value; always a valid non-zero tag on save.
    pub articulation: u8,
}

/// Serialized MIDI controller stream selector. Mirrors
/// [`timeline_state::MidiControllerKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiControllerKind {
    CC(u8),
    PitchBend,
    ChannelPressure,
    PolyPressure,
}

#[derive(Debug, Clone)]
pub struct MidiControllerPoint {
    /// Stable point identity (v26+). `0` on older files means "mint on load".
    pub id: u64,
    pub beat: f32,
    /// Normalized `0.0..=1.0`.
    pub value: f32,
}

#[derive(Debug, Clone)]
pub struct MidiControllerLane {
    pub kind: MidiControllerKind,
    pub points: Vec<MidiControllerPoint>,
    pub visible: bool,
    pub height: f32,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiSysExKind {
    Normal,
    Escaped,
}

#[derive(Debug, Clone)]
pub struct MidiSysExEvent {
    pub kind: MidiSysExKind,
    pub tick: u64,
    pub beat: f32,
    pub data: Vec<u8>,
}

use crate::components::timeline::timeline_state::MidiControllerKind as TlControllerKind;

/// Map a live controller kind to its serialized form.
fn controller_kind_to_project(k: TlControllerKind) -> MidiControllerKind {
    match k {
        TlControllerKind::CC(n) => MidiControllerKind::CC(n),
        TlControllerKind::PitchBend => MidiControllerKind::PitchBend,
        TlControllerKind::ChannelPressure => MidiControllerKind::ChannelPressure,
        TlControllerKind::PolyPressure => MidiControllerKind::PolyPressure,
    }
}

/// Map a serialized controller kind back to the live form.
fn controller_kind_from_project(k: MidiControllerKind) -> TlControllerKind {
    match k {
        MidiControllerKind::CC(n) => TlControllerKind::CC(n),
        MidiControllerKind::PitchBend => TlControllerKind::PitchBend,
        MidiControllerKind::ChannelPressure => TlControllerKind::ChannelPressure,
        MidiControllerKind::PolyPressure => TlControllerKind::PolyPressure,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFormat {
    Vst3,
    Vst2,
    Clap,
    Au,
    Lv2,
    Unknown,
}

// ── Plugin state (binary blobs — future VST/CLAP ready) ──────────────────────

/// Raw binary snapshot of a plugin's internal state. Never JSON/base64.
/// Empty `state_bytes` is valid and means "use plugin defaults".
#[derive(Debug, Clone, Default)]
pub struct PluginStateBlob {
    pub plugin_id: String,
    pub format: Option<PluginFormat>,
    pub state_bytes: Vec<u8>,
    pub vendor: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectPluginInstance {
    pub instance_id: String,
    pub format: PluginFormat,
    pub plugin_path: Option<PathBuf>,
    pub plugin_uid: String,
    pub display_name: String,
    pub state: PluginStateBlob,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectInsert {
    pub id: String,
    pub slot_index: u32,
    pub bypassed: bool,
    pub enabled_audio_output_channels: Vec<u8>,
    /// Registry-resolved plug-in role. `None` identifies a pre-v36 insert whose
    /// role must use the legacy track/slot fallback during snapshot construction.
    pub plugin_is_instrument: Option<bool>,
    /// Mixer-only collapsed/expanded view flag for this instrument's VSTi
    /// multi-out group. Visual state only — never affects routing.
    pub multiout_collapsed: bool,
    pub plugin: Option<ProjectPluginInstance>,
}

// ── Track routing ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V33TrackInputRouting {
    None,
    AllInputs,
    AudioDeviceChannel {
        device_id: String,
        channel: u32,
    },
    AudioDeviceChannels {
        device_id: String,
        channels: Vec<u32>,
    },
    MidiDevice {
        device_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectTrackOutputRouting {
    Main,
    Bus { bus_id: String },
    HardwareOutput { device_id: String, channel: u32 },
    Instrument { track_id: String },
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrackAudioFormat {
    Mono,
    Stereo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectTrackMidiInputRouting {
    None,
    AllInputs,
    MidiDevice { device_id: String },
}

#[derive(Debug, Clone)]
pub struct TrackRouting {
    /// v34+: the logical audio input bus this track references. Stored as the
    /// stable connection id string; never a device id or channel index.
    pub audio_input_connection_id: Option<String>,
    /// **v33-and-older decode only.** The combined input union as read from an
    /// old file, consumed by the migration and then dropped. Always `None` for
    /// a v34 project and never written by the v34 encoder.
    pub legacy_input: Option<V33TrackInputRouting>,
    pub output: ProjectTrackOutputRouting,
    pub audio_format: ProjectTrackAudioFormat,
    pub midi_input: ProjectTrackMidiInputRouting,
    pub midi_channel: Option<u8>,
    /// `true` plays each note back on its own channel; `false` (default)
    /// forces every note onto `midi_channel` (or channel 1). Added alongside
    /// per-note MIDI channels; missing/old data defaults to `false`, matching
    /// the pre-existing single-channel-per-track behavior.
    pub midi_output_per_note: bool,
    /// v46: per-track MPE output policy. Older projects default to Auto so
    /// existing expression curves keep their backwards-compatible playback.
    pub mpe: MpeTrackConfiguration,
    pub sends: Vec<ProjectSend>,
}

impl Default for TrackRouting {
    fn default() -> Self {
        Self {
            audio_input_connection_id: None,
            legacy_input: None,
            output: ProjectTrackOutputRouting::Main,
            audio_format: ProjectTrackAudioFormat::Stereo,
            midi_input: ProjectTrackMidiInputRouting::None,
            midi_channel: None,
            midi_output_per_note: false,
            mpe: MpeTrackConfiguration::default(),
            sends: Vec::new(),
        }
    }
}

impl TrackRouting {
    pub fn default_for_track_type(track_type: ProjectTrackType) -> Self {
        match track_type {
            ProjectTrackType::Audio => Self::default(),
            ProjectTrackType::Instrument => Self {
                midi_input: ProjectTrackMidiInputRouting::AllInputs,
                ..Self::default()
            },
            ProjectTrackType::Midi => Self {
                output: ProjectTrackOutputRouting::None,
                midi_input: ProjectTrackMidiInputRouting::AllInputs,
                ..Self::default()
            },
            ProjectTrackType::Bus
            | ProjectTrackType::Return
            | ProjectTrackType::Group
            | ProjectTrackType::Master
            // A Video track has no audio or MIDI routing at all.
            | ProjectTrackType::Video => Self::default(),
        }
    }
}

/// Persisted aux send (Phase 3). Mirrors `timeline_state::SendSlotState`
/// minus the transient resolved `target_name`.
#[derive(Debug, Clone)]
pub struct ProjectSend {
    pub id: String,
    pub target_track_id: String,
    pub enabled: bool,
    pub pre_fader: bool,
    pub gain_db: f32,
}

// ── Automation ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AutomationPoint {
    pub beat: f32,
    pub value: f32,
    /// [`AutomationCurve`](crate::components::timeline::timeline_state::AutomationCurve)
    /// tag. Persisted from project version 2 onward; defaults to Linear (0)
    /// when loading older files.
    pub curve: u8,
    /// Per-segment curve tension in `-1.0..=1.0`. Persisted from project version
    /// 21 onward; defaults to `0.0` (straight) for older files.
    pub tension: f32,
}

/// Flattened automation target descriptor for persistence. `tag` matches
/// `AutomationTarget::to_tag`; the descriptor strings are only meaningful for
/// the plugin/send variants and are empty otherwise.
#[derive(Debug, Clone, Default)]
pub struct AutomationTargetDesc {
    pub tag: u8,
    pub insert_id: String,
    pub parameter_id: String,
    pub parameter_name: String,
    pub send_id: String,
}

#[derive(Debug, Clone)]
pub struct AutomationLane {
    pub id: String,
    pub parameter_name: String,
    /// Persisted from project version 2 onward; derived from `parameter_name`
    /// for older files.
    pub target: AutomationTargetDesc,
    pub enabled: bool,
    pub points: Vec<AutomationPoint>,
    pub visible: bool,
}

// ── Tracks & clips ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProjectClip {
    pub id: String,
    pub name: String,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub offset_beats: f32,
    pub gain: f32,
    pub muted: bool,
    pub source: ClipSource,
    /// Non-destructive clip-level stretch / pitch state (persisted v16+). Loads
    /// as [`AudioClipStretchState::default`] (mode Off, ratio 1.0,
    /// preserve_pitch false) for older projects.
    pub stretch: AudioClipStretchState,
}

#[derive(Debug, Clone)]
pub struct ProjectTrack {
    pub id: String,
    pub name: String,
    pub track_type: ProjectTrackType,
    /// ARA plug-in processing this track (v42+). `None` for older projects and
    /// for tracks no plug-in owns.
    pub ara: Option<AraTrackBinding>,
    /// What the track's clips hold constant across a tempo change (v43+),
    /// as the stable `TrackTimebase` tag. Older projects are all Musical.
    pub timebase: u8,
    /// Arrangement group membership (v30+). Independent from audio routing.
    pub parent_group_id: Option<String>,
    /// Arrangement folder collapse state (v31+).
    pub group_collapsed: bool,
    /// RGBA hex string e.g. "#56C7C9". Chosen to be human-readable in the file.
    pub color_hex: String,
    pub volume_norm: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub record_arm: bool,
    pub input_monitor: InputMonitorMode,
    pub routing: TrackRouting,
    pub inserts: Vec<ProjectInsert>,
    pub automation_lanes: Vec<AutomationLane>,
    pub clips: Vec<ProjectClip>,
    /// Arrangement row height in px (v17+). `None` uses the default height.
    pub row_height_px: Option<f32>,
    /// Built-in Soundfont Player instrument state (v28+). The player is a track
    /// instrument rather than an insert, so it has no `ProjectInsert` to carry
    /// its settings.
    pub soundfont: Option<ProjectSoundfontPlayer>,
    /// Whether the persisted Track Volume automation lane drives the effective
    /// fader value (v32+). Older projects default to enabled.
    pub volume_automation_read: bool,
    /// Native Solfege instrument state (v37+).
    pub solfege: Option<ProjectSolfegeEngine>,
    /// Recorded takes on this track (v44+), oldest first. A take names one of
    /// the track's own clips, so the audio is stored once — as the clip — and
    /// this list only records which pass produced it and whether it is the one
    /// heard. Empty for older projects, which had no take list.
    pub takes: Vec<ProjectTake>,
    /// Whether the take sub-lane is open in the header (v44+).
    pub takes_expanded: bool,
}

/// One persisted recorded take. See
/// [`crate::components::timeline::timeline_state::TrackTake`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTake {
    pub id: String,
    pub name: String,
    pub clip_id: String,
    pub active: bool,
    pub recorded_at: String,
}

/// Persisted state of a track's built-in Soundfont Player.
///
/// The `.sf2` itself is referenced by absolute path, not copied into the
/// project: General MIDI banks are large, shared between projects, and often
/// live outside the project folder. A missing file loads as a track with no
/// audible instrument rather than failing the project open.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSoundfontPlayer {
    pub path: Option<PathBuf>,
    pub preset_bank: Option<i32>,
    pub preset_patch: Option<i32>,
    pub volume: f32,
    pub reverb_chorus: bool,
    pub polyphony: u32,
    /// v29: amp envelope over the player's output.
    pub envelope: SoundfontEnvelope,
    /// v29: internal synthesis oversampling.
    pub quality: SoundfontRenderQuality,
}

impl Default for ProjectSoundfontPlayer {
    fn default() -> Self {
        Self {
            path: None,
            preset_bank: None,
            preset_patch: None,
            volume: 1.0,
            reverb_chorus: true,
            polyphony: 64,
            envelope: SoundfontEnvelope::default(),
            quality: SoundfontRenderQuality::default(),
        }
    }
}

/// Persisted state of the DAW's native Solfege instrument wrapper.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSolfegeEngine {
    pub model_path: Option<PathBuf>,
    pub instrument: String,
    pub voice: String,
    pub preset: String,
    pub bow_pressure: f32,
    pub vibrato: f32,
    pub dynamics: f32,
    pub expression: f32,
    /// Visible performance-lane layout for the Solfege MIDI editor (v38+).
    /// Editor layout state; never consumed by the realtime engine.
    pub visible_lanes: Vec<ProjectSolfegeLane>,
}

/// Serialized Solfege editor lane row (project format v38+).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSolfegeLane {
    pub lane_id: String,
    pub height: f32,
}

impl Default for ProjectSolfegeEngine {
    fn default() -> Self {
        let state = crate::solfege::SolfegeTrackState::violin(None);
        Self {
            model_path: None,
            instrument: state.instrument,
            voice: state.voice,
            preset: state.preset,
            bow_pressure: state.bow_pressure,
            vibrato: state.vibrato,
            dynamics: state.dynamics,
            expression: state.expression,
            visible_lanes: state
                .visible_lanes
                .into_iter()
                .map(|lane| ProjectSolfegeLane {
                    lane_id: lane.lane_id,
                    height: lane.height,
                })
                .collect(),
        }
    }
}

// ── Mixer ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProjectMixer {
    pub master_volume_norm: f32,
    pub master_inserts: Vec<ProjectInsert>,
    /// v20: persisted mixer tree expanded node ids.
    pub tree_expanded_node_ids: Vec<String>,
    pub tree_pinned_channel_ids: Vec<String>,
    pub tree_hidden_channel_ids: Vec<String>,
}

impl Default for ProjectMixer {
    fn default() -> Self {
        Self {
            master_volume_norm: crate::components::timeline::timeline_state::volume::db_to_norm(
                0.0,
            ),
            master_inserts: Vec::new(),
            tree_expanded_node_ids: Vec::new(),
            tree_pinned_channel_ids: Vec::new(),
            tree_hidden_channel_ids: Vec::new(),
        }
    }
}

// ── Conductor lanes ──────────────────────────────────────────────────────────

/// Fold state of the global (conductor) lanes — Arranger, Markers, Tempo,
/// Signature, Song Text (v40+).
///
/// View state, like the mixer tree's expanded set: none of it reaches the
/// engine. It is persisted for the same reason a track's row height is — a
/// player who folds the tempo lane away, or drags the marker lane taller, has
/// arranged their workspace, and reopening the project should return it rather
/// than the factory defaults.
///
/// Song Text has a draggable height but no collapse latch (its header offers no
/// collapse button), so it appears in the heights and not in the flags. Lane
/// *visibility* is deliberately not here: hiding a lane is a menu command, not
/// a fold, and it is not what this block promises to restore.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectGlobalLanes {
    pub arranger_collapsed: bool,
    pub marker_collapsed: bool,
    pub tempo_collapsed: bool,
    pub time_signature_collapsed: bool,
    /// Dragged heights. `None` means the lane is at its default height, which
    /// is a distinct state from "happens to equal the default today": the
    /// default may change, and an un-dragged lane should follow it.
    pub arranger_height: Option<f32>,
    pub marker_height: Option<f32>,
    pub tempo_height: Option<f32>,
    pub time_signature_height: Option<f32>,
    pub song_text_height: Option<f32>,
}

// ── Assets ───────────────────────────────────────────────────────────────────

/// An audio (or other media) file referenced by the project.
#[derive(Debug, Clone)]
pub struct ProjectAsset {
    pub id: String,
    pub original_filename: String,
    /// Path relative to project folder root, e.g. "Media/Audio/kick.wav"
    pub relative_path: Option<String>,
    /// Absolute fallback — used when file isn't inside project folder.
    pub absolute_path: Option<PathBuf>,
    pub duration_secs: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    /// Content fingerprint (`"<len:x>-<crc:08x>"`) of the copied audio bytes.
    /// Persisted from project version 11 so re-imports of identical content can
    /// be deduplicated without re-hashing the whole asset folder on save.
    /// `None` for assets written by older versions.
    pub source_fingerprint: Option<String>,
    /// Project-relative peak cache path, e.g. `Cache/Waveforms/Assets__Audio__kick.wav.peaks`.
    pub waveform_peak_relative_path: Option<String>,
    /// Total PCM frames in the asset (v12+).
    pub duration_samples: Option<u64>,
}

/// Alias used in specs/docs for persisted audio registry entries.
pub type AudioAsset = ProjectAsset;

// ── Settings ──────────────────────────────────────────────────────────────────

/// A persisted tempo marker. `curve` is the `TempoCurve` tag (0=Hold,
/// 1=Linear, 2=Smooth). `id` is empty in v7 files and assigned on load.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTempoPoint {
    pub id: String,
    pub beat: f64,
    pub bpm: f64,
    pub curve: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTimeSignaturePoint {
    pub id: String,
    pub beat: f64,
    pub numerator: u16,
    pub denominator: u16,
    pub grouping: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTimelineMarker {
    pub id: String,
    pub beat: f64,
    pub name: String,
    pub color_hex: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTimelineRegion {
    pub id: String,
    pub start_beat: f64,
    pub end_beat: f64,
    pub name: String,
    pub color_hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectLyricSyllableMode {
    Phrase,
    Syllables,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectLyricSyllable {
    pub text: String,
    pub offset_beats: f64,
    pub duration_beats: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSongSectionType {
    Custom,
    Intro,
    Verse,
    PreChorus,
    Chorus,
    Bridge,
    Solo,
    Outro,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectSongTextEventKind {
    Chord {
        symbol: String,
    },
    Lyric {
        text: String,
        syllable_mode: ProjectLyricSyllableMode,
        continuation: bool,
        duration_beats: Option<f64>,
        syllables: Vec<ProjectLyricSyllable>,
    },
    Section {
        name: String,
        section_type: ProjectSongSectionType,
        color_hex: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSongTextEvent {
    pub id: String,
    pub beat: f64,
    pub kind: ProjectSongTextEventKind,
}

#[derive(Debug, Clone)]
pub struct ProjectSettings {
    pub bpm: f64,
    /// Project-level tempo automation markers. Empty = static tempo at `bpm`.
    pub tempo_points: Vec<ProjectTempoPoint>,
    /// Global time signature markers. Empty on disk = migrate from legacy pair.
    pub time_signature_points: Vec<ProjectTimeSignaturePoint>,
    pub timeline_markers: Vec<ProjectTimelineMarker>,
    pub timeline_regions: Vec<ProjectTimelineRegion>,
    pub song_text_events: Vec<ProjectSongTextEvent>,
    pub time_sig_num: u32,
    pub time_sig_den: u32,
    pub sample_rate: u32,
    pub bit_depth: u32,
    /// Project timebase — the unit the ruler and position readouts are shown in.
    /// Stored as the stable `TimeDisplayFormat` tag.
    pub time_display_format: u8,
    /// Frame rate Timecode is counted at, as the stable `TimecodeRate` tag.
    pub timecode_rate: u8,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature_points: Vec::new(),
            timeline_markers: Vec::new(),
            timeline_regions: Vec::new(),
            song_text_events: Vec::new(),
            time_sig_num: 4,
            time_sig_den: 4,
            sample_rate: 48000,
            bit_depth: 24,
            time_display_format:
                crate::components::timeline::timeline_state::TimeDisplayFormat::default().to_tag(),
            timecode_rate: crate::components::timeline::timeline_state::TimecodeRate::default()
                .to_tag(),
        }
    }
}

// ── Root project ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FutureboardProject {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub modified_at: u64,
    pub settings: ProjectSettings,
    pub tracks: Vec<ProjectTrack>,
    pub mixer: ProjectMixer,
    pub assets: Vec<ProjectAsset>,
    /// v34+: project Audio Connections. The single source of truth for logical
    /// audio buses; tracks reference entries here by stable id.
    pub audio_connections: Vec<ProjectAudioConnection>,
    /// v35+: the project's Master output, as an Audio Connection id. `None` is
    /// No Output. Only the id is stored — never a device, port, or channel.
    pub master_output_connection_id: Option<String>,
    /// v35+: the Monitor / Control Room output override. `None` means Follow
    /// Master Output.
    pub monitor_output_connection_id: Option<String>,
    /// v35+: latch for the one-time output-routing bootstrap, so a deliberately
    /// deleted Master output is not recreated on the next load.
    pub output_routing_initialized: bool,
    /// v40+: fold state of the conductor lanes above the arrangement.
    pub global_lanes: ProjectGlobalLanes,
    /// v41+: one saved ARA document per bound plug-in.
    pub ara_documents: Vec<ProjectAraDocument>,
}

/// One ARA plug-in's saved document state, for one track.
///
/// ARA archives a whole document, not a region, so this is stored per plug-in
/// instance rather than per clip: every ARA clip on a track shares that track's
/// document. One instance per (plug-in, track) is also what the engine's
/// per-track renderer model requires, since a renderer's output lands in exactly
/// one track's buffer.
#[derive(Debug, Clone)]
pub struct ProjectAraDocument {
    /// Catalog id of the plug-in that wrote this archive.
    pub plugin_id: String,
    /// Track whose ARA document this is.
    pub track_id: String,
    /// The plug-in's `documentArchiveID` at save time. A plug-in refuses an
    /// archive whose id it does not recognise, so restoring without checking
    /// this first would hand it bytes it cannot read.
    pub archive_id: String,
    /// Opaque plug-in bytes, stored raw and length-prefixed exactly like
    /// [`PluginStateBlob::state_bytes`]. Never JSON, never base64.
    pub data: Vec<u8>,
}

/// One persisted logical audio bus. Runtime-derived status is deliberately not
/// stored — it is recomputed from the current device inventory on load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAudioConnection {
    pub id: String,
    pub name: String,
    /// `"input"` / `"output"`.
    pub direction: String,
    /// `"mono"` / `"stereo"` / `"custom"`.
    pub channel_layout: String,
    /// Channel count, meaningful for `custom`; mono/stereo ignore it on load.
    pub channel_count: u32,
    pub device_id: Option<String>,
    /// Ordered per-logical-channel bindings. Order is semantic (Left, Right)
    /// and is never sorted.
    pub port_bindings: Vec<ProjectAudioPortBinding>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAudioPortBinding {
    pub logical_channel: u32,
    pub device_id: String,
    pub port_name: String,
    pub port_index: u32,
}

impl FutureboardProject {
    pub fn new(name: impl Into<String>) -> Self {
        let now = now_secs();
        Self {
            audio_connections: Vec::new(),
            master_output_connection_id: None,
            monitor_output_connection_id: None,
            output_routing_initialized: false,
            id: new_id(),
            name: name.into(),
            created_at: now,
            modified_at: now,
            settings: ProjectSettings::default(),
            tracks: Vec::new(),
            mixer: ProjectMixer::default(),
            assets: Vec::new(),
            global_lanes: ProjectGlobalLanes::default(),
            ara_documents: Vec::new(),
        }
    }
}

// ── Conversion helpers ────────────────────────────────────────────────────────

/// Converts a `gpui::Rgba` to a hex color string "#RRGGBB".
/// Format an `Rgba` as a stable `#RRGGBB` string. Delegates to the canonical
/// [`crate::color`] helper so there is one color implementation project-wide.
pub fn rgba_to_hex(c: gpui::Rgba) -> String {
    crate::color::rgba_to_hex(c)
}

/// Converts a hex color string to `gpui::Rgba`. Unparseable values fall back to
/// the first default-palette color rather than panicking.
pub fn hex_to_rgba(hex: &str) -> gpui::Rgba {
    crate::color::parse_hex_color(hex).unwrap_or_else(|_| crate::color::auto_color_for_index(0))
}

// ── From TimelineState ────────────────────────────────────────────────────────

pub use crate::components::timeline::timeline_state::AraTrackBinding;
use crate::components::timeline::timeline_state::{
    AudioClipStretchState, ClipType, InsertSlotState, TimelineMarkerState, TimelineRegionState,
    TimelineState, TrackType as TlTrackType,
};

fn timeline_insert_to_project(idx: usize, slot: &InsertSlotState) -> ProjectInsert {
    use crate::components::timeline::timeline_state::InsertPluginFormat;

    let plugin = slot.plugin_id.as_ref().map(|pid| {
        let format = match slot.plugin_format {
            Some(InsertPluginFormat::Vst3) => PluginFormat::Vst3,
            Some(InsertPluginFormat::Vst2) => PluginFormat::Vst2,
            Some(InsertPluginFormat::Clap) => PluginFormat::Clap,
            Some(InsertPluginFormat::Au) => PluginFormat::Au,
            Some(InsertPluginFormat::Lv2) => PluginFormat::Lv2,
            _ => PluginFormat::Unknown,
        };
        ProjectPluginInstance {
            instance_id: slot.id.clone(),
            format,
            plugin_path: slot.plugin_path.clone(),
            plugin_uid: pid.clone(),
            display_name: slot.display_name.clone(),
            state: PluginStateBlob {
                plugin_id: pid.clone(),
                format: Some(format),
                state_bytes: slot
                    .vst3_state
                    .as_ref()
                    .map(|state| state.as_ref().clone())
                    .unwrap_or_default(),
                vendor: slot.vendor.clone(),
                name: Some(slot.display_name.clone()),
                version: None,
            },
        }
    });
    ProjectInsert {
        id: slot.id.clone(),
        slot_index: idx as u32,
        bypassed: slot.bypassed,
        enabled_audio_output_channels: slot.enabled_audio_output_channels.clone(),
        plugin_is_instrument: slot.plugin_is_instrument,
        multiout_collapsed: slot.multiout_collapsed,
        plugin,
    }
}

fn project_insert_to_timeline(pi: &ProjectInsert) -> InsertSlotState {
    use crate::components::timeline::timeline_state::{
        InsertLoadStatus, InsertPluginFormat, PluginRuntimeBackend, PluginRuntimeState,
    };

    match &pi.plugin {
        Some(plugin) => {
            let plugin_format = match plugin.format {
                PluginFormat::Vst3 => InsertPluginFormat::Vst3,
                PluginFormat::Vst2 => InsertPluginFormat::Vst2,
                PluginFormat::Clap => InsertPluginFormat::Clap,
                PluginFormat::Au => InsertPluginFormat::Au,
                PluginFormat::Lv2 => InsertPluginFormat::Lv2,
                PluginFormat::Unknown => InsertPluginFormat::Unknown,
            };
            let is_builtin = SpherePluginHost::builtin_audio_bridge_supported(&plugin.plugin_uid);
            let bridge = SpherePluginHost::plugin_host_client::plugin_host_bridge_enabled()
                && (matches!(
                    plugin_format,
                    InsertPluginFormat::Vst3
                        | InsertPluginFormat::Vst2
                        | InsertPluginFormat::Clap
                        | InsertPluginFormat::Au
                ) || is_builtin);
            // Only a format with a module file can be missing from disk; an
            // Audio Unit's absence surfaces when the host tries to instantiate
            // its component id.
            let path_missing = !is_builtin
                && plugin_format.has_module_file()
                && plugin
                    .plugin_path
                    .as_ref()
                    .is_none_or(|path| !path.exists());
            let (load_status, runtime_state, runtime_backend) = if path_missing {
                (
                    InsertLoadStatus::Missing("Plugin file not found".to_string()),
                    PluginRuntimeState::Missing("Plugin file not found".to_string()),
                    if bridge {
                        PluginRuntimeBackend::ExternalBridge
                    } else {
                        PluginRuntimeBackend::InProcess
                    },
                )
            } else {
                (
                    InsertLoadStatus::Loading,
                    PluginRuntimeState::NotLoaded,
                    if bridge {
                        PluginRuntimeBackend::ExternalBridge
                    } else {
                        PluginRuntimeBackend::InProcess
                    },
                )
            };
            InsertSlotState {
                id: pi.id.clone(),
                plugin_id: Some(plugin.plugin_uid.clone()),
                plugin_path: plugin.plugin_path.clone(),
                plugin_format: Some(plugin_format),
                plugin_is_instrument: pi.plugin_is_instrument,
                vendor: plugin
                    .state
                    .vendor
                    .clone()
                    .filter(|vendor| !vendor.trim().is_empty()),
                display_name: plugin.display_name.clone(),
                enabled: true,
                bypassed: pi.bypassed,
                load_status,
                runtime_backend,
                runtime_state,
                host_pid: None,
                parameters: Vec::new(),
                enabled_audio_output_channels: pi.enabled_audio_output_channels.clone(),
                // Re-detected from the host on ProcessingPrepared after load.
                output_bus_channel_counts: Vec::new(),
                multiout_collapsed: pi.multiout_collapsed,
                pending_open_editor: false,
                vst3_state: (!plugin.state.state_bytes.is_empty())
                    .then(|| std::sync::Arc::new(plugin.state.state_bytes.clone())),
            }
        }
        None => InsertSlotState::empty(pi.id.clone()),
    }
}

impl From<&TimelineState> for FutureboardProject {
    fn from(tl: &TimelineState) -> Self {
        let tracks = tl
            .tracks
            .iter()
            // VSTi multi-out child strips (`vsti-out:{insert}:bus:{n}`) ARE
            // persisted: their ids are deterministic, so
            // `ensure_vsti_output_child_tracks` retains the loaded rows (never
            // duplicates them) once the plugin reports its bus layout, and
            // removes rows the layout no longer supports. Persisting them is
            // what carries per-bus mixer state and substrip insert chains
            // (including plugin state) across save/load.
            .map(|t| {
                let track_type = match t.track_type {
                    TlTrackType::Audio => ProjectTrackType::Audio,
                    TlTrackType::Midi => ProjectTrackType::Midi,
                    TlTrackType::Instrument => ProjectTrackType::Instrument,
                    TlTrackType::Bus => ProjectTrackType::Bus,
                    TlTrackType::Return => ProjectTrackType::Return,
                    TlTrackType::Group => ProjectTrackType::Group,
                    TlTrackType::Master => ProjectTrackType::Master,
                    TlTrackType::Video => ProjectTrackType::Video,
                };
                let clips = t
                    .clips
                    .iter()
                    .map(|c| {
                        let source = match &c.clip_type {
                            ClipType::Audio {
                                file_id,
                                source_path,
                            } => {
                                let path = source_path.as_deref().map(PathBuf::from);
                                if path
                                    .as_ref()
                                    .and_then(|p| p.extension())
                                    .and_then(|ext| ext.to_str())
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rauf"))
                                {
                                    let metadata_path = path.as_ref().map(|p| {
                                        let mut value = p.as_os_str().to_os_string();
                                        value.push(".json");
                                        PathBuf::from(value)
                                    });
                                    ClipSource::Rauf {
                                        asset_id: file_id.clone(),
                                        source_path: path.unwrap_or_default(),
                                        metadata_path,
                                        sample_format: "s32le".to_string(),
                                        sample_rate: 48_000,
                                        channels: 0,
                                        start_frame: 0,
                                        length_frames: 0,
                                    }
                                } else {
                                    ClipSource::Audio {
                                        asset_id: file_id.clone(),
                                        source_path: path,
                                    }
                                }
                            }
                            ClipType::Midi {
                                notes,
                                controller_lanes,
                                sysex_events,
                                articulations,
                            } => ClipSource::Midi {
                                notes: notes
                                    .iter()
                                    .map(|n| MidiNote {
                                        id: n.id,
                                        pitch: n.pitch,
                                        start_beats: n.start,
                                        duration_beats: n.duration,
                                        velocity: n.velocity,
                                        release_velocity: n.release_velocity.unwrap_or(0),
                                        muted: n.muted,
                                        channel: n.channel.ui(),
                                        articulation: n
                                            .articulation
                                            .map(|a| a.to_tag())
                                            .unwrap_or(0),
                                        pitch_curve: n
                                            .pitch_curve
                                            .as_ref()
                                            .map(|curve| {
                                                curve
                                                    .points
                                                    .iter()
                                                    .map(|p| MidiPitchPoint {
                                                        id: p.id,
                                                        beat: p.beat,
                                                        cents: p.cents,
                                                        shape: p.shape.to_tag(),
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default(),
                                        expression: n.expression.clone(),
                                        accent: n.accent.map(|accent| MidiAccent {
                                            prominence: accent.prominence,
                                            attack: accent.attack,
                                            agogic: accent.agogic,
                                            timbre: accent.timbre,
                                            confidence: accent.confidence,
                                            source: accent.source.to_tag(),
                                        }),
                                    })
                                    .collect(),
                                controller_lanes: controller_lanes
                                    .iter()
                                    .map(|lane| MidiControllerLane {
                                        kind: controller_kind_to_project(lane.kind),
                                        points: lane
                                            .points
                                            .iter()
                                            .map(|p| MidiControllerPoint {
                                                id: p.id,
                                                beat: p.beat,
                                                value: p.value,
                                            })
                                            .collect(),
                                        visible: lane.visible,
                                        height: lane.height,
                                        collapsed: lane.collapsed,
                                    })
                                    .collect(),
                                sysex_events: sysex_events
                                    .iter()
                                    .map(|event| MidiSysExEvent {
                                        kind: match event.kind {
                                            crate::components::timeline::timeline_state::MidiSysExKind::Normal => {
                                                MidiSysExKind::Normal
                                            }
                                            crate::components::timeline::timeline_state::MidiSysExKind::Escaped => {
                                                MidiSysExKind::Escaped
                                            }
                                        },
                                        tick: event.tick,
                                        beat: event.beat,
                                        data: event.data.clone(),
                                    })
                                    .collect(),
                                articulations: articulations
                                    .iter()
                                    .map(|event| MidiArticulation {
                                        beat: event.beat,
                                        articulation: event.articulation.to_tag(),
                                    })
                                    .collect(),
                            },
                            ClipType::Video {
                                file_id,
                                source_path,
                            } => ClipSource::Video {
                                asset_id: file_id.clone(),
                                source_path: source_path.as_deref().map(PathBuf::from),
                            },
                        };
                        ProjectClip {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            start_beat: c.start_beat as f64,
                            duration_beats: c.duration_beats as f64,
                            offset_beats: c.offset_beats,
                            gain: c.gain,
                            muted: c.muted,
                            source,
                            stretch: c.stretch.clone(),
                        }
                    })
                    .collect();
                let automation_lanes = t
                    .automation_lanes
                    .iter()
                    .map(|al| AutomationLane {
                        id: al.id.clone(),
                        parameter_name: al.name.clone(),
                        target: target_to_desc(&al.target),
                        enabled: al.enabled,
                        points: al
                            .points
                            .iter()
                            .map(|p| AutomationPoint {
                                beat: p.beat,
                                value: p.value,
                                curve: p.curve.to_tag(),
                                tension: p.tension,
                            })
                            .collect(),
                        visible: al.visible,
                    })
                    .collect();
                ProjectTrack {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    track_type,
                    ara: t.ara.clone(),
                    timebase: t.timebase.to_tag(),
                    parent_group_id: t.parent_group_id.clone(),
                    group_collapsed: t.group_collapsed,
                    color_hex: rgba_to_hex(t.color),
                    volume_norm: t.volume,
                    pan: t.pan,
                    muted: t.muted,
                    solo: t.solo,
                    record_arm: t.armed,
                    input_monitor: t.input_monitor,
                    routing: TrackRouting {
                        // v34 persists the audio and MIDI sides independently;
                        // there is no lossy combined field any more.
                        audio_input_connection_id: t
                            .routing
                            .audio_input_connection_id
                            .as_ref()
                            .map(|id| id.as_str().to_string()),
                        legacy_input: None,
                        output: timeline_output_to_project(&t.routing.output),
                        audio_format: timeline_audio_format_to_project(t.routing.audio_format),
                        midi_input: timeline_midi_input_to_project(&t.routing.midi_input),
                        midi_channel: t.routing.midi_channel.map(|ch| ch.clamp(1, 16)),
                        midi_output_per_note: t.routing.midi_output_per_note,
                        mpe: t.routing.mpe.sanitized(),
                        sends: t
                            .sends
                            .iter()
                            .map(|s| ProjectSend {
                                id: s.id.clone(),
                                target_track_id: s.target_track_id.clone(),
                                enabled: s.enabled,
                                pre_fader: s.pre_fader,
                                gain_db: s.gain_db,
                            })
                            .collect(),
                    },
                    inserts: t
                        .inserts
                        .iter()
                        .enumerate()
                        .map(|(idx, slot)| timeline_insert_to_project(idx, slot))
                        .collect(),
                    automation_lanes,
                    clips,
                    row_height_px: tl.track_view_layout.height_for(&t.id).filter(|h| {
                        (*h - crate::components::timeline::timeline_state::DEFAULT_TRACK_HEIGHT)
                            .abs()
                            >= 0.01
                    }),
                    soundfont: t.builtin_soundfont_player.then(|| ProjectSoundfontPlayer {
                        path: t.soundfont_path.as_ref().map(PathBuf::from),
                        preset_bank: t.soundfont_preset.map(|(bank, _)| bank),
                        preset_patch: t.soundfont_preset.map(|(_, patch)| patch),
                        volume: t.soundfont_volume,
                        reverb_chorus: t.soundfont_reverb_chorus,
                        polyphony: t.soundfont_polyphony as u32,
                        envelope: t.soundfont_envelope,
                        quality: t.soundfont_quality,
                    }),
                    volume_automation_read: t.volume_automation_read,
                    solfege: t.solfege.as_ref().map(|state| ProjectSolfegeEngine {
                        model_path: state.model_path.as_ref().map(PathBuf::from),
                        instrument: state.instrument.clone(),
                        voice: state.voice.clone(),
                        preset: state.preset.clone(),
                        bow_pressure: state.bow_pressure,
                        vibrato: state.vibrato,
                        dynamics: state.dynamics,
                        expression: state.expression,
                        visible_lanes: state
                            .visible_lanes
                            .iter()
                            .map(|lane| ProjectSolfegeLane {
                                lane_id: lane.lane_id.clone(),
                                height: lane.height,
                            })
                            .collect(),
                    }),
                    takes: t
                        .takes
                        .iter()
                        .map(|take| ProjectTake {
                            id: take.id.clone(),
                            name: take.name.clone(),
                            clip_id: take.clip_id.clone(),
                            active: take.active,
                            recorded_at: take.recorded_at.clone(),
                        })
                        .collect(),
                    takes_expanded: t.takes_expanded,
                }
            })
            .collect();
        let mut project = FutureboardProject::new("Untitled Project");
        project.settings.bpm = tl.bpm as f64;
        project.settings.sample_rate = tl.project_sample_rate;
        project.settings.time_display_format = tl.time_display_format.to_tag();
        project.settings.timecode_rate = tl.timecode_rate.to_tag();
        project.settings.tempo_points = tl
            .tempo_map
            .points
            .iter()
            .map(|p| ProjectTempoPoint {
                id: p.id.clone(),
                beat: p.beat,
                bpm: p.bpm,
                curve: p.curve.to_tag(),
            })
            .collect();
        project.settings.time_signature_points = tl
            .time_signature_map
            .points
            .iter()
            .map(|p| ProjectTimeSignaturePoint {
                id: p.id.clone(),
                beat: p.beat,
                numerator: p.numerator,
                denominator: p.denominator,
                grouping: p.effective_grouping(),
            })
            .collect();
        project.settings.timeline_markers = tl
            .markers
            .iter()
            .map(|marker| ProjectTimelineMarker {
                id: marker.id.clone(),
                beat: marker.beat,
                name: marker.name.clone(),
                color_hex: marker.color_hex.clone(),
            })
            .collect();
        project.settings.timeline_regions = tl
            .regions
            .iter()
            .map(|region| ProjectTimelineRegion {
                id: region.id.clone(),
                start_beat: region.start_beat,
                end_beat: region.end_beat,
                name: region.name.clone(),
                color_hex: region.color_hex.clone(),
            })
            .collect();
        project.settings.song_text_events = tl
            .song_text_events
            .iter()
            .map(|event| ProjectSongTextEvent {
                id: event.id.clone(),
                beat: event.beat,
                kind: match &event.kind {
                    crate::components::timeline::timeline_state::SongTextEventKind::Chord(
                        chord,
                    ) => ProjectSongTextEventKind::Chord {
                        symbol: chord.symbol.clone(),
                    },
                    crate::components::timeline::timeline_state::SongTextEventKind::Lyric(
                        lyric,
                    ) => ProjectSongTextEventKind::Lyric {
                        text: lyric.text.clone(),
                        syllable_mode: match lyric.syllable_mode {
                            crate::components::timeline::timeline_state::LyricSyllableMode::Phrase => {
                                ProjectLyricSyllableMode::Phrase
                            }
                            crate::components::timeline::timeline_state::LyricSyllableMode::Syllables => {
                                ProjectLyricSyllableMode::Syllables
                            }
                        },
                        continuation: lyric.continuation,
                        duration_beats: lyric.duration_beats,
                        syllables: lyric
                            .syllables
                            .iter()
                            .map(|syllable| ProjectLyricSyllable {
                                text: syllable.text.clone(),
                                offset_beats: syllable.offset_beats,
                                duration_beats: syllable.duration_beats,
                            })
                            .collect(),
                    },
                    crate::components::timeline::timeline_state::SongTextEventKind::Section(
                        section,
                    ) => ProjectSongTextEventKind::Section {
                        name: section.name.clone(),
                        section_type: match section.section_type {
                            crate::components::timeline::timeline_state::SongSectionType::Custom => {
                                ProjectSongSectionType::Custom
                            }
                            crate::components::timeline::timeline_state::SongSectionType::Intro => {
                                ProjectSongSectionType::Intro
                            }
                            crate::components::timeline::timeline_state::SongSectionType::Verse => {
                                ProjectSongSectionType::Verse
                            }
                            crate::components::timeline::timeline_state::SongSectionType::PreChorus => {
                                ProjectSongSectionType::PreChorus
                            }
                            crate::components::timeline::timeline_state::SongSectionType::Chorus => {
                                ProjectSongSectionType::Chorus
                            }
                            crate::components::timeline::timeline_state::SongSectionType::Bridge => {
                                ProjectSongSectionType::Bridge
                            }
                            crate::components::timeline::timeline_state::SongSectionType::Solo => {
                                ProjectSongSectionType::Solo
                            }
                            crate::components::timeline::timeline_state::SongSectionType::Outro => {
                                ProjectSongSectionType::Outro
                            }
                        },
                        color_hex: section.color_hex.clone(),
                    },
                },
            })
            .collect();
        project.settings.time_sig_num = tl.time_signature_num;
        project.settings.time_sig_den = tl.time_signature_den;
        project.tracks = tracks;
        project.mixer.master_volume_norm = tl.master.volume;
        project.mixer.master_inserts = tl
            .master
            .inserts
            .iter()
            .enumerate()
            .map(|(idx, slot)| timeline_insert_to_project(idx, slot))
            .collect();
        project.mixer.tree_expanded_node_ids = tl.mixer_tree.expanded_list();
        project.mixer.tree_pinned_channel_ids = tl.mixer_tree.pinned_list();
        project.mixer.tree_hidden_channel_ids = tl.mixer_tree.hidden_list();
        project.audio_connections = audio_connections_to_project(&tl.audio_connections);
        // Ids only. The resolved route, runtime device index, hardware owner,
        // and channel list all describe the current machine and are recompiled
        // on load rather than saved.
        project.master_output_connection_id = tl
            .master_output_connection_id
            .as_ref()
            .map(|id| id.as_str().to_string());
        project.monitor_output_connection_id = tl
            .monitor_output_connection_id
            .as_ref()
            .map(|id| id.as_str().to_string());
        project.output_routing_initialized = tl.output_routing_initialized;
        project.global_lanes = ProjectGlobalLanes {
            arranger_collapsed: tl.region_track_collapsed,
            marker_collapsed: tl.marker_track_collapsed,
            tempo_collapsed: tl.tempo_track_collapsed,
            time_signature_collapsed: tl.time_signature_track_collapsed,
            arranger_height: tl.global_lane_heights.region,
            marker_height: tl.global_lane_heights.marker,
            tempo_height: tl.global_lane_heights.tempo,
            time_signature_height: tl.global_lane_heights.time_signature,
            song_text_height: tl.global_lane_heights.song_text,
        };
        project
    }
}

/// Apply a loaded `FutureboardProject` back onto an existing `TimelineState`.
/// Apply a decoded project to the timeline state.
///
/// Returns any [`ProjectLoadWarning`]s raised while converting routing. Loading
/// always completes; the caller decides how to surface them.
#[must_use]
pub fn apply_to_timeline(
    project: &FutureboardProject,
    tl: &mut TimelineState,
) -> Vec<ProjectLoadWarning> {
    // Audio Connections generated while migrating v33 routing. Populated as
    // tracks are converted below, then installed on the timeline state.
    let migration_ports = crate::audio_connections::current_available_ports();
    // v34 files carry the registry; v33 files start empty and have it filled in
    // as tracks are migrated. Either way the registry exists before any track
    // reference is resolved.
    let mut migrated_connections = project_to_audio_connections(&project.audio_connections);
    let mut migration_warnings: Vec<crate::project::routing_migration::RoutingMigrationWarning> =
        Vec::new();
    let mut unresolved_connections: Vec<(String, String)> = Vec::new();
    use crate::components::timeline::timeline_state::{
        AccentSource as TlAccentSource, AccentState as TlAccentState, AutomationLaneState,
        AutomationPoint as TlAutoPoint, ClipState, MidiChannel,
        MidiControllerLane as TlControllerLane, MidiControllerPoint as TlControllerPoint,
        MidiNoteState, PitchCurve as TlPitchCurve, PitchPoint as TlPitchPoint,
        PitchSegmentShape as TlPitchSegmentShape, SendSlotState, TrackState,
    };

    tl.bpm = project.settings.bpm as f32;
    tl.project_sample_rate = match project.settings.sample_rate {
        44_100 | 48_000 | 88_200 | 96_000 | 192_000 => project.settings.sample_rate,
        _ => 48_000,
    };
    // Unknown tags fall back to the defaults rather than to whichever variant
    // happens to be first, so a project written by a newer build opens readable.
    tl.time_display_format =
        crate::components::timeline::timeline_state::TimeDisplayFormat::from_tag(
            project.settings.time_display_format,
        );
    tl.timecode_rate = crate::components::timeline::timeline_state::TimecodeRate::from_tag(
        project.settings.timecode_rate,
    );
    tl.tempo_map = crate::components::timeline::timeline_state::TempoMap::with_points(
        project
            .settings
            .tempo_points
            .iter()
            .map(|p| {
                crate::components::timeline::timeline_state::TempoPoint::with_id(
                    p.id.clone(),
                    p.beat,
                    p.bpm,
                    crate::components::timeline::timeline_state::TempoCurve::from_tag(p.curve),
                )
            })
            .collect(),
    );
    tl.tempo_map.ensure_point_ids();
    tl.markers = project
        .settings
        .timeline_markers
        .iter()
        .map(|marker| {
            TimelineMarkerState::with_id(
                marker.id.clone(),
                marker.beat,
                marker.name.clone(),
                marker.color_hex.clone(),
            )
        })
        .collect();
    tl.markers
        .sort_by(|a, b| a.beat.total_cmp(&b.beat).then_with(|| a.id.cmp(&b.id)));
    tl.regions = project
        .settings
        .timeline_regions
        .iter()
        .map(|region| {
            TimelineRegionState::with_id(
                region.id.clone(),
                region.start_beat,
                region.end_beat,
                region.name.clone(),
                region.color_hex.clone(),
            )
        })
        .collect();
    tl.regions.sort_by(|a, b| {
        a.start_beat
            .total_cmp(&b.start_beat)
            .then_with(|| a.id.cmp(&b.id))
    });
    let song_text_events = project
        .settings
        .song_text_events
        .iter()
        .filter_map(|event| {
            use crate::components::timeline::timeline_state::{
                ChordEvent, LyricEvent, LyricSyllable, LyricSyllableMode, SectionEvent,
                SongSectionType, SongTextEvent, SongTextEventKind,
            };

            let kind = match &event.kind {
                ProjectSongTextEventKind::Chord { symbol } => {
                    SongTextEventKind::Chord(ChordEvent {
                        symbol: symbol.clone(),
                    })
                }
                ProjectSongTextEventKind::Lyric {
                    text,
                    syllable_mode,
                    continuation,
                    duration_beats,
                    syllables,
                } => SongTextEventKind::Lyric(LyricEvent {
                    text: text.clone(),
                    syllable_mode: match syllable_mode {
                        ProjectLyricSyllableMode::Phrase => LyricSyllableMode::Phrase,
                        ProjectLyricSyllableMode::Syllables => LyricSyllableMode::Syllables,
                    },
                    continuation: *continuation,
                    duration_beats: *duration_beats,
                    syllables: syllables
                        .iter()
                        .map(|syllable| LyricSyllable {
                            text: syllable.text.clone(),
                            offset_beats: syllable.offset_beats,
                            duration_beats: syllable.duration_beats,
                        })
                        .collect(),
                }),
                ProjectSongTextEventKind::Section {
                    name,
                    section_type,
                    color_hex,
                } => SongTextEventKind::Section(SectionEvent {
                    name: name.clone(),
                    section_type: match section_type {
                        ProjectSongSectionType::Custom => SongSectionType::Custom,
                        ProjectSongSectionType::Intro => SongSectionType::Intro,
                        ProjectSongSectionType::Verse => SongSectionType::Verse,
                        ProjectSongSectionType::PreChorus => SongSectionType::PreChorus,
                        ProjectSongSectionType::Chorus => SongSectionType::Chorus,
                        ProjectSongSectionType::Bridge => SongSectionType::Bridge,
                        ProjectSongSectionType::Solo => SongSectionType::Solo,
                        ProjectSongSectionType::Outro => SongSectionType::Outro,
                    },
                    color_hex: color_hex.clone(),
                }),
            };
            SongTextEvent::with_id(event.id.clone(), event.beat, kind)
        })
        .collect();
    tl.replace_song_text_events(song_text_events);
    if project.settings.time_signature_points.is_empty() {
        tl.time_signature_map =
            crate::components::timeline::timeline_state::TimeSignatureMap::with_default_4_4();
        tl.time_signature_map.points[0].numerator =
            project.settings.time_sig_num.clamp(1, 64) as u16;
        tl.time_signature_map.points[0].denominator =
            project.settings.time_sig_den.clamp(1, 32) as u16;
    } else {
        tl.time_signature_map =
            crate::components::timeline::timeline_state::TimeSignatureMap::with_points(
                project
                    .settings
                    .time_signature_points
                    .iter()
                    .map(|p| {
                        crate::components::timeline::timeline_state::TimeSignaturePoint::with_grouping(
                            p.id.clone(),
                            p.beat,
                            p.numerator,
                            p.denominator,
                            p.grouping.clone(),
                        )
                    })
                    .collect(),
            );
        tl.time_signature_map.ensure_point_ids();
    }
    tl.sync_legacy_time_signature_fields();
    tl.master.volume = project.mixer.master_volume_norm;
    tl.master.inserts = project
        .mixer
        .master_inserts
        .iter()
        .map(project_insert_to_timeline)
        .collect();
    tl.mixer_tree =
        crate::components::timeline::timeline_state::MixerTreeViewState::from_project_lists(
            &project.mixer.tree_expanded_node_ids,
            &project.mixer.tree_pinned_channel_ids,
            &project.mixer.tree_hidden_channel_ids,
        );

    // Conductor lane fold state. Heights go through `set`, which clamps to the
    // drag limits, so a hand-edited or newer file cannot restore a lane tall
    // enough to push the arrangement off screen.
    tl.region_track_collapsed = project.global_lanes.arranger_collapsed;
    tl.marker_track_collapsed = project.global_lanes.marker_collapsed;
    tl.tempo_track_collapsed = project.global_lanes.tempo_collapsed;
    tl.time_signature_track_collapsed = project.global_lanes.time_signature_collapsed;
    {
        use crate::components::timeline::timeline_state::{GlobalLaneHeights, GlobalLaneKind};
        let mut heights = GlobalLaneHeights::default();
        heights.set(
            GlobalLaneKind::Arranger,
            project.global_lanes.arranger_height,
        );
        heights.set(GlobalLaneKind::Marker, project.global_lanes.marker_height);
        heights.set(GlobalLaneKind::Tempo, project.global_lanes.tempo_height);
        heights.set(
            GlobalLaneKind::TimeSignature,
            project.global_lanes.time_signature_height,
        );
        heights.set(
            GlobalLaneKind::SongText,
            project.global_lanes.song_text_height,
        );
        tl.global_lane_heights = heights;
    }

    tl.tracks = project
        .tracks
        .iter()
        .map(|pt| {
            let track_type = match pt.track_type {
                ProjectTrackType::Audio => TlTrackType::Audio,
                ProjectTrackType::Midi => TlTrackType::Midi,
                ProjectTrackType::Instrument => TlTrackType::Instrument,
                ProjectTrackType::Bus => TlTrackType::Bus,
                ProjectTrackType::Return => TlTrackType::Return,
                ProjectTrackType::Group => TlTrackType::Group,
                ProjectTrackType::Master => TlTrackType::Master,
                ProjectTrackType::Video => TlTrackType::Video,
            };
            let clips = pt
                .clips
                .iter()
                .map(|pc| {
                    let clip_type = match &pc.source {
                        ClipSource::Audio {
                            asset_id,
                            source_path,
                        } => ClipType::Audio {
                            file_id: asset_id.clone(),
                            source_path: source_path
                                .as_ref()
                                .map(|p| p.to_string_lossy().into_owned()),
                        },
                        ClipSource::Rauf {
                            asset_id,
                            source_path,
                            ..
                        } => ClipType::Audio {
                            file_id: asset_id.clone(),
                            source_path: Some(source_path.to_string_lossy().into_owned()),
                        },
                        ClipSource::Midi {
                            notes,
                            controller_lanes,
                            sysex_events,
                            articulations,
                        } => ClipType::Midi {
                            notes: notes
                                .iter()
                                .map(|n| {
                                    let mut note = MidiNoteState::from_persisted(
                                        n.id,
                                        n.pitch,
                                        n.start_beats,
                                        n.duration_beats,
                                        n.velocity,
                                        if n.release_velocity == 0 {
                                            None
                                        } else {
                                            Some(n.release_velocity)
                                        },
                                    );
                                    note.muted = n.muted;
                                    note.channel = MidiChannel::from_ui(n.channel);
                                    note.articulation =
                                        crate::components::timeline::timeline_state::ArticulationId::from_tag(
                                            n.articulation,
                                        );
                                    note.pitch_curve = (!n.pitch_curve.is_empty()).then(|| {
                                        TlPitchCurve::from_points(
                                            n.pitch_curve
                                                .iter()
                                                .map(|p| {
                                                    TlPitchPoint::from_persisted(
                                                        p.id,
                                                        p.beat,
                                                        p.cents,
                                                        TlPitchSegmentShape::from_tag(p.shape),
                                                    )
                                                })
                                                .collect(),
                                        )
                                    });
                                    note.expression = n.expression.clone();
                                    note.accent = n.accent.map(|accent| {
                                        TlAccentState {
                                            prominence: accent.prominence,
                                            attack: accent.attack,
                                            agogic: accent.agogic,
                                            timbre: accent.timbre,
                                            confidence: accent.confidence,
                                            source: TlAccentSource::from_tag(accent.source),
                                        }
                                        .sanitized()
                                    });
                                    note
                                })
                                .collect(),
                            controller_lanes: controller_lanes
                                .iter()
                                .map(|lane| TlControllerLane {
                                    kind: controller_kind_from_project(lane.kind),
                                    points: lane
                                        .points
                                        .iter()
                                        .map(|p| {
                                            TlControllerPoint::from_persisted(p.id, p.beat, p.value)
                                        })
                                        .collect(),
                                    visible: lane.visible,
                                    height: lane.height,
                                    collapsed: lane.collapsed,
                                })
                                .collect(),
                            sysex_events: sysex_events
                                .iter()
                                .map(|event| crate::components::timeline::timeline_state::MidiSysExEvent {
                                    kind: match event.kind {
                                        MidiSysExKind::Normal => crate::components::timeline::timeline_state::MidiSysExKind::Normal,
                                        MidiSysExKind::Escaped => crate::components::timeline::timeline_state::MidiSysExKind::Escaped,
                                    },
                                    tick: event.tick,
                                    beat: event.beat,
                                    data: event.data.clone(),
                                })
                                .collect(),
                            // Fresh transient event ids on load, like note ids.
                            // Unknown tags from newer files degrade to "none".
                            articulations: articulations
                                .iter()
                                .filter_map(|event| {
                                    crate::components::timeline::timeline_state::ArticulationId::from_tag(
                                        event.articulation,
                                    )
                                    .map(|articulation| {
                                        crate::components::timeline::timeline_state::MidiArticulationEvent::new(
                                            event.beat,
                                            articulation,
                                        )
                                    })
                                })
                                .collect(),
                        },
                        ClipSource::Video {
                            asset_id,
                            source_path,
                        } => ClipType::Video {
                            file_id: asset_id.clone(),
                            source_path: source_path
                                .as_ref()
                                .map(|p| p.to_string_lossy().into_owned()),
                        },
                        ClipSource::Empty => ClipType::Midi {
                            notes: Vec::new(),
                            controller_lanes: Vec::new(),
                            sysex_events: Vec::new(),
                            articulations: Vec::new(),
                        },
                    };
                    ClipState {
                        id: pc.id.clone(),
                        name: pc.name.clone(),
                        start_beat: pc.start_beat as f32,
                        duration_beats: pc.duration_beats as f32,
                        source_duration_seconds: match &pc.source {
                            ClipSource::Audio { asset_id, .. }
                            | ClipSource::Rauf { asset_id, .. } => project
                                .assets
                                .iter()
                                .find(|asset| asset.id == *asset_id)
                                .and_then(|asset| asset.duration_secs),
                            _ => None,
                        },
                        offset_beats: pc.offset_beats,
                        gain: pc.gain,
                        clip_type,
                        muted: pc.muted,
                        audio_import: crate::components::timeline::timeline_state::AudioImportState::default(),
                        stretch: pc.stretch.clone(),
                    }
                })
                .collect();
            let automation_lanes = pt
                .automation_lanes
                .iter()
                .map(|al| AutomationLaneState {
                    id: al.id.clone(),
                    name: al.parameter_name.clone(),
                    target: desc_to_target(&al.target, &al.parameter_name),
                    enabled: al.enabled,
                    visible: al.visible,
                    points: al
                        .points
                        .iter()
                        .map(|p| {
                            let mut point = TlAutoPoint::with_curve(
                                p.beat,
                                p.value,
                                crate::components::timeline::timeline_state::AutomationCurve::from_tag(
                                    p.curve,
                                ),
                            );
                            point.set_tension(p.tension);
                            point
                        })
                        .collect(),
                })
                .collect();
            let inserts: Vec<InsertSlotState> = pt
                .inserts
                .iter()
                .map(project_insert_to_timeline)
                .collect();
            let sends = pt
                .routing
                .sends
                .iter()
                .map(|s| {
                    let target_name = project
                        .tracks
                        .iter()
                        .find(|t| t.id == s.target_track_id)
                        .map(|t| t.name.clone())
                        .unwrap_or_else(|| s.target_track_id.clone());
                    SendSlotState {
                        id: s.id.clone(),
                        target_track_id: s.target_track_id.clone(),
                        target_name,
                        enabled: s.enabled,
                        pre_fader: s.pre_fader,
                        gain_db: s.gain_db,
                    }
                })
                .collect();
            let instrument_plugin_instance_id = match track_type {
                // A Soundfont Player / Solfege track sounds through the track
                // itself; its inserts are effects, so none is "the instrument".
                _ if pt.soundfont.is_some() || pt.solfege.is_some() => None,
                crate::components::timeline::timeline_state::TrackType::Instrument
                | crate::components::timeline::timeline_state::TrackType::Midi => inserts
                    .iter()
                    .find(|slot| slot.plugin_is_instrument == Some(true))
                    .or_else(|| {
                        inserts
                            .iter()
                            .all(|slot| slot.plugin_is_instrument.is_none())
                            .then(|| inserts.first())
                            .flatten()
                    })
                    .filter(|slot| slot.plugin_id.is_some())
                    .map(|slot| slot.id.clone()),
                _ => None,
            };
            TrackState {
                listen: crate::components::timeline::timeline_state::ListenMode::Off,
                id: pt.id.clone(),
                name: pt.name.clone(),
                track_type,
                ara: pt.ara.clone(),
                timebase: crate::components::timeline::timeline_state::TrackTimebase::from_tag(
                    pt.timebase,
                ),
                parent_group_id: pt.parent_group_id.clone(),
                group_collapsed: pt.group_collapsed,
                color: hex_to_rgba(&pt.color_hex),
                volume: pt.volume_norm,
                // Effective volume is derived (recomputed from automation at the
                // playhead after load); seed it from the persisted base so the
                // first frame before any recompute shows the saved value.
                volume_effective: pt.volume_norm,
                volume_automation_read: pt.volume_automation_read,
                pan: pt.pan,
                muted: pt.muted,
                solo: pt.solo,
                armed: pt.record_arm,
                input_monitor: pt.input_monitor,
                meter_level_l: 0.0,
                meter_level_r: 0.0,
                meter_peak_hold_l: 0.0,
                meter_peak_hold_r: 0.0,
                meter_clip: false,
                clips,
                automation_lanes,
                lane_mode: crate::components::timeline::timeline_state::TrackLaneMode::Clips,
                selected_automation_target: None,
                inserts,
                sends,
                routing: {
                    let mut routing = project_routing_to_timeline(&pt.routing, track_type);
                    match &pt.routing.legacy_input {
                        // v33 and older: convert the combined field. Present
                        // only when the decoder read an old file.
                        Some(legacy) => {
                            let (connection_id, midi_input) = legacy_routing_to_runtime(
                                legacy,
                                routing.midi_input.clone(),
                                track_type,
                                &pt.id,
                                &pt.name,
                                &mut migrated_connections,
                                &migration_ports,
                                &mut migration_warnings,
                            );
                            routing.audio_input_connection_id = connection_id;
                            routing.midi_input = midi_input;
                        }
                        // v34: the id was stored directly. Resolve it against
                        // the registry decoded above; an unknown id is reported
                        // and left unassigned rather than bound to another bus.
                        None => {
                            routing.audio_input_connection_id = pt
                                .routing
                                .audio_input_connection_id
                                .as_ref()
                                .map(|raw| {
                                    crate::audio_connections::AudioConnectionId::from_stored(
                                        raw.clone(),
                                    )
                                })
                                .filter(|id| {
                                    let known = migrated_connections.get(id).is_some();
                                    if !known {
                                        unresolved_connections
                                            .push((pt.id.clone(), pt.name.clone()));
                                    }
                                    known
                                });
                        }
                    }
                    routing
                },
                instrument_plugin_instance_id,
                builtin_soundfont_player: pt.soundfont.is_some(),
                soundfont_path: pt
                    .soundfont
                    .as_ref()
                    .and_then(|sf| sf.path.as_ref())
                    .map(|path| path.to_string_lossy().into_owned()),
                soundfont_preset: pt
                    .soundfont
                    .as_ref()
                    .and_then(|sf| sf.preset_bank.zip(sf.preset_patch)),
                soundfont_volume: pt
                    .soundfont
                    .as_ref()
                    .map(|sf| sf.volume.clamp(0.0, 1.0))
                    .unwrap_or(1.0),
                soundfont_reverb_chorus: pt
                    .soundfont
                    .as_ref()
                    .map(|sf| sf.reverb_chorus)
                    .unwrap_or(true),
                soundfont_polyphony: pt
                    .soundfont
                    .as_ref()
                    .map(|sf| sf.polyphony.clamp(1, 256) as usize)
                    .unwrap_or(64),
                soundfont_envelope: pt
                    .soundfont
                    .as_ref()
                    .map(|sf| sf.envelope.sanitized())
                    .unwrap_or_default(),
                soundfont_quality: pt
                    .soundfont
                    .as_ref()
                    .map(|sf| sf.quality)
                    .unwrap_or_default(),
                takes: pt
                    .takes
                    .iter()
                    .map(|take| {
                        crate::components::timeline::timeline_state::TrackTake {
                            id: take.id.clone(),
                            name: take.name.clone(),
                            clip_id: take.clip_id.clone(),
                            active: take.active,
                            recorded_at: take.recorded_at.clone(),
                        }
                    })
                    .collect(),
                takes_expanded: pt.takes_expanded,
                solfege: pt.solfege.as_ref().map(|state| SolfegeTrackState {
                    model_path: state
                        .model_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    instrument: state.instrument.clone(),
                    voice: state.voice.clone(),
                    preset: state.preset.clone(),
                    bow_pressure: state.bow_pressure,
                    vibrato: state.vibrato,
                    dynamics: state.dynamics,
                    expression: state.expression,
                    visible_lanes: state
                        .visible_lanes
                        .iter()
                        .map(|lane| {
                            crate::solfege::SolfegeLaneVisibility {
                                lane_id: lane.lane_id.clone(),
                                height: lane.height,
                            }
                            .sanitized()
                        })
                        .collect(),
                }),
            }
        })
        .collect();

    let valid_group_ids: std::collections::HashSet<String> = tl
        .tracks
        .iter()
        .filter(|track| track.track_type == TlTrackType::Group)
        .map(|track| track.id.clone())
        .collect();
    for track in &mut tl.tracks {
        if track
            .parent_group_id
            .as_ref()
            .is_some_and(|group_id| !valid_group_ids.contains(group_id))
        {
            track.parent_group_id = None;
        }
    }

    tl.track_view_layout.clear();
    tl.track_height_resize = None;
    tl.track_height_resize_arm = None;
    for pt in &project.tracks {
        let Some(height) = pt.row_height_px else {
            continue;
        };
        let Some(track) = tl.tracks.iter().find(|t| t.id == pt.id) else {
            continue;
        };
        let clamped = crate::components::timeline::timeline_state::clamp_track_row_height(
            track.track_type,
            height,
        );
        tl.track_view_layout.set_height(pt.id.clone(), clamped);
    }

    // Install the Audio Connections generated while converting v33 routing,
    // then validate them against the current hardware. A device that is not
    // present yields DeviceMissing — the connection and every track reference
    // survive, so reconnecting restores the route.
    migrated_connections.revalidate(&migration_ports);
    tl.audio_connections = migrated_connections;

    let mut warnings: Vec<ProjectLoadWarning> = Vec::new();

    // ── Master / Monitor output routing ─────────────────────────────────────
    // A referenced bus that is missing from this project is dropped rather than
    // pointed somewhere else; an *unavailable* one is kept, because the device
    // may simply be unplugged right now.
    tl.master_output_connection_id = project
        .master_output_connection_id
        .as_deref()
        .map(crate::audio_connections::AudioConnectionId::from_stored)
        .filter(|id| tl.audio_connections.get(id).is_some());
    tl.monitor_output_connection_id = project
        .monitor_output_connection_id
        .as_deref()
        .map(crate::audio_connections::AudioConnectionId::from_stored)
        .filter(|id| tl.audio_connections.get(id).is_some());
    tl.output_routing_initialized = project.output_routing_initialized;

    // Compatibility bootstrap: a project that has never initialized output
    // routing (every pre-v35 file) gets a Master output once, so upgrading does
    // not make it unexpectedly silent. The latch below is what stops a deleted
    // output from reappearing on the next launch.
    if !tl.output_routing_initialized {
        let bootstrap = crate::output_routing::bootstrap_master_output(
            &mut tl.audio_connections,
            &migration_ports,
            false,
        );
        if let Some(id) = bootstrap.assigned() {
            tl.master_output_connection_id = Some(id.clone());
        }
        if let Some(message) = bootstrap.warning() {
            warnings.push(ProjectLoadWarning {
                kind: ProjectLoadWarningKind::NoMasterOutput,
                track_id: None,
                track_name: None,
                source_project_version: crate::project::format::PROJECT_VERSION,
                message,
            });
        }
        tl.output_routing_initialized = true;
    }
    // Monitor keeps its default of Follow Master Output; the bootstrap never
    // assigns an override.
    tl.refresh_output_labels();
    for warning in &migration_warnings {
        let crate::project::routing_migration::RoutingMigrationWarning::ConflictingMidiInput {
            track_id,
            track_name,
            source_version,
            ..
        } = warning;
        warnings.push(ProjectLoadWarning {
            kind: ProjectLoadWarningKind::ConflictingLegacyMidiInput,
            track_id: Some(track_id.clone()),
            track_name: Some(track_name.clone()),
            source_project_version: *source_version,
            message: format!(
                "Track '{track_name}' contained conflicting legacy MIDI input assignments.                  The dedicated MIDI input assignment was retained."
            ),
        });
    }
    for (track_id, track_name) in unresolved_connections {
        warnings.push(ProjectLoadWarning {
            kind: ProjectLoadWarningKind::MissingAudioConnection,
            track_id: Some(track_id),
            track_name: Some(track_name.clone()),
            source_project_version: crate::project::format::PROJECT_VERSION,
            message: format!(
                "Track '{track_name}' references an audio input connection that is not in                  this project. Its input was left unassigned."
            ),
        });
    }
    warnings
}

// ── Audio Connections persistence ──────────────────────────────────────────

/// One problem found while loading a project. Loading always continues; these
/// are surfaced by the studio layer rather than printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLoadWarning {
    pub kind: ProjectLoadWarningKind,
    pub track_id: Option<String>,
    pub track_name: Option<String>,
    pub source_project_version: u32,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectLoadWarningKind {
    /// A v33 track carried a MIDI device in the combined field while its
    /// dedicated MIDI input already held a different explicit assignment.
    ConflictingLegacyMidiInput,
    /// A track referenced an Audio Connection id absent from the registry.
    MissingAudioConnection,
    /// The one-time output bootstrap could not give the project a Master
    /// output. Playback stays silent rather than falling back to a device.
    NoMasterOutput,
}

/// Convert a v33 combined input value into the split runtime fields.
///
/// Audio routing becomes a project-local `AudioConnection` in `registry`,
/// reusing one connection per distinct `(device_id, ordered channel list)`.
/// MIDI routing follows the Cases A/B/C rules in
/// [`crate::project::routing_migration`], compared against the default for
/// this exact track type.
///
/// v33-load only: a v34 project stores both fields directly and never calls
/// this.
#[allow(clippy::too_many_arguments)]
pub(crate) fn legacy_routing_to_runtime(
    legacy: &V33TrackInputRouting,
    existing_midi_input: crate::components::timeline::timeline_state::TrackMidiInputRouting,
    track_type: TlTrackType,
    track_id: &str,
    track_name: &str,
    registry: &mut crate::audio_connections::AudioConnectionRegistry,
    ports: &crate::audio_connections::AvailablePorts,
    warnings: &mut Vec<crate::project::routing_migration::RoutingMigrationWarning>,
) -> (
    Option<crate::audio_connections::AudioConnectionId>,
    crate::components::timeline::timeline_state::TrackMidiInputRouting,
) {
    use crate::project::routing_migration::{
        migrate_track_routing, LegacyTrackInputRouting, LegacyTrackRouting,
    };

    let legacy_input = match legacy {
        V33TrackInputRouting::None => LegacyTrackInputRouting::None,
        V33TrackInputRouting::AllInputs => LegacyTrackInputRouting::AllInputs,
        V33TrackInputRouting::AudioDeviceChannel { device_id, channel } => {
            LegacyTrackInputRouting::AudioDeviceChannel {
                device_id: device_id.clone(),
                channel: *channel,
            }
        }
        V33TrackInputRouting::AudioDeviceChannels {
            device_id,
            channels,
        } => LegacyTrackInputRouting::AudioDeviceChannels {
            device_id: device_id.clone(),
            channels: channels.clone(),
        },
        V33TrackInputRouting::MidiDevice { device_id } => LegacyTrackInputRouting::MidiDevice {
            device_id: device_id.clone(),
        },
    };

    // Case A must compare against the default for *this* track type.
    let midi_input_default =
        crate::components::timeline::timeline_state::TrackRoutingState::for_track_type(track_type)
            .midi_input;

    let mut result = migrate_track_routing(
        &[LegacyTrackRouting {
            track_id: track_id.to_string(),
            track_name: track_name.to_string(),
            legacy_input,
            midi_input: existing_midi_input.clone(),
            midi_input_default,
        }],
        ports,
        33,
    );
    warnings.append(&mut result.warnings);

    let migrated = result.tracks.pop();
    let generated = result.generated_connections.pop();
    let connection_id = match (
        migrated
            .as_ref()
            .and_then(|t| t.audio_input_connection_id.clone()),
        generated,
    ) {
        // Route the generated mapping back through the registry so several
        // tracks with an identical legacy source share one connection.
        (Some(_), Some(connection)) => {
            let channels: Vec<u32> = (0..connection.channel_layout.channel_count())
                .filter_map(|logical| {
                    connection
                        .binding(logical)
                        .map(|binding| binding.physical_port_id.port_index)
                })
                .collect();
            let device_id = connection.device_id.clone().unwrap_or_default();
            registry.get_or_create_audio_connection_for_physical_input(
                &crate::audio_connections::PhysicalInputChoice::Ports {
                    device_id,
                    channels,
                },
                ports,
            )
        }
        (Some(id), None) => Some(id),
        _ => None,
    };

    let midi_input = migrated
        .map(|t| t.midi_input)
        .unwrap_or(existing_midi_input);
    (connection_id, midi_input)
}

/// Convert the runtime registry into its persisted form.
pub(crate) fn audio_connections_to_project(
    registry: &crate::audio_connections::AudioConnectionRegistry,
) -> Vec<ProjectAudioConnection> {
    registry
        .all()
        .iter()
        .map(|connection| ProjectAudioConnection {
            id: connection.id.as_str().to_string(),
            name: connection.name.clone(),
            direction: connection.direction.tag().to_string(),
            channel_layout: connection.channel_layout.tag().to_string(),
            channel_count: connection.channel_layout.channel_count() as u32,
            device_id: connection.device_id.clone(),
            // Ordered exactly as held — index order carries Left/Right.
            port_bindings: connection
                .port_bindings
                .iter()
                .map(|binding| ProjectAudioPortBinding {
                    logical_channel: binding.logical_channel as u32,
                    device_id: binding.physical_port_id.device_id.clone(),
                    port_name: binding.physical_port_id.port_name.clone(),
                    port_index: binding.physical_port_id.port_index,
                })
                .collect(),
            enabled: connection.enabled,
        })
        .collect()
}

/// Rebuild the runtime registry from persisted records.
///
/// Status is not restored from the file — it is recomputed by the caller's
/// `revalidate` against the current device inventory.
pub(crate) fn project_to_audio_connections(
    persisted: &[ProjectAudioConnection],
) -> crate::audio_connections::AudioConnectionRegistry {
    use crate::audio_connections::{
        AudioConnection, AudioConnectionDirection, AudioConnectionId, AudioPortBinding,
        AudioPortId, ChannelLayout,
    };

    let mut registry = crate::audio_connections::AudioConnectionRegistry::new();
    let mut restored = Vec::with_capacity(persisted.len());
    for record in persisted {
        let Some(direction) = AudioConnectionDirection::from_tag(&record.direction) else {
            continue;
        };
        let layout =
            ChannelLayout::from_parts(&record.channel_layout, record.channel_count as usize);
        let mut connection = AudioConnection::new(record.name.clone(), direction, layout);
        // Preserve the persisted id verbatim: a reopened project must never
        // regenerate ids, or every track reference would break.
        connection.id = AudioConnectionId::from_stored(record.id.clone());
        connection.device_id = record.device_id.clone();
        connection.enabled = record.enabled;
        connection.port_bindings = record
            .port_bindings
            .iter()
            .map(|binding| AudioPortBinding {
                logical_channel: binding.logical_channel as usize,
                physical_port_id: AudioPortId::new(
                    binding.device_id.clone(),
                    binding.port_name.clone(),
                    binding.port_index,
                ),
            })
            .collect();
        restored.push(connection);
    }
    registry.replace_all(restored);
    registry
}

fn timeline_output_to_project(
    output: &crate::components::timeline::timeline_state::TrackOutputRouting,
) -> ProjectTrackOutputRouting {
    use crate::components::timeline::timeline_state::TrackOutputRouting as T;
    match output {
        T::Main => ProjectTrackOutputRouting::Main,
        T::Bus { bus_id } => ProjectTrackOutputRouting::Bus {
            bus_id: bus_id.clone(),
        },
        T::HardwareOutput { device_id, channel } => ProjectTrackOutputRouting::HardwareOutput {
            device_id: device_id.clone(),
            channel: *channel,
        },
        T::Instrument { track_id } => ProjectTrackOutputRouting::Instrument {
            track_id: track_id.clone(),
        },
        T::None => ProjectTrackOutputRouting::None,
    }
}

fn timeline_audio_format_to_project(
    audio_format: crate::components::timeline::timeline_state::TrackAudioFormat,
) -> ProjectTrackAudioFormat {
    match audio_format {
        crate::components::timeline::timeline_state::TrackAudioFormat::Mono => {
            ProjectTrackAudioFormat::Mono
        }
        crate::components::timeline::timeline_state::TrackAudioFormat::Stereo => {
            ProjectTrackAudioFormat::Stereo
        }
    }
}

fn timeline_midi_input_to_project(
    input: &crate::components::timeline::timeline_state::TrackMidiInputRouting,
) -> ProjectTrackMidiInputRouting {
    use crate::components::timeline::timeline_state::TrackMidiInputRouting as T;
    match input {
        T::None => ProjectTrackMidiInputRouting::None,
        T::AllInputs => ProjectTrackMidiInputRouting::AllInputs,
        T::MidiDevice { device_id } => ProjectTrackMidiInputRouting::MidiDevice {
            device_id: device_id.clone(),
        },
    }
}

fn project_routing_to_timeline(
    routing: &TrackRouting,
    track_type: TlTrackType,
) -> crate::components::timeline::timeline_state::TrackRoutingState {
    use crate::components::timeline::timeline_state::{
        TrackAudioFormat, TrackMidiInputRouting, TrackOutputRouting, TrackRoutingState,
    };
    let mut state = TrackRoutingState::for_track_type(track_type);
    // v33 stores one combined field. The caller runs the migration adapter and
    // assigns `audio_input_connection_id` afterwards, because that needs the
    // project registry which this pure conversion does not own.
    state.output = match &routing.output {
        ProjectTrackOutputRouting::Main => TrackOutputRouting::Main,
        ProjectTrackOutputRouting::Bus { bus_id } => TrackOutputRouting::Bus {
            bus_id: bus_id.clone(),
        },
        ProjectTrackOutputRouting::HardwareOutput { device_id, channel } => {
            TrackOutputRouting::HardwareOutput {
                device_id: device_id.clone(),
                channel: *channel,
            }
        }
        ProjectTrackOutputRouting::Instrument { track_id } => TrackOutputRouting::Instrument {
            track_id: track_id.clone(),
        },
        ProjectTrackOutputRouting::None => TrackOutputRouting::None,
    };
    state.audio_format = match routing.audio_format {
        ProjectTrackAudioFormat::Mono => TrackAudioFormat::Mono,
        ProjectTrackAudioFormat::Stereo => TrackAudioFormat::Stereo,
    };
    state.midi_input = match &routing.midi_input {
        ProjectTrackMidiInputRouting::None => TrackMidiInputRouting::None,
        ProjectTrackMidiInputRouting::AllInputs => TrackMidiInputRouting::AllInputs,
        ProjectTrackMidiInputRouting::MidiDevice { device_id } => {
            TrackMidiInputRouting::MidiDevice {
                device_id: device_id.clone(),
            }
        }
    };
    state.midi_channel = routing.midi_channel.map(|ch| ch.clamp(1, 16));
    state.midi_output_per_note = routing.midi_output_per_note;
    state.mpe = routing.mpe.sanitized();
    state
}

/// Flatten an [`AutomationTarget`] into its persisted descriptor.
fn target_to_desc(
    target: &crate::components::timeline::timeline_state::AutomationTarget,
) -> AutomationTargetDesc {
    use crate::components::timeline::timeline_state::AutomationTarget as T;
    let mut desc = AutomationTargetDesc {
        tag: target.to_tag(),
        ..Default::default()
    };
    match target {
        T::PluginParameter {
            insert_id,
            parameter_id,
            parameter_name,
        } => {
            desc.insert_id = insert_id.clone();
            desc.parameter_id = parameter_id.clone();
            desc.parameter_name = parameter_name.clone();
        }
        T::SendLevel { send_id } => desc.send_id = send_id.clone(),
        _ => {}
    }
    desc
}

/// Rebuild an [`AutomationTarget`] from a persisted descriptor. Falls back to
/// deriving from `parameter_name` when the descriptor is from an older file
/// (tag 0 with no plugin/send descriptor strings).
fn desc_to_target(
    desc: &AutomationTargetDesc,
    parameter_name: &str,
) -> crate::components::timeline::timeline_state::AutomationTarget {
    use crate::components::timeline::timeline_state::AutomationTarget as T;
    match desc.tag {
        1 => T::TrackPan,
        2 => T::TrackMute,
        3 => T::PluginParameter {
            insert_id: desc.insert_id.clone(),
            parameter_id: desc.parameter_id.clone(),
            parameter_name: if desc.parameter_name.is_empty() {
                parameter_name.to_string()
            } else {
                desc.parameter_name.clone()
            },
        },
        4 => T::SendLevel {
            send_id: desc.send_id.clone(),
        },
        // tag 0: TrackVolume, or a legacy file — derive from the lane name.
        _ => {
            if desc.insert_id.is_empty() && desc.send_id.is_empty() {
                T::from_legacy_name(parameter_name)
            } else {
                T::TrackVolume
            }
        }
    }
}

#[cfg(test)]
mod v33_routing_adapter_tests {
    use super::*;
    use crate::audio_connections::{
        AudioConnectionRegistry, AudioConnectionStatus, AvailablePorts, ChannelLayout,
    };
    use crate::components::timeline::timeline_state::TrackMidiInputRouting;

    fn ports() -> AvailablePorts {
        AvailablePorts::for_device("input-device", "Interface", 4, 2)
    }

    #[allow(clippy::type_complexity)]
    fn load(
        legacy: V33TrackInputRouting,
        midi: TrackMidiInputRouting,
        track_type: TlTrackType,
        registry: &mut AudioConnectionRegistry,
    ) -> (
        Option<crate::audio_connections::AudioConnectionId>,
        TrackMidiInputRouting,
        Vec<crate::project::routing_migration::RoutingMigrationWarning>,
    ) {
        let mut warnings = Vec::new();
        let (id, midi_input) = legacy_routing_to_runtime(
            &legacy,
            midi,
            track_type,
            "track-1",
            "Track 1",
            registry,
            &ports(),
            &mut warnings,
        );
        (id, midi_input, warnings)
    }

    // ── v33 load ────────────────────────────────────────────────────────────

    #[test]
    fn v33_mono_audio_route_becomes_a_mono_connection() {
        let mut registry = AudioConnectionRegistry::new();
        let (id, _, warnings) = load(
            V33TrackInputRouting::AudioDeviceChannel {
                device_id: "input-device".to_string(),
                channel: 2,
            },
            TrackMidiInputRouting::None,
            TlTrackType::Audio,
            &mut registry,
        );
        let id = id.expect("audio route migrates");
        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.channel_layout, ChannelLayout::Mono);
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            2
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn v33_stereo_route_preserves_left_right_ordering() {
        let mut registry = AudioConnectionRegistry::new();
        let (id, _, _) = load(
            V33TrackInputRouting::AudioDeviceChannels {
                device_id: "input-device".to_string(),
                channels: vec![2, 3],
            },
            TrackMidiInputRouting::None,
            TlTrackType::Audio,
            &mut registry,
        );
        let connection = registry.get(&id.unwrap()).unwrap();
        assert_eq!(connection.channel_layout, ChannelLayout::Stereo);
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            2
        );
        assert_eq!(
            connection.binding(1).unwrap().physical_port_id.port_index,
            3
        );
    }

    #[test]
    fn two_tracks_with_the_same_legacy_source_share_one_connection() {
        let mut registry = AudioConnectionRegistry::new();
        let source = V33TrackInputRouting::AudioDeviceChannel {
            device_id: "input-device".to_string(),
            channel: 1,
        };
        let (a, _, _) = load(
            source.clone(),
            TrackMidiInputRouting::None,
            TlTrackType::Audio,
            &mut registry,
        );
        let (b, _, _) = load(
            source,
            TrackMidiInputRouting::None,
            TlTrackType::Audio,
            &mut registry,
        );
        assert_eq!(a, b);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn a_missing_legacy_device_survives_as_device_missing() {
        let mut registry = AudioConnectionRegistry::new();
        let (id, _, _) = load(
            V33TrackInputRouting::AudioDeviceChannel {
                device_id: "unplugged".to_string(),
                channel: 0,
            },
            TrackMidiInputRouting::None,
            TlTrackType::Audio,
            &mut registry,
        );
        let id = id.expect("assignment preserved");
        registry.revalidate(&ports());
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::DeviceMissing
        );
    }

    #[test]
    fn v33_none_leaves_the_track_unassigned_and_midi_untouched() {
        let mut registry = AudioConnectionRegistry::new();
        let dedicated = TrackMidiInputRouting::MidiDevice {
            device_id: "Keystation".to_string(),
        };
        let (id, midi, warnings) = load(
            V33TrackInputRouting::None,
            dedicated.clone(),
            TlTrackType::Audio,
            &mut registry,
        );
        assert!(id.is_none());
        assert_eq!(midi, dedicated);
        assert!(warnings.is_empty());
    }

    /// Case C: two explicit MIDI assignments — the dedicated field wins and a
    /// structured warning is emitted.
    #[test]
    fn conflicting_midi_assignments_retain_the_dedicated_field_and_warn() {
        let mut registry = AudioConnectionRegistry::new();
        let (_, midi, warnings) = load(
            V33TrackInputRouting::MidiDevice {
                device_id: "MPK Mini".to_string(),
            },
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string(),
            },
            TlTrackType::Audio,
            &mut registry,
        );
        assert_eq!(
            midi,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string()
            }
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message().contains("dedicated MIDI input"));
    }

    /// Case A against the per-track-type default: an untouched MIDI track sits
    /// at AllInputs, so the legacy device is taken with no warning.
    #[test]
    fn an_untouched_midi_track_takes_the_legacy_device() {
        let mut registry = AudioConnectionRegistry::new();
        let (_, midi, warnings) = load(
            V33TrackInputRouting::MidiDevice {
                device_id: "MPK Mini".to_string(),
            },
            TrackMidiInputRouting::AllInputs,
            TlTrackType::Midi,
            &mut registry,
        );
        assert_eq!(
            midi,
            TrackMidiInputRouting::MidiDevice {
                device_id: "MPK Mini".to_string()
            }
        );
        assert!(warnings.is_empty());
    }

    // ── v34 save ────────────────────────────────────────────────────────────

    /// The registry must survive save → reopen with every id byte-identical,
    /// otherwise track references break.
    #[test]
    fn v34_registry_round_trips_with_identical_ids() {
        use crate::audio_connections::{AudioConnection, AudioConnectionDirection, ChannelLayout};

        let mut registry = AudioConnectionRegistry::new();
        let mic = registry.add(
            AudioConnection::new(
                "Microphone",
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
            )
            .bind_consecutive("input-device", 0, |i| format!("Input {}", i + 1)),
        );
        let main = registry.add(
            AudioConnection::new(
                "Main Speakers",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("input-device", 0, |i| format!("Output {}", i + 1)),
        );

        let persisted = audio_connections_to_project(&registry);
        let bytes = crate::project::format::encode_project(&FutureboardProject {
            audio_connections: persisted,
            ..FutureboardProject::new("v34")
        });
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");
        let restored = project_to_audio_connections(&reopened.audio_connections);

        assert!(restored.get(&mic).is_some(), "ids must not be regenerated");
        assert!(restored.get(&main).is_some());
        assert_eq!(restored.name_of(&mic), Some("Microphone"));
        assert_eq!(
            restored.get(&main).unwrap().direction,
            AudioConnectionDirection::Output,
            "direction round-trips"
        );
        assert_eq!(
            restored.get(&mic).unwrap().channel_layout,
            ChannelLayout::Mono
        );
    }

    /// Ordered bindings are semantic: [0, 1] and [1, 0] must stay distinct
    /// across a save/reopen or Left and Right would swap.
    #[test]
    fn v34_preserves_ordered_stereo_bindings_and_does_not_normalize_them() {
        use crate::audio_connections::{AvailablePorts, PhysicalInputChoice};

        let ports = AvailablePorts::for_device("input-device", "Interface", 4, 2);
        let mut registry = AudioConnectionRegistry::new();
        let forward = registry
            .get_or_create_audio_connection_for_physical_input(
                &PhysicalInputChoice::Ports {
                    device_id: "input-device".to_string(),
                    channels: vec![0, 1],
                },
                &ports,
            )
            .unwrap();
        let reversed = registry
            .get_or_create_audio_connection_for_physical_input(
                &PhysicalInputChoice::Ports {
                    device_id: "input-device".to_string(),
                    channels: vec![1, 0],
                },
                &ports,
            )
            .unwrap();
        assert_ne!(forward, reversed, "reversed pairs are different buses");

        let bytes = crate::project::format::encode_project(&FutureboardProject {
            audio_connections: audio_connections_to_project(&registry),
            ..FutureboardProject::new("v34")
        });
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");
        let restored = project_to_audio_connections(&reopened.audio_connections);

        let f = restored.get(&forward).unwrap();
        assert_eq!(f.binding(0).unwrap().physical_port_id.port_index, 0);
        assert_eq!(f.binding(1).unwrap().physical_port_id.port_index, 1);
        let r = restored.get(&reversed).unwrap();
        assert_eq!(r.binding(0).unwrap().physical_port_id.port_index, 1);
        assert_eq!(r.binding(1).unwrap().physical_port_id.port_index, 0);
    }

    /// A device that is absent stays exactly as configured — no remapping.
    #[test]
    fn v34_keeps_a_missing_device_configured_rather_than_remapping_it() {
        use crate::audio_connections::{
            AudioConnection, AudioConnectionDirection, AudioConnectionStatus, AvailablePorts,
            ChannelLayout,
        };

        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(
            AudioConnection::new("Gone", AudioConnectionDirection::Input, ChannelLayout::Mono)
                .bind_consecutive("unplugged", 3, |i| format!("Input {}", i + 1)),
        );

        let bytes = crate::project::format::encode_project(&FutureboardProject {
            audio_connections: audio_connections_to_project(&registry),
            ..FutureboardProject::new("v34")
        });
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");
        let mut restored = project_to_audio_connections(&reopened.audio_connections);
        restored.revalidate(&AvailablePorts::for_device("other", "Other", 2, 2));

        let connection = restored.get(&id).expect("connection preserved");
        assert_eq!(connection.status, AudioConnectionStatus::DeviceMissing);
        assert_eq!(connection.device_id.as_deref(), Some("unplugged"));
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            3
        );
    }

    #[test]
    fn the_encoder_writes_the_current_format_version() {
        let bytes = crate::project::format::encode_project(&FutureboardProject::new("v46"));
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(version, 46);
        assert_eq!(crate::project::format::PROJECT_VERSION, 46);
    }

    // ── v35 Master / Monitor output routing ─────────────────────────────────

    /// Build a timeline whose registry holds `Main Output 1-2` plus a second
    /// `Headphones` output on 3-4, with output routing already initialized so
    /// the compatibility bootstrap stays out of the way.
    fn timeline_with_two_outputs() -> (
        crate::components::timeline::timeline_state::TimelineState,
        crate::audio_connections::AudioConnectionId,
        crate::audio_connections::AudioConnectionId,
    ) {
        use crate::audio_connections::{
            AudioConnection, AudioConnectionDirection, AudioConnectionRegistry, AvailablePorts,
            ChannelLayout,
        };
        use crate::components::timeline::timeline_state::TimelineState;

        let ports = AvailablePorts::for_device("dev-1", "Interface", 2, 4);
        let mut registry = AudioConnectionRegistry::default_template(&ports, "dev-1");
        let headphones = registry.add(
            AudioConnection::new(
                "Headphones",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 2, |i| format!("Output {}", i + 1)),
        );
        registry.revalidate(&ports);
        let main = registry
            .by_direction(AudioConnectionDirection::Output)
            .into_iter()
            .find(|c| c.name == "Main Output 1-2")
            .expect("main output")
            .id
            .clone();

        let mut tl = TimelineState::default();
        tl.audio_connections = registry;
        tl.output_routing_initialized = true;
        (tl, main, headphones)
    }

    #[test]
    fn v35_round_trips_master_and_monitor_output_assignments() {
        let (mut tl, main, headphones) = timeline_with_two_outputs();
        tl.set_master_output_connection(Some(main.clone()));
        tl.set_monitor_output_connection(Some(headphones.clone()));

        let bytes = crate::project::format::encode_project(&FutureboardProject::from(&tl));
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");
        assert_eq!(
            reopened.master_output_connection_id.as_deref(),
            Some(main.as_str())
        );
        assert_eq!(
            reopened.monitor_output_connection_id.as_deref(),
            Some(headphones.as_str())
        );
        assert!(reopened.output_routing_initialized);

        let mut loaded = crate::components::timeline::timeline_state::TimelineState::default();
        let _ = apply_to_timeline(&reopened, &mut loaded);
        assert_eq!(loaded.master_output_connection_id.as_ref(), Some(&main));
        assert_eq!(
            loaded.monitor_output_connection_id.as_ref(),
            Some(&headphones)
        );
    }

    /// Follow Master Output is a stored state in its own right: `None` must
    /// come back as `None`, not as a copy of the Master id.
    #[test]
    fn v35_round_trips_a_monitor_that_follows_the_master_output() {
        let (mut tl, main, _headphones) = timeline_with_two_outputs();
        tl.set_master_output_connection(Some(main.clone()));
        assert!(tl.monitor_output_connection_id.is_none());

        let bytes = crate::project::format::encode_project(&FutureboardProject::from(&tl));
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");
        let mut loaded = crate::components::timeline::timeline_state::TimelineState::default();
        let _ = apply_to_timeline(&reopened, &mut loaded);

        assert!(
            loaded.monitor_output_connection_id.is_none(),
            "Follow Master Output must not be flattened into an explicit override"
        );
        assert_eq!(loaded.effective_monitor_output_connection(), Some(main));
    }

    /// Only ids are persisted. The resolved route, runtime device index,
    /// hardware owner, and channel list all describe the current machine.
    #[test]
    fn v35_persists_only_connection_ids_for_output_routing() {
        let (mut tl, main, headphones) = timeline_with_two_outputs();
        tl.set_master_output_connection(Some(main.clone()));
        tl.set_monitor_output_connection(Some(headphones.clone()));

        let project = FutureboardProject::from(&tl);
        assert_eq!(
            project.master_output_connection_id.as_deref(),
            Some(main.as_str())
        );
        assert_eq!(
            project.monitor_output_connection_id.as_deref(),
            Some(headphones.as_str())
        );
        // The persisted forms are exactly the ids — no device, port, or channel
        // is smuggled into them.
        assert!(project
            .master_output_connection_id
            .as_deref()
            .is_some_and(|id| id.starts_with("ac-")));
    }

    /// A pre-v35 project has never initialized output routing, so the
    /// compatibility bootstrap runs — once. Which id it picks depends on the
    /// machine's real hardware (that choice is covered exhaustively in
    /// [`crate::output_routing`]); what this test pins down is that the latch
    /// is set on load and that a second load does not re-run it.
    #[test]
    fn a_pre_v35_project_bootstraps_output_routing_exactly_once() {
        let (tl, main, _headphones) = timeline_with_two_outputs();
        let mut legacy = FutureboardProject::from(&tl);
        legacy
            .audio_connections
            .retain(|connection| connection.id == main.as_str());
        legacy.master_output_connection_id = None;
        legacy.monitor_output_connection_id = None;
        legacy.output_routing_initialized = false;

        let mut loaded = crate::components::timeline::timeline_state::TimelineState::default();
        let _ = apply_to_timeline(&legacy, &mut loaded);
        assert!(loaded.output_routing_initialized, "the latch is set");
        assert!(
            loaded.monitor_output_connection_id.is_none(),
            "Monitor is left following Master; the bootstrap never sets an override"
        );

        // Saving and reopening must not run it again — a user who then deletes
        // the Master output keeps it deleted.
        let mut reopened = FutureboardProject::from(&loaded);
        assert!(reopened.output_routing_initialized);
        let connections_before = reopened.audio_connections.len();
        reopened.master_output_connection_id = None;
        let mut second = crate::components::timeline::timeline_state::TimelineState::default();
        let _ = apply_to_timeline(&reopened, &mut second);
        assert!(
            second.master_output_connection_id.is_none(),
            "a deliberately cleared Master output must not be recreated"
        );
        assert_eq!(
            second.audio_connections.len(),
            connections_before,
            "no second bootstrap bus is created"
        );
    }

    /// The bootstrap never invents a hardware destination when there is none.
    #[test]
    fn a_project_with_no_valid_output_stays_unassigned_and_warns() {
        let mut legacy = FutureboardProject::new("legacy");
        legacy.output_routing_initialized = false;

        let mut loaded = crate::components::timeline::timeline_state::TimelineState::default();
        let warnings = apply_to_timeline(&legacy, &mut loaded);

        if crate::audio_connections::current_available_ports()
            .ports_for(
                "",
                crate::audio_connections::AudioConnectionDirection::Output,
            )
            .is_empty()
            && loaded.master_output_connection_id.is_none()
        {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.kind == ProjectLoadWarningKind::NoMasterOutput),
                "no usable output must produce a structured warning, not a fallback"
            );
        }
        assert!(loaded.output_routing_initialized);
    }

    /// A reference to a bus that is not in the project is dropped rather than
    /// pointed at some other output.
    #[test]
    fn a_master_output_referencing_a_missing_bus_loads_unassigned() {
        let (tl, _main, _headphones) = timeline_with_two_outputs();
        let mut project = FutureboardProject::from(&tl);
        project.master_output_connection_id = Some("ac-ghost".to_string());
        project.monitor_output_connection_id = Some("ac-ghost".to_string());

        let mut loaded = crate::components::timeline::timeline_state::TimelineState::default();
        let _ = apply_to_timeline(&project, &mut loaded);
        assert!(loaded.master_output_connection_id.is_none());
        assert!(loaded.monitor_output_connection_id.is_none());
    }

    /// Renaming the assigned bus changes the chip labels and nothing else.
    #[test]
    fn renaming_the_assigned_output_updates_labels_without_touching_the_routing() {
        let (mut tl, main, _headphones) = timeline_with_two_outputs();
        tl.set_master_output_connection(Some(main.clone()));
        tl.refresh_output_labels();
        assert_eq!(tl.master.output_label, "Main Output 1-2");

        tl.audio_connections.update_name(&main, "Studio Monitors");
        tl.refresh_output_labels();

        assert_eq!(tl.master.output_label, "Studio Monitors");
        assert_eq!(
            tl.master_output_connection_id.as_ref(),
            Some(&main),
            "a rename never changes the stored id"
        );
    }

    /// Audio and MIDI assignments are independent in v34 — the hybrid state
    /// that v33 could not represent now round-trips with nothing dropped.
    #[test]
    fn v34_round_trips_coexisting_audio_and_midi_assignments() {
        use crate::audio_connections::{AvailablePorts, PhysicalInputChoice};
        use crate::components::timeline::timeline_state::{TimelineState, TrackMidiInputRouting};

        let ports = AvailablePorts::for_device("input-device", "Interface", 4, 2);
        let mut tl = TimelineState::default();
        let track_id = tl.create_audio_track();
        let connection = tl
            .audio_connections
            .get_or_create_audio_connection_for_physical_input(
                &PhysicalInputChoice::Ports {
                    device_id: "input-device".to_string(),
                    channels: vec![2, 3],
                },
                &ports,
            )
            .unwrap();
        tl.set_track_audio_input_connection(&track_id, Some(connection.clone()));
        tl.set_track_midi_input(
            &track_id,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string(),
            },
        );

        let project = FutureboardProject::from(&tl);
        let bytes = crate::project::format::encode_project(&project);
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");

        let mut loaded = TimelineState::default();
        let warnings = apply_to_timeline(&reopened, &mut loaded);
        assert!(
            warnings.is_empty(),
            "the hybrid state must round-trip with no warning: {warnings:?}"
        );

        let track = loaded.find_track(&track_id).expect("track");
        assert_eq!(
            track.routing.audio_input_connection_id.as_ref(),
            Some(&connection),
            "the same id comes back — never regenerated"
        );
        assert_eq!(
            track.routing.midi_input,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string()
            }
        );
        let restored = loaded.audio_connections.get(&connection).expect("bus");
        assert_eq!(restored.binding(0).unwrap().physical_port_id.port_index, 2);
        assert_eq!(restored.binding(1).unwrap().physical_port_id.port_index, 3);
    }

    /// An id with no matching connection is reported and left unassigned — it
    /// must never silently bind to some other bus.
    #[test]
    fn an_unknown_connection_id_is_reported_and_left_unassigned() {
        use crate::components::timeline::timeline_state::TimelineState;

        let mut tl = TimelineState::default();
        let track_id = tl.create_audio_track();
        let mut project = FutureboardProject::from(&tl);
        // Point the track at an id the registry does not contain.
        project.tracks[0].routing.audio_input_connection_id = Some("ac-ghost".to_string());
        project.audio_connections.clear();

        let mut loaded = TimelineState::default();
        let warnings = apply_to_timeline(&project, &mut loaded);

        let track = loaded.find_track(&track_id).expect("track");
        assert!(
            track.routing.audio_input_connection_id.is_none(),
            "an unresolved reference must not bind to an unrelated connection"
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].kind,
            ProjectLoadWarningKind::MissingAudioConnection
        );
        assert!(warnings[0].message.contains("left unassigned"));
    }

    // ── v33 → v34 migration through the real loader ─────────────────────────

    /// A v33 project migrates on load, and saving it writes v34 with the
    /// generated ids intact. Reopening that v34 file must not migrate again.
    #[test]
    fn a_migrated_project_saves_as_v34_and_does_not_migrate_again() {
        use crate::components::timeline::timeline_state::TimelineState;

        let mut v33 = FutureboardProject::new("legacy");
        let mut tl_seed = TimelineState::default();
        let track_id = tl_seed.create_audio_track();
        v33 = FutureboardProject::from(&tl_seed);
        // Force the v33 shape: a legacy combined route, no registry.
        v33.tracks[0].routing.audio_input_connection_id = None;
        v33.tracks[0].routing.legacy_input = Some(V33TrackInputRouting::AudioDeviceChannels {
            device_id: "input-device".to_string(),
            channels: vec![2, 3],
        });
        v33.audio_connections.clear();

        let mut migrated = TimelineState::default();
        let warnings = apply_to_timeline(&v33, &mut migrated);
        assert!(warnings.is_empty());

        let assigned = migrated
            .find_track(&track_id)
            .and_then(|t| t.routing.audio_input_connection_id.clone())
            .expect("legacy route migrated");
        let connection = migrated.audio_connections.get(&assigned).unwrap();
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            2
        );
        assert_eq!(
            connection.binding(1).unwrap().physical_port_id.port_index,
            3
        );

        // Save at the current version and reopen.
        let saved = crate::project::format::encode_project(&FutureboardProject::from(&migrated));
        let version = u32::from_le_bytes(saved[8..12].try_into().unwrap());
        assert_eq!(
            version,
            crate::project::format::PROJECT_VERSION,
            "a migrated project saves at the current version"
        );

        let reopened = crate::project::format::decode_project(&saved).expect("decode");
        assert!(
            reopened
                .tracks
                .iter()
                .all(|t| t.routing.legacy_input.is_none()),
            "a v34 file carries no legacy union, so migration cannot run again"
        );
        let mut second = TimelineState::default();
        let second_warnings = apply_to_timeline(&reopened, &mut second);
        assert!(second_warnings.is_empty());
        assert_eq!(
            second
                .find_track(&track_id)
                .and_then(|t| t.routing.audio_input_connection_id.clone()),
            Some(assigned),
            "the id is stable across the v34 reopen"
        );
    }

    /// Case C surfaces as a structured warning, not stderr.
    #[test]
    fn a_conflicting_legacy_midi_assignment_produces_a_structured_warning() {
        use crate::components::timeline::timeline_state::{TimelineState, TrackMidiInputRouting};

        let mut tl_seed = TimelineState::default();
        let track_id = tl_seed.create_audio_track();
        tl_seed.set_track_midi_input(
            &track_id,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string(),
            },
        );
        let mut v33 = FutureboardProject::from(&tl_seed);
        v33.tracks[0].routing.legacy_input = Some(V33TrackInputRouting::MidiDevice {
            device_id: "MPK Mini".to_string(),
        });
        v33.tracks[0].routing.audio_input_connection_id = None;

        let mut loaded = TimelineState::default();
        let warnings = apply_to_timeline(&v33, &mut loaded);

        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert_eq!(
            warning.kind,
            ProjectLoadWarningKind::ConflictingLegacyMidiInput
        );
        assert_eq!(warning.track_id.as_deref(), Some(track_id.as_str()));
        assert_eq!(warning.source_project_version, 33);
        assert!(warning
            .message
            .contains("dedicated MIDI input assignment was retained"));

        // The dedicated assignment wins.
        assert_eq!(
            loaded.find_track(&track_id).unwrap().routing.midi_input,
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".to_string()
            }
        );
    }

    /// Full panel-path round-trip: build a connection exactly the way the panel
    /// does (mutation API only), save as v34, reopen, and assert the id and the
    /// ordered mappings survive.
    #[test]
    fn a_panel_created_and_edited_connection_survives_v34_save_and_reopen() {
        use crate::audio_connections::{
            AudioConnectionDirection, AudioConnectionStatus, AudioPortId, AvailablePorts,
            ChannelLayout,
        };
        use crate::components::timeline::timeline_state::TimelineState;

        let ports = AvailablePorts::for_device("dev-1", "Interface", 4, 4);
        let mut tl = TimelineState::default();

        // 1. Create through the panel's Add path.
        let (id, add) = tl.audio_connections.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
            &ports,
        );
        assert!(add.needs_routing_rebuild);

        // 2. Rename.
        let rename = tl.audio_connections.update_name(&id, "  Vocal Mic  ");
        assert!(!rename.needs_routing_rebuild, "a rename is not routing");
        assert_eq!(tl.audio_connections.name_of(&id), Some("Vocal Mic"));

        // 3. Mono -> Stereo.
        tl.audio_connections
            .update_layout(&id, ChannelLayout::Stereo, &ports);

        // 4. Device + ordered Left/Right. Deliberately reversed so the test
        //    would fail if anything normalized the pair.
        tl.audio_connections
            .update_device(&id, Some("dev-1"), &ports);
        tl.audio_connections.update_port_binding(
            &id,
            0,
            Some(AudioPortId::new("dev-1", "Input 3", 2)),
            &ports,
        );
        tl.audio_connections.update_port_binding(
            &id,
            1,
            Some(AudioPortId::new("dev-1", "Input 2", 1)),
            &ports,
        );
        assert_eq!(
            tl.audio_connections.resolved_ports(&id, &ports),
            Some(vec![2, 1])
        );

        // 5. Disable, so the enabled flag is exercised too.
        tl.audio_connections.update_enabled(&id, false, &ports);

        // 6/7. Save and reopen.
        let project = FutureboardProject::from(&tl);
        let bytes = crate::project::format::encode_project(&project);
        assert_eq!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            crate::project::format::PROJECT_VERSION,
            "panel edits save at the current version"
        );
        let reopened = crate::project::format::decode_project(&bytes).expect("decode");
        let mut loaded = TimelineState::default();
        let warnings = apply_to_timeline(&reopened, &mut loaded);
        assert!(warnings.is_empty(), "clean reopen: {warnings:?}");

        // 8. Same id, same ordered mappings, same enabled state.
        let restored = loaded
            .audio_connections
            .get(&id)
            .expect("the panel-created id survives");
        assert_eq!(restored.name, "Vocal Mic");
        assert_eq!(restored.channel_layout, ChannelLayout::Stereo);
        assert_eq!(restored.device_id.as_deref(), Some("dev-1"));
        assert_eq!(restored.binding(0).unwrap().physical_port_id.port_index, 2);
        assert_eq!(restored.binding(1).unwrap().physical_port_id.port_index, 1);
        assert!(!restored.enabled);

        // Re-enabling after the reopen resolves to the same ordered pair.
        loaded.audio_connections.update_enabled(&id, true, &ports);
        assert_eq!(
            loaded.audio_connections.get(&id).unwrap().status,
            AudioConnectionStatus::Active
        );
        assert_eq!(
            loaded.audio_connections.resolved_ports(&id, &ports),
            Some(vec![2, 1])
        );
    }

    /// A track assigned through the panel path keeps only a connection id —
    /// no raw device or channel data reaches TrackRoutingState.
    #[test]
    fn panel_edits_never_write_raw_device_routing_into_a_track() {
        use crate::audio_connections::{AudioConnectionDirection, AvailablePorts, ChannelLayout};
        use crate::components::timeline::timeline_state::TimelineState;

        let ports = AvailablePorts::for_device("dev-1", "Interface", 4, 4);
        let mut tl = TimelineState::default();
        let track_id = tl.create_audio_track();
        let (id, _) = tl.audio_connections.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
            &ports,
        );
        tl.set_track_audio_input_connection(&track_id, Some(id.clone()));

        let project = FutureboardProject::from(&tl);
        let track = &project.tracks[0];
        assert_eq!(
            track.routing.audio_input_connection_id.as_deref(),
            Some(id.as_str())
        );
        assert!(
            track.routing.legacy_input.is_none(),
            "v34 never reconstructs the legacy combined union"
        );
    }
}

#[cfg(test)]
mod project_settings_persistence_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, TimelineState, TrackType,
    };
    use sphere_midi_service::mpe::{MpeOutputMode, MpeTrackConfiguration};

    #[test]
    fn project_sample_rate_survives_save_decode_and_timeline_restore() {
        let mut timeline = TimelineState::default();
        timeline.project_sample_rate = 96_000;

        let encoded = crate::project::format::encode_project(&FutureboardProject::from(&timeline));
        let decoded = crate::project::format::decode_project(&encoded).expect("decode project");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);

        assert_eq!(decoded.settings.sample_rate, 96_000);
        assert_eq!(restored.project_sample_rate, 96_000);
    }

    #[test]
    fn project_timebase_survives_save_decode_and_timeline_restore() {
        use crate::components::timeline::timeline_state::{TimeDisplayFormat, TimecodeRate};

        let mut timeline = TimelineState::default();
        timeline.time_display_format = TimeDisplayFormat::Timecode;
        timeline.timecode_rate = TimecodeRate::Fps25;

        let encoded = crate::project::format::encode_project(&FutureboardProject::from(&timeline));
        let decoded = crate::project::format::decode_project(&encoded).expect("decode project");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);

        assert_eq!(restored.time_display_format, TimeDisplayFormat::Timecode);
        assert_eq!(restored.timecode_rate, TimecodeRate::Fps25);
    }

    #[test]
    fn mpe_track_configuration_survives_save_decode_and_timeline_restore() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_midi_track();
        let expected = MpeTrackConfiguration {
            mode: MpeOutputMode::Upper,
            member_channels: 4,
            member_pitch_range: 12.0,
            manager_pitch_range: 24.0,
        };
        assert!(timeline.set_track_mpe_configuration(&track_id, expected));

        let encoded = crate::project::format::encode_project(&FutureboardProject::from(&timeline));
        let decoded = crate::project::format::decode_project(&encoded).expect("decode project");
        assert_eq!(decoded.tracks[0].routing.mpe, expected);

        let restored = project_routing_to_timeline(
            &decoded.tracks[0].routing,
            crate::components::timeline::timeline_state::TrackType::Midi,
        );
        assert_eq!(restored.mpe, expected);
    }

    #[test]
    fn a_project_without_a_stored_timebase_opens_in_bars_and_beats() {
        use crate::components::timeline::timeline_state::{TimeDisplayFormat, TimecodeRate};

        // What a pre-v43 file decodes to: the settings carry the defaults
        // because the body simply ended before the timebase block.
        let project = FutureboardProject::new("legacy");
        let mut restored = TimelineState::default();
        restored.time_display_format = TimeDisplayFormat::Samples;
        restored.timecode_rate = TimecodeRate::Fps24;
        let _ = apply_to_timeline(&project, &mut restored);

        assert_eq!(restored.time_display_format, TimeDisplayFormat::BarsBeats);
        assert_eq!(restored.timecode_rate, TimecodeRate::Fps30);
    }

    #[test]
    fn invalid_project_sample_rate_falls_back_without_changing_app_defaults() {
        let mut project = FutureboardProject::new("invalid rate");
        project.settings.sample_rate = 12_345;
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&project, &mut restored);

        assert_eq!(restored.project_sample_rate, 48_000);
    }

    #[test]
    fn solfege_track_settings_survive_save_decode_and_timeline_restore() {
        let mut timeline = TimelineState::default();
        let track_id = timeline.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Solfege Violin".to_string(),
            color: crate::theme::Colors::accent_primary(),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let expected = crate::solfege::SolfegeTrackState {
            model_path: Some("C:\\Models\\violin.fbmx".to_string()),
            instrument: "Violin".to_string(),
            voice: "Solo Bowed String".to_string(),
            preset: "VSCO Solo Violin".to_string(),
            bow_pressure: 0.71,
            vibrato: 0.29,
            dynamics: 0.84,
            expression: 0.93,
            visible_lanes: vec![
                crate::solfege::SolfegeLaneVisibility {
                    lane_id: "velocity".to_string(),
                    height: 64.0,
                },
                crate::solfege::SolfegeLaneVisibility {
                    lane_id: "bow-pressure".to_string(),
                    height: 96.0,
                },
            ],
        };
        assert!(timeline.set_track_solfege_engine(&track_id, Some(expected.clone())));

        let encoded = encode_project(&FutureboardProject::from(&timeline));
        let decoded = decode_project(&encoded).expect("decode project");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);

        let track = restored
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("Solfege track restored");
        assert_eq!(track.solfege, Some(expected));
        assert!(!track.builtin_soundfont_player);
    }

    /// A Solfege performance must come back exactly as it was saved: the note
    /// ids the pitch curves are keyed to, the curves themselves, the per-note
    /// articulation, and the visible-lane layout.
    #[test]
    fn solfege_performance_data_survives_save_and_load() {
        use crate::components::timeline::timeline_state::{
            ArticulationId, MidiControllerKind, PitchCurve, PitchPoint, PitchSegmentShape,
        };

        let mut timeline = TimelineState::default();
        let track_id = timeline.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Solfege Violin".to_string(),
            color: crate::theme::Colors::accent_primary(),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let mut solfege = crate::solfege::SolfegeTrackState::violin(None);
        solfege.visible_lanes = vec![
            crate::solfege::SolfegeLaneVisibility {
                lane_id: "dynamics".to_string(),
                height: 88.0,
            },
            crate::solfege::SolfegeLaneVisibility {
                lane_id: "bow-pressure".to_string(),
                height: 61.0,
            },
        ];
        assert!(timeline.set_track_solfege_engine(&track_id, Some(solfege)));

        let clip = timeline
            .build_midi_clip(&track_id, 0.0, 4.0)
            .expect("clip builds");
        let clip_id = clip.id.clone();
        crate::components::edit::edit_commands::EditCommand::CreateClip {
            track_id: track_id.clone(),
            clip,
        }
        .execute(&mut timeline);

        let note_id = timeline
            .add_midi_note(&clip_id, 62, 1.0, 2.0, 96)
            .expect("note added");
        {
            let notes = timeline.midi_clip_notes_mut(&clip_id).unwrap();
            let note = notes.iter_mut().find(|n| n.id == note_id).unwrap();
            note.articulation = Some(ArticulationId::Legato);
            note.pitch_curve = Some(PitchCurve::from_points(vec![
                PitchPoint::new(0.0, -137.5, PitchSegmentShape::Smooth),
                PitchPoint::new(0.5, 0.0, PitchSegmentShape::Linear),
                PitchPoint::new(1.75, 23.25, PitchSegmentShape::Hold),
            ]));
        }
        // A performance lane on the same clip, to prove lanes and pitch travel
        // together rather than one shadowing the other.
        timeline.put_controller_point(&clip_id, MidiControllerKind::CC(1), 0.5, 0.8);

        let encoded = crate::project::format::encode_project(&FutureboardProject::from(&timeline));
        let decoded = crate::project::format::decode_project(&encoded).expect("decode project");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);

        let track = restored
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("Solfege track restored");
        let lanes = &track.solfege.as_ref().expect("solfege state").visible_lanes;
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0].lane_id, "dynamics");
        assert_eq!(lanes[0].height, 88.0);
        assert_eq!(lanes[1].lane_id, "bow-pressure");

        let note = restored
            .midi_note(&clip_id, note_id)
            .expect("note restored under its original id");
        assert_eq!(note.pitch, 62);
        assert_eq!(note.articulation, Some(ArticulationId::Legato));
        let curve = note.pitch_curve.as_ref().expect("pitch curve restored");
        assert_eq!(curve.points.len(), 3);
        assert_eq!(curve.points[0].cents, -137.5);
        assert_eq!(curve.points[0].shape, PitchSegmentShape::Smooth);
        assert_eq!(curve.points[2].shape, PitchSegmentShape::Hold);
        assert!((curve.cents_at(1.75) - 23.25).abs() < 0.001);

        let points = restored
            .controller_lane_points(&clip_id, MidiControllerKind::CC(1))
            .expect("dynamics lane restored");
        assert_eq!(points.len(), 1);
        assert!((points[0].value - 0.8).abs() < 0.001);
    }

    /// Analyse, save, close, open — and the accents are the ones that were
    /// analysed, not a fresh analysis and not a row of neutral values.
    ///
    /// Provenance survives too, which is the part that matters beyond the
    /// numbers: a hand-edited accent that came back marked "generated" would be
    /// quietly overwritten by the next Analyze Accent.
    #[test]
    fn accent_survives_save_and_load_including_its_provenance() {
        use crate::components::timeline::timeline_state::{AccentSource, AccentState};

        let mut timeline = TimelineState::default();
        let track_id = timeline.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Solfege Violin".to_string(),
            color: crate::theme::Colors::accent_primary(),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        assert!(timeline.set_track_solfege_engine(
            &track_id,
            Some(crate::solfege::SolfegeTrackState::violin(None))
        ));
        let clip = timeline
            .build_midi_clip(&track_id, 0.0, 4.0)
            .expect("clip builds");
        let clip_id = clip.id.clone();
        crate::components::edit::edit_commands::EditCommand::CreateClip {
            track_id: track_id.clone(),
            clip,
        }
        .execute(&mut timeline);

        let generated_id = timeline
            .add_midi_note(&clip_id, 60, 0.0, 1.0, 96)
            .expect("note added");
        let manual_id = timeline
            .add_midi_note(&clip_id, 64, 1.0, 1.0, 96)
            .expect("note added");
        let untouched_id = timeline
            .add_midi_note(&clip_id, 67, 2.0, 1.0, 96)
            .expect("note added");
        {
            let notes = timeline.midi_clip_notes_mut(&clip_id).unwrap();
            notes
                .iter_mut()
                .find(|note| note.id == generated_id)
                .unwrap()
                .accent = Some(AccentState::generated(0.82, 0.71, 0.34, 0.55, 0.63));
            notes
                .iter_mut()
                .find(|note| note.id == manual_id)
                .unwrap()
                .accent = Some(AccentState::neutral().with_prominence(0.25));
        }

        let encoded = crate::project::format::encode_project(&FutureboardProject::from(&timeline));
        let decoded = crate::project::format::decode_project(&encoded).expect("decode project");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);

        let generated = restored
            .midi_note(&clip_id, generated_id)
            .expect("note restored")
            .accent
            .expect("accent restored");
        assert_eq!(generated.prominence, 0.82);
        assert_eq!(generated.attack, 0.71);
        assert_eq!(generated.agogic, 0.34);
        assert_eq!(generated.timbre, 0.55);
        assert_eq!(generated.confidence, 0.63);
        assert_eq!(generated.source, AccentSource::Generated);

        let manual = restored
            .midi_note(&clip_id, manual_id)
            .expect("note restored")
            .accent
            .expect("accent restored");
        assert_eq!(manual.prominence, 0.25);
        assert_eq!(
            manual.source,
            AccentSource::Manual,
            "a hand-edited accent came back as generated and would be overwritten"
        );

        // A note that was never analysed comes back un-analysed, not neutral:
        // the two are different states and the re-analysis policy depends on
        // telling them apart.
        assert!(restored
            .midi_note(&clip_id, untouched_id)
            .expect("note restored")
            .accent
            .is_none());
    }
}

#[cfg(test)]
mod group_track_persistence_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, TimelineState, TrackType,
    };

    fn add_track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: crate::theme::Colors::accent_primary(),
            volume: crate::components::timeline::timeline_state::volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    #[test]
    fn group_membership_survives_binary_roundtrip() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let group_id = add_track(&mut state, TrackType::Group, "Drums");
        let child_id = add_track(&mut state, TrackType::Audio, "Kick");
        assert!(state.assign_track_to_group(&child_id, &group_id));
        assert_eq!(state.toggle_group_collapsed(&group_id), Some(true));

        let bytes = encode_project(&FutureboardProject::from(&state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        assert_eq!(
            restored
                .find_track(&child_id)
                .unwrap()
                .parent_group_id
                .as_deref(),
            Some(group_id.as_str())
        );
        assert_eq!(
            restored.find_track(&group_id).unwrap().track_type,
            TrackType::Group
        );
        assert!(restored.find_track(&group_id).unwrap().group_collapsed);
        assert!(restored.remove_track_from_group(&child_id));
        assert!(restored
            .find_track(&child_id)
            .unwrap()
            .parent_group_id
            .is_none());
    }
}

#[cfg(test)]
mod inspector_property_persistence_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{StretchMode, TimelineState};

    #[test]
    fn pan_and_audio_inspector_properties_survive_project_roundtrip() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_audio_track();
        state.set_track_pan(&track_id, -0.42);
        assert!(state.set_track_volume_automation_read(&track_id, false));
        let clip_id = state.insert_audio_clip_with_duration(
            track_id.clone(),
            "C:/Audio/source.wav".to_string(),
            "Source".to_string(),
            2.5,
            8.0,
            Some(4.0),
        );
        assert!(state.set_clip_gain(&clip_id, 0.63));
        assert!(state.set_clip_muted(&clip_id, true));
        let mut stretch = state.clip_stretch(&clip_id).cloned().expect("stretch");
        stretch.mode = StretchMode::Manual;
        stretch.pitch_shift_semitones = 3.25;
        stretch.transient_sensitivity = 0.7;
        stretch.fade_in_ms = 125.0;
        stretch.fade_out_ms = 250.0;
        stretch.gain_db = -1.5;
        stretch.pan = 0.2;
        assert!(state.set_clip_stretch(&clip_id, stretch.clone()));

        let bytes = encode_project(&FutureboardProject::from(&state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let track = restored.find_track(&track_id).expect("track");
        assert!((track.pan - -0.42).abs() < 1.0e-6);
        assert!(!track.volume_automation_read);
        let (_, clip) = restored.find_clip(&clip_id).expect("clip");
        assert!((clip.start_beat - 2.5).abs() < 1.0e-6);
        assert!((clip.duration_beats - 8.0).abs() < 1.0e-6);
        assert!((clip.gain - 0.63).abs() < 1.0e-6);
        assert!(clip.muted);
        assert_eq!(clip.stretch, stretch);
    }
}

#[cfg(test)]
mod articulation_persistence_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{ArticulationId, TimelineState};

    /// Per-note articulations and the clip's direction articulation lane must
    /// survive save → binary encode/decode → load. Event ids are transient and
    /// re-minted on load; beats and articulation identities are what persist.
    #[test]
    fn midi_articulations_survive_save_and_reload() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_midi_track();
        let clip_id = state.create_midi_clip(&track_id, 0.0, 8.0).expect("clip");
        let plain = state
            .add_midi_note(&clip_id, 60, 0.0, 1.0, 100)
            .expect("note");
        let accented = state
            .add_midi_note(&clip_id, 64, 1.0, 1.0, 100)
            .expect("note");
        state.set_midi_notes_articulation(&clip_id, &[accented], Some(ArticulationId::Accent));
        state.add_midi_articulation(&clip_id, 0.0, ArticulationId::Sustain);
        state.add_midi_articulation(&clip_id, 4.0, ArticulationId::Staccato);
        let _ = plain;

        let project = FutureboardProject::from(&state);
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let notes = restored.midi_clip_notes(&clip_id).expect("notes restored");
        assert_eq!(notes.len(), 2);
        let by_pitch = |p: u8| notes.iter().find(|n| n.pitch == p).expect("pitch");
        assert_eq!(by_pitch(60).articulation, None);
        assert_eq!(by_pitch(64).articulation, Some(ArticulationId::Accent));
        // Restored notes keep raw duration/velocity (playback-only modifiers).
        assert_eq!(by_pitch(64).duration, 1.0);
        assert_eq!(by_pitch(64).velocity, 100);

        let events = restored
            .midi_clip_articulations(&clip_id)
            .expect("articulation lane restored");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].beat, 0.0);
        assert_eq!(events[0].articulation, ArticulationId::Sustain);
        assert_eq!(events[1].beat, 4.0);
        assert_eq!(events[1].articulation, ArticulationId::Staccato);
    }
}

#[cfg(test)]
mod vsti_substrip_persistence_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        vsti_output_child_track_id, CreateTrackOptions, InsertPluginFormat, TimelineState,
        TrackType,
    };

    /// Substrip (VSTi multi-out child strip) mixer state and FX insert chains —
    /// including opaque plugin state bytes — must survive save -> binary
    /// encode/decode -> load. Child strips have deterministic ids, so
    /// `ensure_vsti_output_child_tracks` retains (never duplicates) the loaded
    /// rows once the plugin reports its layout.
    #[test]
    fn substrip_insert_chain_and_mixer_state_roundtrip() {
        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Drums".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "drums".to_string(),
            Some(PathBuf::from("C:/p/drums.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Drums".to_string(),
        );
        state.set_insert_output_bus_layout(&track_id, &slot, &[2, 2]);
        state.auto_enable_detected_insert_outputs(&track_id, &slot, 4);

        let child_id = vsti_output_child_track_id(&slot, 1);
        assert!(
            state.tracks.iter().any(|t| t.id == child_id),
            "multi-out layout should create the bus-1 child strip"
        );

        // FX insert on the substrip, with plugin state bytes and bypass set.
        let fx_slot = state
            .add_insert(&child_id)
            .expect("substrip accepts inserts");
        state.set_insert_plugin(
            &child_id,
            &fx_slot,
            "comp".to_string(),
            Some(PathBuf::from("C:/p/comp.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Comp".to_string(),
        );
        {
            let slots = state.insert_slots_mut(&child_id).expect("child slots");
            let fx = slots.iter_mut().find(|s| s.id == fx_slot).expect("fx slot");
            fx.vst3_state = Some(std::sync::Arc::new(vec![1, 2, 3, 4]));
            fx.bypassed = true;
        }
        // Per-bus mixer state.
        state.toggle_track_mute(&child_id);
        state.set_track_pan(&child_id, -0.25);

        let project = FutureboardProject::from(&state);
        assert!(
            project.tracks.iter().any(|t| t.id == child_id),
            "child strip must be persisted"
        );
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");

        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let child = restored
            .tracks
            .iter()
            .find(|t| t.id == child_id)
            .expect("substrip restored");
        assert!(child.muted, "substrip mute state restored");
        assert!((child.pan + 0.25).abs() < 1e-6, "substrip pan restored");
        let fx = child
            .inserts
            .iter()
            .find(|s| s.id == fx_slot)
            .expect("substrip insert restored");
        assert_eq!(fx.plugin_id.as_deref(), Some("comp"));
        assert!(fx.bypassed, "substrip insert bypass restored");
        assert_eq!(
            fx.vst3_state.as_ref().map(|s| s.as_ref().clone()),
            Some(vec![1, 2, 3, 4]),
            "substrip insert plugin state bytes restored"
        );
    }

    /// A built-in plugin (no VST3 runtime, `InsertPluginFormat::Unknown`)
    /// persists its DSP state through the same `vst3_state` byte channel as
    /// any other insert — the field is opaque-bytes-keyed-by-plugin_id, not
    /// format-gated (see `InsertSlotState::vst3_state`'s doc comment). This
    /// is what `collect_builtin_instances` (`plugin_ops.rs`) reads back to
    /// populate a shared editor's `selectInstance.state`.
    #[test]
    fn builtin_plugin_state_bytes_roundtrip_through_save_and_load() {
        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Guitar".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "rodharerist".to_string(),
            None,
            InsertPluginFormat::Unknown,
            None,
            "Rodhareist".to_string(),
        );
        let json_state = br#"{"schema_version":1,"params":{"amp_gain":7.5}}"#.to_vec();
        {
            let slots = state.insert_slots_mut(&track_id).expect("slots");
            let fx = slots.iter_mut().find(|s| s.id == slot).expect("fx slot");
            fx.vst3_state = Some(std::sync::Arc::new(json_state.clone()));
        }

        let project = FutureboardProject::from(&state);
        let bytes = encode_project(&project);
        let decoded = decode_project(&bytes).expect("decode");

        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let track = restored
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .expect("track restored");
        let fx = track
            .inserts
            .iter()
            .find(|s| s.id == slot)
            .expect("builtin insert restored");
        assert_eq!(fx.plugin_id.as_deref(), Some("rodharerist"));
        assert_eq!(
            fx.vst3_state.as_ref().map(|s| s.as_ref().clone()),
            Some(json_state),
            "built-in plugin's JSON state bytes must survive save/load"
        );
    }

    /// A VST2 insert must come back as VST2, not as VST3 or Unknown: the format
    /// is what selects the native bridge on reload, so a collision in the
    /// on-disk tag would send a `.dll` to the VST3 loader and fail silently.
    #[test]
    fn vst2_insert_round_trips_format_and_opaque_state() {
        use crate::components::timeline::timeline_state::PluginRuntimeBackend;

        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Keys".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "vst2:1400136302".to_string(),
            Some(PathBuf::from("C:/Program Files/VSTPlugins/Legacy.dll")),
            InsertPluginFormat::Vst2,
            None,
            "Legacy".to_string(),
        );
        // `effGetChunk` bank bytes — opaque to the host, so they must survive
        // byte for byte.
        let chunk = vec![0xDEu8, 0xAD, 0xBE, 0xEF];
        {
            let slots = state.insert_slots_mut(&track_id).expect("slots");
            let fx = slots.iter_mut().find(|s| s.id == slot).expect("fx slot");
            fx.vst3_state = Some(std::sync::Arc::new(chunk.clone()));
        }

        let bytes = encode_project(&FutureboardProject::from(&state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let fx = restored
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.inserts.iter().find(|s| s.id == slot))
            .expect("vst2 insert restored");
        assert_eq!(fx.plugin_format, Some(InsertPluginFormat::Vst2));
        assert_eq!(fx.runtime_backend, PluginRuntimeBackend::ExternalBridge);
        assert_eq!(
            fx.vst3_state.as_ref().map(|s| s.as_ref().clone()),
            Some(chunk),
            "the VST2 chunk bytes must survive save/load"
        );
    }

    /// A CLAP insert must come back as CLAP and bridge-hosted, carrying its
    /// `clap.state` blob. Same reasoning as the VST2 case: the format is what
    /// selects the native bridge on reload.
    #[test]
    fn clap_insert_round_trips_format_and_opaque_state() {
        use crate::components::timeline::timeline_state::PluginRuntimeBackend;

        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Pad".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "com.example.synth".to_string(),
            Some(PathBuf::from(
                "C:/Program Files/Common Files/CLAP/Example.clap",
            )),
            InsertPluginFormat::Clap,
            None,
            "Example".to_string(),
        );
        // `clap.state` bytes — opaque to the host, so they must survive byte
        // for byte.
        let blob = vec![0x01u8, 0x02, 0x03, 0x04, 0x05];
        {
            let slots = state.insert_slots_mut(&track_id).expect("slots");
            let fx = slots.iter_mut().find(|s| s.id == slot).expect("fx slot");
            fx.vst3_state = Some(std::sync::Arc::new(blob.clone()));
        }

        let bytes = encode_project(&FutureboardProject::from(&state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let fx = restored
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.inserts.iter().find(|s| s.id == slot))
            .expect("clap insert restored");
        assert_eq!(fx.plugin_format, Some(InsertPluginFormat::Clap));
        assert_eq!(fx.runtime_backend, PluginRuntimeBackend::ExternalBridge);
        assert_eq!(
            fx.vst3_state.as_ref().map(|s| s.as_ref().clone()),
            Some(blob),
            "the CLAP state bytes must survive save/load"
        );
    }

    /// An Audio Unit is addressed by component id, so `plugin_path` holds a
    /// string that will never exist on disk. Project load must not read that as
    /// a broken plugin: the slot has to come back loadable, bridge-hosted, and
    /// still carrying its opaque ClassInfo bytes.
    #[test]
    fn audio_unit_insert_loads_without_a_module_file_on_disk() {
        use crate::components::timeline::timeline_state::{InsertLoadStatus, PluginRuntimeBackend};

        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Vocal".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        let component = "au:61756678:64656c79:6170706c";
        state.set_insert_plugin(
            &track_id,
            &slot,
            component.to_string(),
            Some(PathBuf::from(component)),
            InsertPluginFormat::Au,
            None,
            "AUDelay".to_string(),
        );
        let class_info = vec![7u8, 8, 9];
        {
            let slots = state.insert_slots_mut(&track_id).expect("slots");
            let fx = slots.iter_mut().find(|s| s.id == slot).expect("fx slot");
            fx.vst3_state = Some(std::sync::Arc::new(class_info.clone()));
        }

        let bytes = encode_project(&FutureboardProject::from(&state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        apply_to_timeline(&decoded, &mut restored);

        let fx = restored
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.inserts.iter().find(|s| s.id == slot))
            .expect("audio unit insert restored");
        assert_eq!(fx.plugin_format, Some(InsertPluginFormat::Au));
        assert_eq!(
            fx.load_status,
            InsertLoadStatus::Loading,
            "a component id that is not a file must not read as a missing plugin"
        );
        assert_eq!(fx.runtime_backend, PluginRuntimeBackend::ExternalBridge);
        assert!(fx.is_bridge_hosted_external_module());
        assert_eq!(
            fx.vst3_state.as_ref().map(|s| s.as_ref().clone()),
            Some(class_info),
            "the AU's opaque ClassInfo bytes must survive save/load"
        );
    }
}

#[cfg(test)]
mod conductor_lane_persistence_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        GlobalLaneKind, TimelineState, GLOBAL_LANE_MAX_HEIGHT,
    };

    /// Folding the tempo lane away is an arrangement of the workspace, not a
    /// transient view: reopening the project has to return it folded.
    #[test]
    fn folded_conductor_lanes_survive_a_project_roundtrip() {
        let mut state = TimelineState::default();
        state.tempo_track_collapsed = true;
        state.marker_track_collapsed = true;
        state
            .global_lane_heights
            .set(GlobalLaneKind::Arranger, Some(60.0));
        state
            .global_lane_heights
            .set(GlobalLaneKind::TimeSignature, Some(88.0));

        let bytes = encode_project(&FutureboardProject::from(&state));
        let decoded = decode_project(&bytes).expect("decode");
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&decoded, &mut restored);

        assert!(restored.tempo_track_collapsed);
        assert!(restored.marker_track_collapsed);
        assert!(!restored.region_track_collapsed);
        assert!(!restored.time_signature_track_collapsed);
        assert_eq!(
            restored.global_lane_heights.get(GlobalLaneKind::Arranger),
            Some(60.0)
        );
        assert_eq!(
            restored
                .global_lane_heights
                .get(GlobalLaneKind::TimeSignature),
            Some(88.0)
        );
        // An un-dragged lane stays at "default", not at whatever the default
        // happened to be on the machine that saved the file.
        assert_eq!(
            restored.global_lane_heights.get(GlobalLaneKind::Marker),
            None
        );
    }

    /// The conductor lanes never scroll, so a height from a hand-edited or
    /// newer file must not be able to push the arrangement off screen.
    #[test]
    fn a_lane_height_from_disk_is_clamped_to_the_drag_limits() {
        let mut project = FutureboardProject::new("Hostile");
        project.global_lanes.tempo_height = Some(10_000.0);
        let mut restored = TimelineState::default();
        let _ = apply_to_timeline(&project, &mut restored);
        assert_eq!(
            restored.global_lane_heights.get(GlobalLaneKind::Tempo),
            Some(GLOBAL_LANE_MAX_HEIGHT)
        );
    }
}
