//! Platform-neutral description of the project as ARA sees it, plus the traits
//! the host application implements to serve an ARA plug-in.
//!
//! Nothing here mentions `ara2_bridge`, GPUI, or the audio engine. The app fills
//! these records from its own state; [`crate::AraSession`] turns them into ARA
//! graph objects and keeps the two in sync.

use crate::error::AraResult;

/// Stable identity of one decoded audio asset (Futureboard's clip asset key).
///
/// Becomes the `persistentID` of an `ARAAudioSource`, so it must survive save,
/// load, and undo — a plug-in restoring an archive matches its stored objects by
/// this string.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AraSourceKey(pub String);

/// Stable identity of one audio clip (Futureboard's `ClipState::id`).
///
/// Becomes the `persistentID` of both the `ARAAudioModification` and its
/// `ARAPlaybackRegion`: edits belong to a clip, not to the underlying file, so
/// two clips of the same file can be tuned differently.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AraClipKey(pub String);

/// Stable identity of one track, backing an `ARARegionSequence`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AraTrackKey(pub String);

macro_rules! key_str {
    ($name:ident) => {
        impl $name {
            /// Borrows the underlying identifier.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

key_str!(AraSourceKey);
key_str!(AraClipKey);
key_str!(AraTrackKey);

/// RGB colour in the 0..=1 range, as ARA expresses object colours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AraColor {
    /// Red channel.
    pub red: f32,
    /// Green channel.
    pub green: f32,
    /// Blue channel.
    pub blue: f32,
}

/// One decoded audio asset offered to the plug-in.
#[derive(Clone, Debug, PartialEq)]
pub struct AraAudioSourceDesc {
    /// Persistent identity.
    pub key: AraSourceKey,
    /// Display name (usually the file name).
    pub name: String,
    /// Native sample rate of the asset, not the project rate.
    pub sample_rate: f64,
    /// Length in frames per channel.
    pub frame_count: i64,
    /// Channel count of the asset.
    pub channel_count: i32,
}

/// One track, as an ARA region sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct AraRegionSequenceDesc {
    /// Persistent identity.
    pub key: AraTrackKey,
    /// Display name.
    pub name: String,
    /// Arrangement order, used by plug-ins for lane ordering.
    pub order_index: i32,
    /// Track colour, when the track has one.
    pub color: Option<AraColor>,
}

/// Playback transformations the host is asking the plug-in to perform.
///
/// Only flags the plug-in advertises in its factory are actually requested;
/// [`crate::AraFactoryInfo::supported_transforms`] reports what is available and
/// [`AraPlaybackTransform::intersect`] narrows a request to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AraPlaybackTransform {
    /// Stretch modification time to playback time.
    pub timestretch: bool,
    /// Stretch while following the musical context tempo map.
    pub timestretch_reflecting_tempo: bool,
    /// Let the plug-in shape the head of the region.
    pub content_based_fade_at_head: bool,
    /// Let the plug-in shape the tail of the region.
    pub content_based_fade_at_tail: bool,
}

impl AraPlaybackTransform {
    /// No transformation: playback time maps 1:1 onto modification time.
    pub const NONE: Self = Self {
        timestretch: false,
        timestretch_reflecting_tempo: false,
        content_based_fade_at_head: false,
        content_based_fade_at_tail: false,
    };

    /// Whether any transformation is requested.
    pub fn is_none(self) -> bool {
        self == Self::NONE
    }

    /// Keeps only the flags present in `supported`.
    pub fn intersect(self, supported: Self) -> Self {
        Self {
            timestretch: self.timestretch && supported.timestretch,
            timestretch_reflecting_tempo: self.timestretch_reflecting_tempo
                && supported.timestretch_reflecting_tempo,
            content_based_fade_at_head: self.content_based_fade_at_head
                && supported.content_based_fade_at_head,
            content_based_fade_at_tail: self.content_based_fade_at_tail
                && supported.content_based_fade_at_tail,
        }
    }
}

/// One audio clip, as an ARA audio modification plus its playback region.
///
/// All four time values are in seconds. Modification time is measured from the
/// start of the audio source; playback time is timeline position.
#[derive(Clone, Debug, PartialEq)]
pub struct AraPlaybackRegionDesc {
    /// Persistent identity of the clip.
    pub key: AraClipKey,
    /// The asset this clip reads from.
    pub source: AraSourceKey,
    /// The track this clip sits on.
    pub track: AraTrackKey,
    /// Display name.
    pub name: String,
    /// Offset of the clip's first frame inside the source.
    pub start_in_modification: f64,
    /// Trimmed length inside the source.
    pub duration_in_modification: f64,
    /// Timeline position of the clip.
    pub start_in_playback: f64,
    /// Timeline length of the clip.
    pub duration_in_playback: f64,
    /// Requested playback transformations.
    pub transform: AraPlaybackTransform,
    /// Clip colour, when the clip has one.
    pub color: Option<AraColor>,
}

/// The whole ARA-visible project, supplied as a snapshot and diffed on apply.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AraGraph {
    /// Document name shown by the plug-in.
    pub name: Option<String>,
    /// Every asset referenced by at least one region.
    pub sources: Vec<AraAudioSourceDesc>,
    /// Every track that carries at least one region.
    pub sequences: Vec<AraRegionSequenceDesc>,
    /// Every ARA-managed clip.
    pub regions: Vec<AraPlaybackRegionDesc>,
}

impl AraGraph {
    /// Returns the first structural problem, or `Ok(())`.
    ///
    /// Checked before touching the plug-in so a malformed snapshot fails loudly
    /// on the host side rather than half-applying across the ABI.
    pub fn validate(&self) -> AraResult<()> {
        use crate::error::AraHostError;
        use std::collections::HashSet;

        let mut sources = HashSet::new();
        for source in &self.sources {
            if source.frame_count <= 0 || source.sample_rate <= 0.0 || source.channel_count <= 0 {
                return Err(AraHostError::invalid(format!(
                    "audio source '{}' has an empty or negative shape",
                    source.key.as_str()
                )));
            }
            if !sources.insert(source.key.clone()) {
                return Err(AraHostError::invalid(format!(
                    "duplicate audio source '{}'",
                    source.key.as_str()
                )));
            }
        }

        let mut sequences = HashSet::new();
        for sequence in &self.sequences {
            if !sequences.insert(sequence.key.clone()) {
                return Err(AraHostError::invalid(format!(
                    "duplicate region sequence '{}'",
                    sequence.key.as_str()
                )));
            }
        }

        let mut regions = HashSet::new();
        for region in &self.regions {
            if !regions.insert(region.key.clone()) {
                return Err(AraHostError::invalid(format!(
                    "duplicate playback region '{}'",
                    region.key.as_str()
                )));
            }
            if !sources.contains(&region.source) {
                return Err(AraHostError::invalid(format!(
                    "region '{}' references unknown source '{}'",
                    region.key.as_str(),
                    region.source.as_str()
                )));
            }
            if !sequences.contains(&region.track) {
                return Err(AraHostError::invalid(format!(
                    "region '{}' references unknown track '{}'",
                    region.key.as_str(),
                    region.track.as_str()
                )));
            }
            if region.duration_in_modification <= 0.0 || region.duration_in_playback <= 0.0 {
                return Err(AraHostError::invalid(format!(
                    "region '{}' has a non-positive duration",
                    region.key.as_str()
                )));
            }
            if region.start_in_modification < 0.0 {
                return Err(AraHostError::invalid(format!(
                    "region '{}' starts before its source",
                    region.key.as_str()
                )));
            }
        }

        Ok(())
    }

    /// How far this graph departs from the one a document holds.
    ///
    /// Expects a graph that passed [`Self::validate`], whose keys are unique.
    #[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
    pub(crate) fn change_from(&self, held: &impl HeldGraph) -> AraGraphChange {
        // Keys are unique on both sides, so equal counts plus every key found
        // means equal key sets.
        if held.counts() != (self.sources.len(), self.sequences.len(), self.regions.len()) {
            return AraGraphChange::Structure;
        }
        // A source's properties are the audio itself: a new length, rate or
        // channel count is new material, and ARA only lets those change with
        // the plug-in's sample access off.
        let sources_same = self
            .sources
            .iter()
            .all(|desc| held.source(&desc.key) == Some(desc));
        let sequences_found = self
            .sequences
            .iter()
            .all(|desc| held.sequence(&desc.key).is_some());
        // A modification is created on its source for good, and the editor's
        // selection names regions by track, so a region that changes either is
        // rebuilt rather than updated.
        let regions_found = self.regions.iter().all(|desc| {
            held.region(&desc.key)
                .is_some_and(|region| region.source == desc.source && region.track == desc.track)
        });
        if !(sources_same && sequences_found && regions_found) {
            return AraGraphChange::Structure;
        }

        let name_same = self
            .name
            .as_deref()
            .is_none_or(|name| held.document_name() == Some(name));
        let sequences_same = self
            .sequences
            .iter()
            .all(|desc| held.sequence(&desc.key) == Some(desc));
        let regions_same = self
            .regions
            .iter()
            .all(|desc| held.region(&desc.key) == Some(desc));
        if name_same && sequences_same && regions_same {
            AraGraphChange::Unchanged
        } else {
            AraGraphChange::Properties
        }
    }
}

/// How far a new [`AraGraph`] departs from the one a document already holds,
/// which decides what the host has to do around applying it.
///
/// ARA lets renderer assignments change only while the plug-in is not in its
/// render state, so a change that adds or removes playback regions needs the
/// renderer out of the audio graph first. Property updates are ordinary model
/// edits, which a host may make while regions render: the plug-in synchronises
/// its render threads between `beginEditing` and `endEditing` (see the model
/// graph notes in `ARAInterface.h`, and the SDK test plug-in, which gates its
/// renderers exactly there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AraGraphChange {
    /// Every object exists with exactly this description; applying the graph
    /// makes no plug-in call.
    Unchanged,
    /// The same objects, some with new properties: a region moved, trimmed,
    /// renamed or recoloured, a track renamed, reordered or recoloured, the
    /// document renamed. Applying creates and destroys nothing, so every
    /// renderer assignment and the editor's selection stay valid.
    Properties,
    /// Objects are created or destroyed, a region changes its source or track,
    /// or a source changes shape.
    Structure,
}

/// The graph a document holds, looked up by key, for
/// [`AraGraph::change_from`].
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(crate) trait HeldGraph {
    /// The document name last published, if any.
    fn document_name(&self) -> Option<&str>;
    /// How many sources, sequences and regions exist.
    fn counts(&self) -> (usize, usize, usize);
    /// The description a source was last published with.
    fn source(&self, key: &AraSourceKey) -> Option<&AraAudioSourceDesc>;
    /// The description a sequence was last published with.
    fn sequence(&self, key: &AraTrackKey) -> Option<&AraRegionSequenceDesc>;
    /// The description a region was last published with.
    fn region(&self, key: &AraClipKey) -> Option<&AraPlaybackRegionDesc>;
}

/// One tempo map entry: a timeline instant and its position in quarter notes.
///
/// ARA requires at least two entries and a strictly increasing sequence in both
/// dimensions; [`AraMusicalTimeline::validate`] enforces that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AraTempoEntry {
    /// Position in seconds.
    pub time_seconds: f64,
    /// Position in quarter notes.
    pub quarter_position: f64,
}

/// One bar-signature change, positioned in quarter notes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AraBarSignature {
    /// Beats per bar.
    pub numerator: i32,
    /// Beat unit.
    pub denominator: i32,
    /// Position in quarter notes.
    pub quarter_position: f64,
}

/// One key-signature change, positioned in quarter notes.
///
/// Mirrors ARA's `ARAContentKeySignature`. As in ARA, the first key signature
/// also applies before its own position.
#[derive(Clone, Debug, PartialEq)]
pub struct AraKeySignature {
    /// The tonic as a circle-of-fifths index: C = 0, G = 1, F = -1.
    ///
    /// The index is also the spelling, so D♭ is -5 and C♯ is 7.
    pub root_fifths: i32,
    /// Which of the twelve semitones above the tonic belong to the key.
    ///
    /// Index 0 is the tonic itself and is always used.
    pub intervals: [bool; 12],
    /// Display name, or `None` to let the plug-in name the key.
    ///
    /// ARA requires sharps and flats as U+266F and U+266D, not `#` and `b`.
    pub name: Option<String>,
    /// Position in quarter notes.
    pub quarter_position: f64,
}

/// The project's musical context: tempo map, bar signatures and key.
///
/// An empty [`Self::keys`] means the project has no key. Key-signature content
/// is then reported unavailable rather than invented, and the plug-in keeps
/// its own detection. Chords are not published.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AraMusicalTimeline {
    /// Tempo entries, ascending.
    pub tempo: Vec<AraTempoEntry>,
    /// Bar signatures, ascending.
    pub bars: Vec<AraBarSignature>,
    /// Key signatures, ascending. Empty when the project has no key.
    pub keys: Vec<AraKeySignature>,
}

impl AraMusicalTimeline {
    /// Returns the first structural problem, or `Ok(())`.
    pub fn validate(&self) -> AraResult<()> {
        use crate::error::AraHostError;

        if self.tempo.len() < 2 {
            return Err(AraHostError::invalid(
                "an ARA tempo map needs at least two entries",
            ));
        }
        for pair in self.tempo.windows(2) {
            if pair[1].time_seconds <= pair[0].time_seconds
                || pair[1].quarter_position <= pair[0].quarter_position
            {
                return Err(AraHostError::invalid(
                    "ARA tempo entries must strictly increase in time and quarters",
                ));
            }
        }
        if self.bars.is_empty() {
            return Err(AraHostError::invalid(
                "an ARA musical context needs at least one bar signature",
            ));
        }
        for bar in &self.bars {
            if bar.numerator <= 0 || bar.denominator <= 0 {
                return Err(AraHostError::invalid(
                    "ARA bar signatures need positive numerator and denominator",
                ));
            }
        }
        for pair in self.bars.windows(2) {
            if pair[1].quarter_position <= pair[0].quarter_position {
                return Err(AraHostError::invalid(
                    "ARA bar signatures must strictly increase in quarters",
                ));
            }
        }
        for key in &self.keys {
            if !key.quarter_position.is_finite() {
                return Err(AraHostError::invalid(
                    "ARA key signatures need a finite position",
                ));
            }
            if !key.intervals[0] {
                return Err(AraHostError::invalid(
                    "an ARA key signature must use its root interval",
                ));
            }
            if key.name.as_ref().is_some_and(|name| name.contains('\0')) {
                return Err(AraHostError::invalid(
                    "an ARA key signature name must not contain NUL",
                ));
            }
        }
        for pair in self.keys.windows(2) {
            if pair[1].quarter_position <= pair[0].quarter_position {
                return Err(AraHostError::invalid(
                    "ARA key signatures must strictly increase in quarters",
                ));
            }
        }
        Ok(())
    }
}

/// A random-access reader over one audio asset.
///
/// ARA calls a reader from at most one thread at a time, but may drive several
/// readers of the same source concurrently, so each reader owns its own cursor.
/// Reads happen off the model thread and must not block on it.
pub trait AraSampleReader: Send + 'static {
    /// Channels this reader always fills.
    fn channel_count(&self) -> usize;

    /// Total frames per channel.
    fn frame_count(&self) -> i64;

    /// Reads planar samples starting at `start_frame`.
    ///
    /// `out` has exactly [`Self::channel_count`] slices of equal length. Frames
    /// outside the source must be written as silence rather than refused: ARA
    /// permits reads that run past the end.
    fn read_planar_f32(&mut self, start_frame: i64, out: &mut [&mut [f32]]) -> AraResult<()>;
}

/// Resolves an asset key into independent readers.
pub trait AraAudioAccess: Send + Sync + 'static {
    /// Opens a fresh reader positioned at the start of the asset.
    fn open_reader(&self, source: &AraSourceKey) -> AraResult<Box<dyn AraSampleReader>>;
}

/// A transport action requested by the plug-in's editor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AraTransportRequest {
    /// Begin playback.
    Start,
    /// Stop playback.
    Stop,
    /// Locate to a timeline position in seconds.
    SetPosition(f64),
    /// Set the cycle range in seconds.
    SetCycleRange {
        /// Cycle start.
        start: f64,
        /// Cycle length.
        duration: f64,
    },
    /// Enable or disable cycling.
    EnableCycle(bool),
}

/// Receives transport requests coming from a plug-in editor.
///
/// Called from whatever thread the plug-in uses, so an implementation must post
/// the request to the transport owner instead of acting inline.
pub trait AraTransportControl: Send + Sync + 'static {
    /// Handles one request.
    fn request(&self, request: AraTransportRequest);
}

/// An asynchronous model change reported by the plug-in.
#[derive(Clone, Debug, PartialEq)]
pub enum AraModelUpdate {
    /// Analysis of one source started, progressed, or completed.
    AnalysisProgress {
        /// The analysed asset, when the host still knows it.
        source: Option<AraSourceKey>,
        /// Raw ARA analysis-progress state (start / update / complete).
        state: i32,
        /// Progress in the 0..=1 range.
        value: f32,
    },
    /// Content of one source changed.
    SourceContentChanged {
        /// The affected asset, when the host still knows it.
        source: Option<AraSourceKey>,
    },
    /// Content of one clip's modification changed.
    ModificationContentChanged {
        /// The affected clip, when the host still knows it.
        clip: Option<AraClipKey>,
    },
    /// Content of one clip's playback region changed.
    RegionContentChanged {
        /// The affected clip, when the host still knows it.
        clip: Option<AraClipKey>,
    },
    /// Persistent document data changed, so the project is dirty.
    DocumentDataChanged,
}

/// Receives model updates.
///
/// ARA lets a plug-in report these only from inside
/// [`crate::AraSession::notify_model_updates`], so a conforming plug-in calls
/// this re-entrantly on the model thread; a non-conforming one may use its own
/// threads. Either way implementations must be allocation-light, non-blocking
/// and must not call back into the session: push onto a bounded queue and let
/// the UI drain it.
pub trait AraModelObserver: Send + Sync + 'static {
    /// Handles one update.
    fn notify(&self, update: AraModelUpdate);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(key: &str) -> AraAudioSourceDesc {
        AraAudioSourceDesc {
            key: key.into(),
            name: key.to_owned(),
            sample_rate: 48_000.0,
            frame_count: 48_000,
            channel_count: 2,
        }
    }

    fn sequence(key: &str) -> AraRegionSequenceDesc {
        AraRegionSequenceDesc {
            key: key.into(),
            name: key.to_owned(),
            order_index: 0,
            color: None,
        }
    }

    fn region(key: &str, source_key: &str, track_key: &str) -> AraPlaybackRegionDesc {
        AraPlaybackRegionDesc {
            key: key.into(),
            source: source_key.into(),
            track: track_key.into(),
            name: key.to_owned(),
            start_in_modification: 0.0,
            duration_in_modification: 1.0,
            start_in_playback: 0.0,
            duration_in_playback: 1.0,
            transform: AraPlaybackTransform::NONE,
            color: None,
        }
    }

    #[test]
    fn valid_graph_passes() {
        let graph = AraGraph {
            name: Some("project".into()),
            sources: vec![source("asset-1")],
            sequences: vec![sequence("track-1")],
            regions: vec![region("clip-1", "asset-1", "track-1")],
        };
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn region_referencing_unknown_source_is_rejected() {
        let graph = AraGraph {
            sources: vec![source("asset-1")],
            sequences: vec![sequence("track-1")],
            regions: vec![region("clip-1", "asset-missing", "track-1")],
            ..AraGraph::default()
        };
        assert!(graph.validate().is_err());
    }

    #[test]
    fn duplicate_clip_keys_are_rejected() {
        let graph = AraGraph {
            sources: vec![source("asset-1")],
            sequences: vec![sequence("track-1")],
            regions: vec![
                region("clip-1", "asset-1", "track-1"),
                region("clip-1", "asset-1", "track-1"),
            ],
            ..AraGraph::default()
        };
        assert!(graph.validate().is_err());
    }

    #[test]
    fn transform_narrows_to_supported_flags() {
        let requested = AraPlaybackTransform {
            timestretch: true,
            timestretch_reflecting_tempo: true,
            content_based_fade_at_head: true,
            content_based_fade_at_tail: false,
        };
        let supported = AraPlaybackTransform {
            timestretch: true,
            timestretch_reflecting_tempo: false,
            content_based_fade_at_head: false,
            content_based_fade_at_tail: true,
        };
        assert_eq!(
            requested.intersect(supported),
            AraPlaybackTransform {
                timestretch: true,
                ..AraPlaybackTransform::NONE
            }
        );
    }

    #[test]
    fn tempo_map_needs_two_increasing_entries() {
        let mut timeline = AraMusicalTimeline {
            tempo: vec![AraTempoEntry {
                time_seconds: 0.0,
                quarter_position: 0.0,
            }],
            bars: vec![AraBarSignature {
                numerator: 4,
                denominator: 4,
                quarter_position: 0.0,
            }],
            keys: Vec::new(),
        };
        assert!(timeline.validate().is_err());

        timeline.tempo.push(AraTempoEntry {
            time_seconds: 0.5,
            quarter_position: 1.0,
        });
        assert!(timeline.validate().is_ok());

        timeline.tempo[1].quarter_position = 0.0;
        assert!(timeline.validate().is_err());
    }

    fn valid_timeline() -> AraMusicalTimeline {
        AraMusicalTimeline {
            tempo: vec![
                AraTempoEntry {
                    time_seconds: 0.0,
                    quarter_position: 0.0,
                },
                AraTempoEntry {
                    time_seconds: 0.5,
                    quarter_position: 1.0,
                },
            ],
            bars: vec![AraBarSignature {
                numerator: 4,
                denominator: 4,
                quarter_position: 0.0,
            }],
            keys: Vec::new(),
        }
    }

    fn a_minor(quarter_position: f64) -> AraKeySignature {
        let mut intervals = [false; 12];
        for interval in [0, 2, 3, 5, 7, 8, 10] {
            intervals[interval] = true;
        }
        AraKeySignature {
            root_fifths: 3,
            intervals,
            name: Some("A Natural Minor".to_owned()),
            quarter_position,
        }
    }

    #[test]
    fn a_timeline_without_keys_is_valid() {
        assert!(valid_timeline().validate().is_ok());
    }

    #[test]
    fn key_signatures_must_strictly_increase() {
        let mut timeline = valid_timeline();
        timeline.keys = vec![a_minor(0.0), a_minor(8.0)];
        assert!(timeline.validate().is_ok());

        timeline.keys[1].quarter_position = 0.0;
        assert!(timeline.validate().is_err());

        timeline.keys[1].quarter_position = f64::NAN;
        assert!(timeline.validate().is_err());
    }

    #[test]
    fn a_key_signature_must_use_its_root() {
        let mut timeline = valid_timeline();
        let mut key = a_minor(0.0);
        key.intervals[0] = false;
        timeline.keys = vec![key];
        assert!(timeline.validate().is_err());
    }

    #[test]
    fn a_key_signature_name_must_not_contain_nul() {
        let mut timeline = valid_timeline();
        let mut key = a_minor(0.0);
        key.name = Some("A\0minor".to_owned());
        timeline.keys = vec![key];
        assert!(timeline.validate().is_err());
    }

    /// An applied graph stands in for the document that holds it.
    impl HeldGraph for AraGraph {
        fn document_name(&self) -> Option<&str> {
            self.name.as_deref()
        }

        fn counts(&self) -> (usize, usize, usize) {
            (self.sources.len(), self.sequences.len(), self.regions.len())
        }

        fn source(&self, key: &AraSourceKey) -> Option<&AraAudioSourceDesc> {
            self.sources.iter().find(|desc| desc.key == *key)
        }

        fn sequence(&self, key: &AraTrackKey) -> Option<&AraRegionSequenceDesc> {
            self.sequences.iter().find(|desc| desc.key == *key)
        }

        fn region(&self, key: &AraClipKey) -> Option<&AraPlaybackRegionDesc> {
            self.regions.iter().find(|desc| desc.key == *key)
        }
    }

    fn two_clips() -> AraGraph {
        AraGraph {
            name: Some("Futureboard — Vox".into()),
            sources: vec![source("asset-1"), source("asset-2")],
            sequences: vec![sequence("track-1")],
            regions: vec![
                region("clip-1", "asset-1", "track-1"),
                region("clip-2", "asset-2", "track-1"),
            ],
        }
    }

    #[test]
    fn the_same_graph_is_unchanged() {
        let held = two_clips();
        assert_eq!(two_clips().change_from(&held), AraGraphChange::Unchanged);
        // No name means "leave the document's name alone".
        let unnamed = AraGraph {
            name: None,
            ..two_clips()
        };
        assert_eq!(unnamed.change_from(&held), AraGraphChange::Unchanged);
    }

    #[test]
    fn moving_trimming_or_renaming_a_clip_changes_only_properties() {
        let held = two_clips();

        let mut moved = two_clips();
        moved.regions[1].start_in_playback += 2.0;
        assert_eq!(moved.change_from(&held), AraGraphChange::Properties);

        let mut trimmed = two_clips();
        trimmed.regions[0].start_in_modification = 0.25;
        trimmed.regions[0].duration_in_modification = 0.5;
        trimmed.regions[0].duration_in_playback = 0.5;
        assert_eq!(trimmed.change_from(&held), AraGraphChange::Properties);

        let mut renamed = two_clips();
        renamed.regions[0].name = "Verse".into();
        renamed.sequences[0].name = "Lead Vox".into();
        renamed.name = Some("Futureboard — Lead Vox".into());
        assert_eq!(renamed.change_from(&held), AraGraphChange::Properties);

        let mut recoloured = two_clips();
        recoloured.regions[1].color = Some(AraColor {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
        });
        assert_eq!(recoloured.change_from(&held), AraGraphChange::Properties);
    }

    #[test]
    fn a_reordered_graph_with_the_same_objects_is_unchanged() {
        let held = two_clips();
        let mut reordered = two_clips();
        reordered.regions.reverse();
        reordered.sources.reverse();
        assert_eq!(reordered.change_from(&held), AraGraphChange::Unchanged);
    }

    #[test]
    fn adding_or_removing_a_clip_is_structural() {
        let held = two_clips();

        let mut removed = two_clips();
        removed.regions.pop();
        removed.sources.pop();
        assert_eq!(removed.change_from(&held), AraGraphChange::Structure);

        // A split: one more region on a source that already exists.
        let mut split = two_clips();
        split.regions.push(region("clip-3", "asset-2", "track-1"));
        assert_eq!(split.change_from(&held), AraGraphChange::Structure);

        // Same count, different clip: delete one and add another.
        let mut swapped = two_clips();
        swapped.regions[1].key = "clip-9".into();
        assert_eq!(swapped.change_from(&held), AraGraphChange::Structure);
    }

    #[test]
    fn a_clip_that_changes_its_source_or_a_source_that_changes_shape_is_structural() {
        let held = two_clips();

        let mut resourced = two_clips();
        resourced.regions[0].source = "asset-2".into();
        assert_eq!(resourced.change_from(&held), AraGraphChange::Structure);

        let mut longer = two_clips();
        longer.sources[0].frame_count *= 2;
        assert_eq!(longer.change_from(&held), AraGraphChange::Structure);
    }
}
