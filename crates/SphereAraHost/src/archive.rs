//! What a stored ARA document holds, where a restore puts it, and whether the
//! restore landed.
//!
//! A plug-in restores archived state by persistent ID and says nothing when an
//! ID has no match: the call succeeds, the archived state is ignored, and the
//! next store no longer contains it. So the host has to know, on its own
//! terms, what an archive holds ([`AraArchiveIdentity`], captured with the
//! bytes), how to place it when IDs moved ([`AraRestoreMap`]), and whether the
//! plug-in actually took it ([`AraRestoreReport`]).
//!
//! Everything here is plain data and pure functions, the same on every
//! platform; `imp` turns a [`RestorePlan`] into the bridge's filter.

// The stub reports every restore unsupported and never plans one.
#![cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]

use std::collections::{HashMap, HashSet};

use crate::error::{AraHostError, AraResult};
use crate::model::{
    AraAudioSourceDesc, AraClipKey, AraKeySignature, AraPlaybackRegionDesc, AraSourceKey,
    ara_persistent_id,
};

/// ARA's `kARAContentGradeInitial`: placeholder content, nothing analysed.
pub(crate) const GRADE_INITIAL: i32 = 0;

/// ARA's `kARAContentGradeDetected`: content an analysis produced.
pub(crate) const GRADE_DETECTED: i32 = 1;

/// Fingerprint of the key signatures a plug-in was offered.
///
/// Stored with an archive so a later restore can tell whether the project key
/// changed in between; only then is the plug-in told the harmony changed.
/// The value is persisted, so its derivation is fixed: FNV-1a (64-bit) over a
/// versioned little-endian encoding of every key's root, interval set,
/// position and name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AraKeyDescriptor(pub u64);

impl AraKeyDescriptor {
    /// Encoding version, folded into every digest.
    const VERSION: u8 = 1;

    /// Describes `keys`, in order. No keys at all has a descriptor too.
    pub fn of(keys: &[AraKeySignature]) -> Self {
        let mut digest = Fnv1a::new();
        digest.write(&[Self::VERSION]);
        digest.write(&(keys.len() as u64).to_le_bytes());
        for key in keys {
            digest.write(&key.root_fifths.to_le_bytes());
            let mask = key
                .intervals
                .iter()
                .enumerate()
                .fold(0u16, |mask, (index, used)| {
                    mask | (u16::from(*used) << index)
                });
            digest.write(&mask.to_le_bytes());
            // `-0.0` and `0.0` are one position.
            let position = if key.quarter_position == 0.0 {
                0.0f64
            } else {
                key.quarter_position
            };
            digest.write(&position.to_bits().to_le_bytes());
            match key.name.as_deref() {
                Some(name) => {
                    digest.write(&[1]);
                    digest.write(&(name.len() as u64).to_le_bytes());
                    digest.write(name.as_bytes());
                }
                None => digest.write(&[0]),
            }
        }
        Self(digest.finish())
    }
}

/// 64-bit FNV-1a, spelled out because the value is persisted and std's
/// hashers are free to change between releases.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Whether a restore must be followed by a harmony-changed notification.
///
/// Only when the archive recorded the key it was stored with and that key
/// differs from the one the plug-in is offered now. Plug-ins keep per-clip
/// copies of the key (Melodyne does), which the notification re-copies; with
/// an unchanged key it would only overwrite scales the user set. An unknown
/// (legacy) record never triggers it.
pub(crate) fn needs_harmony_resync(
    recorded: Option<AraKeyDescriptor>,
    current: AraKeyDescriptor,
    harmonic_content: bool,
) -> bool {
    harmonic_content && recorded.is_some_and(|recorded| recorded != current)
}

/// One audio source as it stood when an archive was stored.
#[derive(Clone, Debug, PartialEq)]
pub struct AraArchivedSource {
    /// The persistent ID the plug-in stored the source's state under.
    pub persistent_id: String,
    /// The host key the source was published from.
    pub key: AraSourceKey,
    /// Sample rate of the audio.
    pub sample_rate: f64,
    /// Frames per channel.
    pub frame_count: i64,
    /// Channel count.
    pub channel_count: i32,
}

impl AraArchivedSource {
    /// Whether `desc` describes audio of the same shape.
    ///
    /// Restored source state describes the audio it was analysed from, so it
    /// may only land on a source of the same length, rate and channel count.
    pub fn same_shape(&self, desc: &AraAudioSourceDesc) -> bool {
        self.sample_rate == desc.sample_rate
            && self.frame_count == desc.frame_count
            && self.channel_count == desc.channel_count
    }
}

/// One audio modification as it stood when an archive was stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AraArchivedModification {
    /// The persistent ID the plug-in stored the modification's state under.
    pub persistent_id: String,
    /// The host clip the modification belonged to.
    pub clip: AraClipKey,
    /// Persistent ID of the audio source the modification was created on.
    pub source_persistent_id: String,
}

/// Which objects an archive holds, recorded in the same call that stored it.
///
/// ARA has no call that lists the IDs inside an archive, so this is the only
/// way to know them later: to tell a restore that placed nothing from one that
/// placed everything, and to map archived IDs onto current ones.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AraArchiveIdentity {
    /// Every audio source in the document, by host key.
    pub sources: Vec<AraArchivedSource>,
    /// Every audio modification in the document, by host clip.
    pub modifications: Vec<AraArchivedModification>,
    /// The key signatures the plug-in was offered, or `None` when unknown.
    pub keys: Option<AraKeyDescriptor>,
}

impl AraArchiveIdentity {
    /// The archived source stored under `persistent_id`.
    pub fn source(&self, persistent_id: &str) -> Option<&AraArchivedSource> {
        self.sources
            .iter()
            .find(|source| source.persistent_id == persistent_id)
    }

    /// The archived modification stored under `persistent_id`.
    pub fn modification(&self, persistent_id: &str) -> Option<&AraArchivedModification> {
        self.modifications
            .iter()
            .find(|modification| modification.persistent_id == persistent_id)
    }
}

/// A stored document and the identity it was stored with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AraStoredArchive {
    /// The plug-in's opaque archive, written under
    /// [`crate::AraFactoryInfo::document_archive_id`].
    pub bytes: Vec<u8>,
    /// What `bytes` holds.
    pub identity: AraArchiveIdentity,
}

/// Where archived objects land in the current graph, when their IDs moved.
///
/// Objects the map does not name are restored by matching persistent ID, as
/// without a map. An archived source is always restored together with every
/// one of its archived modifications that has a place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AraRestoreMap {
    /// Archived source persistent ID → current source.
    pub sources: Vec<(String, AraSourceKey)>,
    /// Archived modification persistent ID → current clip.
    pub modifications: Vec<(String, AraClipKey)>,
}

/// Which part of an archive one restore brings back.
///
/// ARA 2 lets a host restore an archive in parts, with several calls that
/// each name what they restore (partial persistency): a source whose audio is
/// offline can be restored later, from the same archive, once it is back,
/// without touching what was restored before. Restoring a source's state
/// always brings its modifications' along, and the plug-in's private
/// document data belongs with the first part restored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AraRestoreScope<'a> {
    /// Everything the archive holds that has a place in the graph, with the
    /// plug-in's private document data.
    #[default]
    Whole,
    /// Only these archived sources, by the persistent ID they were archived
    /// under, each with every archived modification of it that has a place;
    /// with the private document data only when `document_data` is set.
    /// Always restored through a filter, so it needs ARA 2 Final. Without an
    /// identity, each named source is taken to be archived under the ID its
    /// current source is published with, and so is each of its clips.
    Sources {
        /// Archived persistent IDs of the sources to restore.
        archived: &'a [String],
        /// Whether the plug-in's private document data is restored too.
        document_data: bool,
    },
}

/// A saved document to restore, and how to place it.
#[derive(Clone, Copy, Debug)]
pub struct AraRestoreRequest<'a> {
    /// The archive identifier the bytes were written under.
    pub archive_id: &'a str,
    /// The archive itself.
    pub bytes: &'a [u8],
    /// What the archive holds, when it was recorded at store time. `None` for
    /// archives written before identities were recorded.
    pub identity: Option<&'a AraArchiveIdentity>,
    /// Archived → current placement. `None` restores whatever matches by
    /// persistent ID, with no filter unless `scope` needs one.
    pub map: Option<&'a AraRestoreMap>,
    /// Which part of the archive to restore.
    pub scope: AraRestoreScope<'a>,
}

/// Whether restored state reached the plug-in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AraRestoreOutcome {
    /// The plug-in took the archived state.
    Matched,
    /// The plug-in found nothing to restore and is analysing afresh.
    Missed,
    /// No reliable evidence either way.
    Inconclusive,
}

/// What one restore did to one current audio source.
#[derive(Clone, Debug, PartialEq)]
pub struct AraSourceRestore {
    /// The current source.
    pub key: AraSourceKey,
    /// The persistent ID it is published under.
    pub persistent_id: String,
    /// The archived ID restored into it, when the host knew it: always with a
    /// map, otherwise only with an identity.
    pub archived_id: Option<String>,
    /// The verdict for this source.
    pub outcome: AraRestoreOutcome,
    /// The plug-in's note-content grade before the restore, `None` when it
    /// could not be read or the plug-in does not analyse notes.
    pub grade_before: Option<i32>,
    /// The same grade right after the restore.
    pub grade_after: Option<i32>,
    /// Whether note analysis is still incomplete after the restore, as the
    /// plug-in reports it; `None` when unknown. `Some(false)` settles any
    /// analysis the plug-in reported started and never completed.
    pub analysis_incomplete: Option<bool>,
}

/// The result of one restore.
///
/// Only [`Self::restored`] means the live document now holds the archive.
/// Anything else means some archived state may be missing from it, and a
/// store would lose that state for good.
#[derive(Clone, Debug, PartialEq)]
pub struct AraRestoreReport {
    /// Why the restore did not run, or the plug-in's refusal. Set means the
    /// archive must be treated as not restored.
    pub error: Option<AraHostError>,
    /// The verdict over the whole restore; see [`AraRestoreOutcome`].
    pub outcome: AraRestoreOutcome,
    /// One entry per current source the restore targeted.
    pub sources: Vec<AraSourceRestore>,
    /// Archived source IDs with no place in the current graph (known only
    /// with an identity).
    pub unplaced_sources: Vec<String>,
    /// Archived modification IDs with no place in the current graph (known
    /// only with an identity).
    pub unplaced_modifications: Vec<String>,
    /// Whether the plug-in was told the harmony changed afterwards.
    pub harmony_resynced: bool,
}

impl AraRestoreReport {
    /// A restore that never reached the plug-in.
    pub(crate) fn not_run(error: AraHostError) -> Self {
        Self {
            error: Some(error),
            outcome: AraRestoreOutcome::Missed,
            sources: Vec::new(),
            unplaced_sources: Vec::new(),
            unplaced_modifications: Vec::new(),
            harmony_resynced: false,
        }
    }

    /// Whether every archived state known to the host reached the plug-in.
    pub fn restored(&self) -> bool {
        self.error.is_none() && self.outcome == AraRestoreOutcome::Matched
    }
}

/// Judges one source from its note grade before and after a restore.
///
/// A source is created with nothing analysed (grade initial). A plug-in that
/// takes archived analysis has content at once; one that found nothing to
/// restore still has none and starts analysing. `analysis_seen` is any
/// analysis progress the source reported while the restore ran: a plug-in
/// that analysed during a restore did not simply take the archived result, so
/// it overrules a match (a very short file can finish analysing before the
/// grade is read).
///
/// `as_saved` says the recorded identity holds this source under the very ID
/// and shape it was restored by (see [`PlannedSource::as_saved`]). Its
/// archived state then went to exactly the object it was stored from, and a
/// grade still initial afterwards says the archive held no analysis for it
/// (stored before its analysis finished, say), not that the restore missed:
/// no evidence either way.
pub(crate) fn classify_source(
    before: Option<i32>,
    after: Option<i32>,
    analysis_seen: bool,
    as_saved: bool,
) -> AraRestoreOutcome {
    match (before, after) {
        (Some(GRADE_INITIAL), Some(after)) if after >= GRADE_DETECTED => {
            if analysis_seen {
                AraRestoreOutcome::Inconclusive
            } else {
                AraRestoreOutcome::Matched
            }
        }
        (Some(GRADE_INITIAL), Some(GRADE_INITIAL)) if as_saved => AraRestoreOutcome::Inconclusive,
        (Some(GRADE_INITIAL), Some(GRADE_INITIAL)) => AraRestoreOutcome::Missed,
        _ => AraRestoreOutcome::Inconclusive,
    }
}

/// Judges a whole restore from its per-source verdicts.
///
/// `expects` says whether the part of the archive the restore was asked for
/// holds any source (`None` when unknown: a whole legacy archive), and
/// `unplaced` whether any of that part had no place in the graph.
pub(crate) fn overall_outcome(
    sources: &[AraRestoreOutcome],
    expects: Option<bool>,
    unplaced: bool,
) -> AraRestoreOutcome {
    if sources.is_empty() {
        return match expects {
            // Nothing archived, nothing to miss.
            Some(false) if !unplaced => AraRestoreOutcome::Matched,
            // Archived sources, and none of them had a place.
            Some(_) => AraRestoreOutcome::Missed,
            None => AraRestoreOutcome::Inconclusive,
        };
    }
    let all = |wanted| sources.iter().all(|outcome| *outcome == wanted);
    if all(AraRestoreOutcome::Matched) {
        if unplaced {
            AraRestoreOutcome::Inconclusive
        } else {
            AraRestoreOutcome::Matched
        }
    } else if all(AraRestoreOutcome::Missed) {
        AraRestoreOutcome::Missed
    } else {
        AraRestoreOutcome::Inconclusive
    }
}

/// One current source a restore targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlannedSource {
    /// The current source.
    pub(crate) key: AraSourceKey,
    /// Its published persistent ID.
    pub(crate) current_id: String,
    /// The archived ID restored into it, when known.
    pub(crate) archived_id: Option<String>,
    /// The recorded identity holds this source under `current_id`, with the
    /// shape it has now, and the restore goes by ID: its state lands exactly
    /// where it was stored from. Never set for a filtered (mapped) restore.
    pub(crate) as_saved: bool,
}

/// An explicit `ARARestoreObjectsFilter`: (archived, current) persistent IDs,
/// and whether the private document data comes along.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlannedFilter {
    pub(crate) document_data: bool,
    pub(crate) sources: Vec<(String, String)>,
    pub(crate) modifications: Vec<(String, String)>,
}

/// How one restore is carried out, decided before the plug-in is touched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RestorePlan {
    /// The filter to pass, or `None` for the null filter.
    pub(crate) filter: Option<PlannedFilter>,
    /// The current sources whose note grade judges the restore.
    pub(crate) targets: Vec<PlannedSource>,
    /// Archived source IDs with no place (identity, or a scope, only).
    pub(crate) unplaced_sources: Vec<String>,
    /// Archived modification IDs with no place (identity only).
    pub(crate) unplaced_modifications: Vec<String>,
    /// Whether the part of the archive asked for holds any source, `None`
    /// when unknown (see [`overall_outcome`]).
    pub(crate) expects: Option<bool>,
}

/// The rule the bridge applies to every filter ID.
fn usable_id(id: &str) -> bool {
    !id.is_empty() && id.is_ascii() && !id.contains('\0')
}

/// Decides how to restore an archive into the graph `sources` and `regions`
/// describe.
///
/// Without a map the restore goes out with no filter, the path every plug-in
/// supports, and targets the current sources the archive holds (all of them
/// when the identity is unknown).
///
/// With a map the restore goes out with a filter that has document data
/// selected. It lists every mapped source, every other current source whose
/// ID the archive holds (identity pairs), and for each listed source every
/// archived modification that has a place on it: ARA restores only what a
/// filter lists, and restoring a source's state requires restoring its
/// modifications' too. A mapping is refused when its current object is not
/// in the graph, when an archived or current ID appears twice, and, when the
/// identity is known, when the archive does not hold the mapped object, when
/// a mapped source's shape differs, or when a mapped modification's source
/// is not restored onto the current clip's source. An identity pair whose
/// shape differs is left out and reported unplaced instead.
///
/// A [`AraRestoreScope::Sources`] restore always has a filter and lists only
/// the named sources and their modifications; see [`plan_scoped`].
pub(crate) fn plan_restore(
    identity: Option<&AraArchiveIdentity>,
    map: Option<&AraRestoreMap>,
    scope: AraRestoreScope<'_>,
    sources: &[AraAudioSourceDesc],
    regions: &[AraPlaybackRegionDesc],
) -> AraResult<RestorePlan> {
    if let AraRestoreScope::Sources {
        archived,
        document_data,
    } = scope
    {
        return plan_scoped(identity, map, archived, document_data, sources, regions);
    }
    let Some(map) = map else {
        return Ok(plan_by_id(identity, sources, regions));
    };

    let source_descs: HashMap<&AraSourceKey, &AraAudioSourceDesc> =
        sources.iter().map(|desc| (&desc.key, desc)).collect();
    let region_descs: HashMap<&AraClipKey, &AraPlaybackRegionDesc> =
        regions.iter().map(|desc| (&desc.key, desc)).collect();

    // (archived ID, current key) per listed source, in listing order.
    let mut listed_sources: Vec<(String, AraSourceKey)> = Vec::new();
    let mut explicit_archived: HashSet<&str> = HashSet::new();
    let mut explicit_current: HashSet<&AraSourceKey> = HashSet::new();
    for (archived_id, key) in &map.sources {
        if !usable_id(archived_id) {
            return Err(AraHostError::invalid(format!(
                "restore map names an unusable archived source ID {archived_id:?}"
            )));
        }
        let Some(desc) = source_descs.get(key) else {
            return Err(AraHostError::invalid(format!(
                "restore map targets audio source '{}', which is not in the graph",
                key.as_str()
            )));
        };
        if !explicit_archived.insert(archived_id) || !explicit_current.insert(key) {
            return Err(AraHostError::invalid(
                "restore map names an audio source twice",
            ));
        }
        if let Some(identity) = identity {
            let Some(archived) = identity.source(archived_id) else {
                return Err(AraHostError::invalid(format!(
                    "the archive holds no audio source '{archived_id}'"
                )));
            };
            if !archived.same_shape(desc) {
                return Err(AraHostError::invalid(format!(
                    "archived audio source '{archived_id}' has another length, rate or \
                     channel count than '{}'",
                    key.as_str()
                )));
            }
        }
        listed_sources.push((archived_id.clone(), key.clone()));
    }
    for desc in sources {
        if explicit_current.contains(&desc.key) {
            continue;
        }
        let id = ara_persistent_id(desc.key.as_str());
        // Its archived state was mapped onto another source.
        if explicit_archived.contains(id.as_ref()) {
            continue;
        }
        let holds = match identity {
            Some(identity) => identity
                .source(&id)
                .is_some_and(|archived| archived.same_shape(desc)),
            None => true,
        };
        if holds {
            listed_sources.push((id.into_owned(), desc.key.clone()));
        }
    }
    let archived_source_of: HashMap<&AraSourceKey, &str> = listed_sources
        .iter()
        .map(|(archived_id, key)| (key, archived_id.as_str()))
        .collect();

    let mut listed_modifications: Vec<(String, String)> = Vec::new();
    let mut explicit_modifications: HashSet<&str> = HashSet::new();
    let mut explicit_clips: HashSet<&AraClipKey> = HashSet::new();
    for (archived_id, clip) in &map.modifications {
        if !usable_id(archived_id) {
            return Err(AraHostError::invalid(format!(
                "restore map names an unusable archived modification ID {archived_id:?}"
            )));
        }
        let Some(region) = region_descs.get(clip) else {
            return Err(AraHostError::invalid(format!(
                "restore map targets clip '{}', which is not in the graph",
                clip.as_str()
            )));
        };
        if !explicit_modifications.insert(archived_id) || !explicit_clips.insert(clip) {
            return Err(AraHostError::invalid(
                "restore map names an audio modification twice",
            ));
        }
        if let Some(identity) = identity {
            let Some(archived) = identity.modification(archived_id) else {
                return Err(AraHostError::invalid(format!(
                    "the archive holds no audio modification '{archived_id}'"
                )));
            };
            if archived_source_of.get(&region.source).copied()
                != Some(archived.source_persistent_id.as_str())
            {
                return Err(AraHostError::invalid(format!(
                    "archived modification '{archived_id}' can only be restored together with \
                     its audio source, onto the source of clip '{}'",
                    clip.as_str()
                )));
            }
        }
        listed_modifications.push((
            archived_id.clone(),
            ara_persistent_id(clip.as_str()).into_owned(),
        ));
    }
    for region in regions {
        if explicit_clips.contains(&region.key) {
            continue;
        }
        let id = ara_persistent_id(region.key.as_str());
        if explicit_modifications.contains(id.as_ref()) {
            continue;
        }
        // A modification is restored only with its source.
        let Some(archived_source) = archived_source_of.get(&region.source) else {
            continue;
        };
        let holds = match identity {
            Some(identity) => identity
                .modification(&id)
                .is_some_and(|archived| archived.source_persistent_id == *archived_source),
            None => true,
        };
        if holds {
            let id = id.into_owned();
            listed_modifications.push((id.clone(), id));
        }
    }

    let (unplaced_sources, unplaced_modifications) = match identity {
        Some(identity) => {
            let placed_sources: HashSet<&str> = listed_sources
                .iter()
                .map(|(archived_id, _)| archived_id.as_str())
                .collect();
            let placed_modifications: HashSet<&str> = listed_modifications
                .iter()
                .map(|(archived_id, _)| archived_id.as_str())
                .collect();
            (
                identity
                    .sources
                    .iter()
                    .filter(|source| !placed_sources.contains(source.persistent_id.as_str()))
                    .map(|source| source.persistent_id.clone())
                    .collect(),
                identity
                    .modifications
                    .iter()
                    .filter(|modification| {
                        !placed_modifications.contains(modification.persistent_id.as_str())
                    })
                    .map(|modification| modification.persistent_id.clone())
                    .collect(),
            )
        }
        None => (Vec::new(), Vec::new()),
    };

    let targets = listed_sources
        .iter()
        .map(|(archived_id, key)| PlannedSource {
            key: key.clone(),
            current_id: ara_persistent_id(key.as_str()).into_owned(),
            archived_id: Some(archived_id.clone()),
            as_saved: false,
        })
        .collect();
    let filter = PlannedFilter {
        document_data: true,
        sources: listed_sources
            .into_iter()
            .map(|(archived_id, key)| (archived_id, ara_persistent_id(key.as_str()).into_owned()))
            .collect(),
        modifications: listed_modifications,
    };
    Ok(RestorePlan {
        filter: Some(filter),
        targets,
        unplaced_sources,
        unplaced_modifications,
        expects: identity.map(|identity| !identity.sources.is_empty()),
    })
}

/// Plans a restore of part of an archive: the sources `archived` names (by
/// archived persistent ID) and, for each, every archived modification with a
/// place on it, through a filter whose document data is `document_data`.
///
/// Each named source lands where `map` puts it, or else on the current
/// source published under that very ID; with an identity, only when the
/// archive holds it and the shape matches, and a name the archive does not
/// hold is refused. Without an identity nothing says what the archive holds,
/// so a named source is taken to be archived under its current ID, and so is
/// every clip on it. A named source with no place is reported unplaced, as is
/// (with an identity) an archived modification of it with no place. Nothing
/// outside the scope is listed or reported: it is restored by another call.
/// Map entries for sources outside the scope are ignored; the rest are held
/// to the rules [`plan_restore`] applies.
fn plan_scoped(
    identity: Option<&AraArchiveIdentity>,
    map: Option<&AraRestoreMap>,
    archived: &[String],
    document_data: bool,
    sources: &[AraAudioSourceDesc],
    regions: &[AraPlaybackRegionDesc],
) -> AraResult<RestorePlan> {
    let empty = AraRestoreMap::default();
    let map = map.unwrap_or(&empty);
    let source_descs: HashMap<&AraSourceKey, &AraAudioSourceDesc> =
        sources.iter().map(|desc| (&desc.key, desc)).collect();
    let by_published_id: HashMap<String, &AraAudioSourceDesc> = sources
        .iter()
        .map(|desc| (ara_persistent_id(desc.key.as_str()).into_owned(), desc))
        .collect();
    let region_descs: HashMap<&AraClipKey, &AraPlaybackRegionDesc> =
        regions.iter().map(|desc| (&desc.key, desc)).collect();

    let mut scope: Vec<&str> = Vec::with_capacity(archived.len());
    for archived_id in archived {
        if !usable_id(archived_id) {
            return Err(AraHostError::invalid(format!(
                "restore scope names an unusable archived source ID {archived_id:?}"
            )));
        }
        if identity.is_some_and(|identity| identity.source(archived_id).is_none()) {
            return Err(AraHostError::invalid(format!(
                "the archive holds no audio source '{archived_id}'"
            )));
        }
        if !scope.contains(&archived_id.as_str()) {
            scope.push(archived_id);
        }
    }

    // Mapped sources in scope, checked as `plan_restore` checks them.
    let mut mapped: HashMap<&str, &AraSourceKey> = HashMap::new();
    let mut mapped_targets: HashSet<&AraSourceKey> = HashSet::new();
    for (archived_id, key) in &map.sources {
        if !scope.contains(&archived_id.as_str()) {
            continue;
        }
        let Some(desc) = source_descs.get(key) else {
            return Err(AraHostError::invalid(format!(
                "restore map targets audio source '{}', which is not in the graph",
                key.as_str()
            )));
        };
        if mapped.insert(archived_id, key).is_some() || !mapped_targets.insert(key) {
            return Err(AraHostError::invalid(
                "restore map names an audio source twice",
            ));
        }
        if let Some(archived) = identity.and_then(|identity| identity.source(archived_id)) {
            if !archived.same_shape(desc) {
                return Err(AraHostError::invalid(format!(
                    "archived audio source '{archived_id}' has another length, rate or \
                     channel count than '{}'",
                    key.as_str()
                )));
            }
        }
    }

    // (archived ID, current key, placed through the map) per listed source.
    let mut listed_sources: Vec<(String, AraSourceKey, bool)> = Vec::new();
    let mut unplaced_sources: Vec<String> = Vec::new();
    for archived_id in &scope {
        if let Some(key) = mapped.get(archived_id) {
            listed_sources.push(((*archived_id).to_owned(), (*key).clone(), true));
            continue;
        }
        let place = by_published_id.get(*archived_id).filter(|desc| {
            !mapped_targets.contains(&desc.key)
                && identity
                    .and_then(|identity| identity.source(archived_id))
                    .is_none_or(|archived| archived.same_shape(desc))
        });
        match place {
            Some(desc) => listed_sources.push(((*archived_id).to_owned(), desc.key.clone(), false)),
            None => unplaced_sources.push((*archived_id).to_owned()),
        }
    }
    let archived_source_of: HashMap<&AraSourceKey, &str> = listed_sources
        .iter()
        .map(|(archived_id, key, _)| (key, archived_id.as_str()))
        .collect();

    let mut listed_modifications: Vec<(String, String)> = Vec::new();
    let mut explicit_modifications: HashSet<&str> = HashSet::new();
    let mut explicit_clips: HashSet<&AraClipKey> = HashSet::new();
    for (archived_id, clip) in &map.modifications {
        let archived_source = match identity {
            Some(identity) => {
                let Some(modification) = identity.modification(archived_id) else {
                    return Err(AraHostError::invalid(format!(
                        "the archive holds no audio modification '{archived_id}'"
                    )));
                };
                if !scope.contains(&modification.source_persistent_id.as_str()) {
                    continue;
                }
                Some(modification.source_persistent_id.as_str())
            }
            None => None,
        };
        let Some(region) = region_descs.get(clip) else {
            return Err(AraHostError::invalid(format!(
                "restore map targets clip '{}', which is not in the graph",
                clip.as_str()
            )));
        };
        let listed_source = archived_source_of.get(&region.source).copied();
        match (archived_source, listed_source) {
            // Without an identity, a mapped clip goes with the listed source
            // it sits on, and is not for this call otherwise.
            (None, None) => continue,
            (None, Some(_)) => {}
            (Some(wanted), got) if got == Some(wanted) => {}
            (Some(_), _) => {
                return Err(AraHostError::invalid(format!(
                    "archived modification '{archived_id}' can only be restored together with \
                     its audio source, onto the source of clip '{}'",
                    clip.as_str()
                )));
            }
        }
        if !usable_id(archived_id)
            || !explicit_modifications.insert(archived_id)
            || !explicit_clips.insert(clip)
        {
            return Err(AraHostError::invalid(
                "restore map names an audio modification twice or unusably",
            ));
        }
        listed_modifications.push((
            archived_id.clone(),
            ara_persistent_id(clip.as_str()).into_owned(),
        ));
    }

    let mut unplaced_modifications: Vec<String> = Vec::new();
    match identity {
        Some(identity) => {
            let clips_by_id: HashMap<String, &AraPlaybackRegionDesc> = regions
                .iter()
                .map(|region| (ara_persistent_id(region.key.as_str()).into_owned(), region))
                .collect();
            for modification in &identity.modifications {
                if !scope.contains(&modification.source_persistent_id.as_str())
                    || explicit_modifications.contains(modification.persistent_id.as_str())
                {
                    continue;
                }
                let place = clips_by_id
                    .get(&modification.persistent_id)
                    .filter(|region| {
                        !explicit_clips.contains(&region.key)
                            && archived_source_of.get(&region.source).copied()
                                == Some(modification.source_persistent_id.as_str())
                    });
                match place {
                    Some(_) => listed_modifications.push((
                        modification.persistent_id.clone(),
                        modification.persistent_id.clone(),
                    )),
                    None => unplaced_modifications.push(modification.persistent_id.clone()),
                }
            }
        }
        None => {
            for region in regions {
                if explicit_clips.contains(&region.key)
                    || !archived_source_of.contains_key(&region.source)
                {
                    continue;
                }
                let id = ara_persistent_id(region.key.as_str()).into_owned();
                if explicit_modifications.contains(id.as_str()) {
                    continue;
                }
                listed_modifications.push((id.clone(), id));
            }
        }
    }

    let targets = listed_sources
        .iter()
        .map(|(archived_id, key, through_map)| PlannedSource {
            key: key.clone(),
            current_id: ara_persistent_id(key.as_str()).into_owned(),
            archived_id: Some(archived_id.clone()),
            // By its own ID into the object the identity says it came from.
            as_saved: identity.is_some() && !through_map,
        })
        .collect();
    let filter = PlannedFilter {
        document_data,
        sources: listed_sources
            .into_iter()
            .map(|(archived_id, key, _)| {
                (archived_id, ara_persistent_id(key.as_str()).into_owned())
            })
            .collect(),
        modifications: listed_modifications,
    };
    Ok(RestorePlan {
        filter: Some(filter),
        targets,
        unplaced_sources,
        unplaced_modifications,
        expects: Some(!scope.is_empty()),
    })
}

/// The null-filter plan: restore whatever matches by persistent ID.
fn plan_by_id(
    identity: Option<&AraArchiveIdentity>,
    sources: &[AraAudioSourceDesc],
    regions: &[AraPlaybackRegionDesc],
) -> RestorePlan {
    let current_sources: HashSet<String> = sources
        .iter()
        .map(|desc| ara_persistent_id(desc.key.as_str()).into_owned())
        .collect();
    let current_clips: HashSet<String> = regions
        .iter()
        .map(|desc| ara_persistent_id(desc.key.as_str()).into_owned())
        .collect();
    let targets = sources
        .iter()
        .filter_map(|desc| {
            let id = ara_persistent_id(desc.key.as_str()).into_owned();
            let (archived_id, as_saved) = match identity {
                // A source the archive does not hold is fresh, not missed.
                Some(identity) => {
                    let archived = identity.source(&id)?;
                    (
                        Some(archived.persistent_id.clone()),
                        archived.same_shape(desc),
                    )
                }
                None => (None, false),
            };
            Some(PlannedSource {
                key: desc.key.clone(),
                current_id: id,
                archived_id,
                as_saved,
            })
        })
        .collect();
    let (unplaced_sources, unplaced_modifications) = match identity {
        Some(identity) => (
            identity
                .sources
                .iter()
                .filter(|source| !current_sources.contains(&source.persistent_id))
                .map(|source| source.persistent_id.clone())
                .collect(),
            identity
                .modifications
                .iter()
                .filter(|modification| !current_clips.contains(&modification.persistent_id))
                .map(|modification| modification.persistent_id.clone())
                .collect(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    RestorePlan {
        filter: None,
        targets,
        unplaced_sources,
        unplaced_modifications,
        expects: identity.map(|identity| !identity.sources.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AraPlaybackTransform, AraTrackKey};

    fn source(key: &str, frames: i64) -> AraAudioSourceDesc {
        AraAudioSourceDesc {
            key: key.into(),
            name: key.to_owned(),
            sample_rate: 44_100.0,
            frame_count: frames,
            channel_count: 2,
        }
    }

    fn region(key: &str, source_key: &str) -> AraPlaybackRegionDesc {
        AraPlaybackRegionDesc {
            key: key.into(),
            source: source_key.into(),
            track: AraTrackKey::from("track-1"),
            name: key.to_owned(),
            start_in_modification: 0.0,
            duration_in_modification: 1.0,
            start_in_playback: 0.0,
            duration_in_playback: 1.0,
            transform: AraPlaybackTransform::NONE,
            color: None,
        }
    }

    fn archived_source(key: &str, frames: i64) -> AraArchivedSource {
        AraArchivedSource {
            persistent_id: ara_persistent_id(key).into_owned(),
            key: key.into(),
            sample_rate: 44_100.0,
            frame_count: frames,
            channel_count: 2,
        }
    }

    fn archived_modification(clip: &str, source_key: &str) -> AraArchivedModification {
        AraArchivedModification {
            persistent_id: ara_persistent_id(clip).into_owned(),
            clip: clip.into(),
            source_persistent_id: ara_persistent_id(source_key).into_owned(),
        }
    }

    const ABS: &str = "/Users/doppio/Documents/CodeProject/1.wav";
    const REL: &str = "Assets/Audio/1.wav";
    const FRAMES: i64 = 1_205_470;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(archived, current)| ((*archived).to_owned(), (*current).to_owned()))
            .collect()
    }

    #[test]
    fn grades_decide_each_source() {
        use AraRestoreOutcome::*;
        assert_eq!(classify_source(Some(0), Some(1), false, false), Matched);
        assert_eq!(classify_source(Some(0), Some(3), false, false), Matched);
        assert_eq!(classify_source(Some(0), Some(0), false, false), Missed);
        // Analysis during the restore overrules the grade, never the reverse.
        assert_eq!(classify_source(Some(0), Some(1), true, false), Inconclusive);
        assert_eq!(classify_source(Some(0), Some(0), true, false), Missed);
        // Already analysed before: the grade proves nothing.
        assert_eq!(
            classify_source(Some(1), Some(1), false, false),
            Inconclusive
        );
        assert_eq!(
            classify_source(Some(2), Some(0), false, false),
            Inconclusive
        );
        // No notes, or a grade that could not be read.
        assert_eq!(classify_source(None, Some(1), false, false), Inconclusive);
        assert_eq!(classify_source(Some(0), None, false, false), Inconclusive);
        assert_eq!(
            classify_source(Some(0), Some(-1), false, false),
            Inconclusive
        );
        // A match is a match whether or not the identity placed it.
        assert_eq!(classify_source(Some(0), Some(1), false, true), Matched);
    }

    /// Finding (review): a source archived before its analysis finished comes
    /// back without content even though every ID landed where it was stored
    /// from. That is no evidence of a miss.
    #[test]
    fn a_source_restored_as_saved_that_stays_initial_is_unconfirmed_not_missed() {
        use AraRestoreOutcome::*;
        assert_eq!(classify_source(Some(0), Some(0), false, true), Inconclusive);
        assert_eq!(classify_source(Some(0), Some(0), true, true), Inconclusive);
        // A retry after a build that failed part way: the source was already
        // analysed before the restore, so nothing is judged from its grade.
        assert_eq!(classify_source(Some(1), Some(1), false, true), Inconclusive);
        assert_eq!(
            classify_source(Some(1), Some(1), false, false),
            Inconclusive
        );
    }

    /// Only a by-ID restore of a source the identity records with its current
    /// shape is "as saved"; a mapped one, a legacy one, or one whose audio
    /// changed shape under its ID is not.
    #[test]
    fn only_a_recorded_source_restored_by_its_own_id_is_as_saved() {
        let sources = [source(REL, FRAMES), source("changed.wav", 10)];
        let regions = [region("clip-1", REL), region("clip-2", "changed.wav")];
        let identity = AraArchiveIdentity {
            sources: vec![
                archived_source(REL, FRAMES),
                archived_source("changed.wav", 11),
            ],
            modifications: vec![
                archived_modification("clip-1", REL),
                archived_modification("clip-2", "changed.wav"),
            ],
            keys: None,
        };
        let by_id = plan_restore(
            Some(&identity),
            None,
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap();
        let as_saved: Vec<(&str, bool)> = by_id
            .targets
            .iter()
            .map(|target| (target.key.as_str(), target.as_saved))
            .collect();
        assert_eq!(as_saved, vec![(REL, true), ("changed.wav", false)]);

        let legacy = plan_restore(None, None, AraRestoreScope::Whole, &sources, &regions).unwrap();
        assert!(legacy.targets.iter().all(|target| !target.as_saved));

        let mapped = plan_restore(
            Some(&identity),
            Some(&AraRestoreMap::default()),
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap();
        assert!(!mapped.targets.is_empty());
        assert!(mapped.targets.iter().all(|target| !target.as_saved));
    }

    #[test]
    fn the_whole_restore_is_matched_only_when_everything_landed() {
        use AraRestoreOutcome::*;
        assert_eq!(overall_outcome(&[Matched, Matched], None, false), Matched);
        assert_eq!(overall_outcome(&[Missed, Missed], None, false), Missed);
        assert_eq!(
            overall_outcome(&[Matched, Missed], None, false),
            Inconclusive
        );
        assert_eq!(
            overall_outcome(&[Matched, Inconclusive], None, false),
            Inconclusive
        );
        // Archived state with no place keeps a full match from counting.
        assert_eq!(overall_outcome(&[Matched], Some(true), true), Inconclusive);
        // No targets: the identity decides.
        assert_eq!(overall_outcome(&[], None, false), Inconclusive);
        assert_eq!(overall_outcome(&[], Some(true), true), Missed);
        assert_eq!(overall_outcome(&[], Some(false), false), Matched);
    }

    #[test]
    fn a_report_is_restored_only_when_matched_without_error() {
        let mut report = AraRestoreReport::not_run(AraHostError::invalid("x"));
        assert!(!report.restored());
        report.error = None;
        assert!(!report.restored(), "not_run is a miss");
        report.outcome = AraRestoreOutcome::Matched;
        assert!(report.restored());
    }

    #[test]
    fn without_a_map_the_null_filter_targets_what_the_archive_holds() {
        let sources = [source(REL, FRAMES), source("new.wav", 10)];
        let regions = [region("clip-1", REL), region("clip-2", "new.wav")];
        let identity = AraArchiveIdentity {
            sources: vec![archived_source(REL, FRAMES), archived_source("gone.wav", 5)],
            modifications: vec![
                archived_modification("clip-1", REL),
                archived_modification("clip-9", "gone.wav"),
            ],
            keys: None,
        };

        let plan = plan_restore(
            Some(&identity),
            None,
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap();
        assert_eq!(plan.filter, None);
        assert_eq!(
            plan.targets,
            vec![PlannedSource {
                key: REL.into(),
                current_id: REL.to_owned(),
                archived_id: Some(REL.to_owned()),
                as_saved: true,
            }],
            "a source added since the archive is fresh, not missed"
        );
        assert_eq!(plan.unplaced_sources, vec!["gone.wav".to_owned()]);
        assert_eq!(plan.unplaced_modifications, vec!["clip-9".to_owned()]);

        // A legacy archive: every current source is a target, nothing is
        // known to be unplaced.
        let legacy = plan_restore(None, None, AraRestoreScope::Whole, &sources, &regions).unwrap();
        assert_eq!(legacy.targets.len(), 2);
        assert!(
            legacy
                .targets
                .iter()
                .all(|target| target.archived_id.is_none())
        );
        assert!(legacy.unplaced_sources.is_empty() && legacy.unplaced_modifications.is_empty());
    }

    #[test]
    fn a_legacy_archive_is_mapped_through_its_old_source_id() {
        // The evidence project: the archive holds the absolute drop path, the
        // project now keys the source by its relative copy.
        let sources = [source(REL, FRAMES)];
        let regions = [region("clip-1", REL)];
        let map = AraRestoreMap {
            sources: vec![(ABS.to_owned(), REL.into())],
            modifications: Vec::new(),
        };

        let plan =
            plan_restore(None, Some(&map), AraRestoreScope::Whole, &sources, &regions).unwrap();
        let filter = plan.filter.expect("a map always filters");
        assert_eq!(filter.sources, pairs(&[(ABS, REL)]));
        assert_eq!(
            filter.modifications,
            pairs(&[("clip-1", "clip-1")]),
            "the clip is listed as an identity pair, or ARA skips it"
        );
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].archived_id.as_deref(), Some(ABS));
    }

    #[test]
    fn a_mapped_source_brings_every_archived_modification_that_has_a_place() {
        let sources = [source(REL, FRAMES), source("other.wav", 10)];
        let regions = [
            region("clip-1", REL),
            region("clip-2", REL),
            region("clip-3", "other.wav"),
            // Same ID as an archived clip, on another source: no place.
            region("clip-4", "other.wav"),
        ];
        let identity = AraArchiveIdentity {
            sources: vec![
                archived_source(ABS, FRAMES),
                archived_source("other.wav", 10),
            ],
            modifications: vec![
                archived_modification("clip-1", ABS),
                archived_modification("clip-2", ABS),
                archived_modification("clip-3", "other.wav"),
                archived_modification("clip-4", ABS),
                archived_modification("clip-deleted", ABS),
            ],
            keys: None,
        };
        let map = AraRestoreMap {
            sources: vec![(ABS.to_owned(), REL.into())],
            modifications: Vec::new(),
        };

        let plan = plan_restore(
            Some(&identity),
            Some(&map),
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert_eq!(
            filter.sources,
            pairs(&[(ABS, REL), ("other.wav", "other.wav")]),
            "sources the map does not name are listed as identity pairs"
        );
        assert_eq!(
            filter.modifications,
            pairs(&[
                ("clip-1", "clip-1"),
                ("clip-2", "clip-2"),
                ("clip-3", "clip-3")
            ])
        );
        assert!(plan.unplaced_sources.is_empty());
        assert_eq!(
            plan.unplaced_modifications,
            vec!["clip-4".to_owned(), "clip-deleted".to_owned()]
        );
    }

    #[test]
    fn a_mapped_clip_follows_its_mapped_source() {
        let sources = [source("SRC-B", 10)];
        let regions = [region("MOD-9", "SRC-B")];
        let identity = AraArchiveIdentity {
            sources: vec![archived_source("SRC-A", 10)],
            modifications: vec![archived_modification("MOD-1", "SRC-A")],
            keys: None,
        };
        let map = AraRestoreMap {
            sources: vec![("SRC-A".to_owned(), "SRC-B".into())],
            modifications: vec![("MOD-1".to_owned(), "MOD-9".into())],
        };
        let plan = plan_restore(
            Some(&identity),
            Some(&map),
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert_eq!(filter.sources, pairs(&[("SRC-A", "SRC-B")]));
        assert_eq!(filter.modifications, pairs(&[("MOD-1", "MOD-9")]));
        assert!(plan.unplaced_sources.is_empty() && plan.unplaced_modifications.is_empty());

        // Without its source the modification is refused.
        let lonely = AraRestoreMap {
            sources: Vec::new(),
            modifications: vec![("MOD-1".to_owned(), "MOD-9".into())],
        };
        assert!(
            plan_restore(
                Some(&identity),
                Some(&lonely),
                AraRestoreScope::Whole,
                &sources,
                &regions
            )
            .is_err()
        );
    }

    #[test]
    fn a_mapping_onto_other_audio_is_refused() {
        let sources = [source(REL, FRAMES + 1)];
        let regions = [region("clip-1", REL)];
        let identity = AraArchiveIdentity {
            sources: vec![archived_source(ABS, FRAMES)],
            modifications: vec![archived_modification("clip-1", ABS)],
            keys: None,
        };
        let map = AraRestoreMap {
            sources: vec![(ABS.to_owned(), REL.into())],
            modifications: Vec::new(),
        };
        let error = plan_restore(
            Some(&identity),
            Some(&map),
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap_err();
        assert!(matches!(error, AraHostError::Invalid(_)), "{error:?}");

        // An identity pair whose audio changed shape is left out instead.
        let unmapped = AraRestoreMap::default();
        let identity = AraArchiveIdentity {
            sources: vec![archived_source(REL, FRAMES)],
            modifications: vec![archived_modification("clip-1", REL)],
            keys: None,
        };
        let plan = plan_restore(
            Some(&identity),
            Some(&unmapped),
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert!(filter.sources.is_empty() && filter.modifications.is_empty());
        assert_eq!(plan.unplaced_sources, vec![REL.to_owned()]);
        assert_eq!(plan.unplaced_modifications, vec!["clip-1".to_owned()]);
    }

    #[test]
    fn a_map_must_name_objects_that_exist_once() {
        let sources = [source(REL, FRAMES), source("b.wav", 10)];
        let regions = [region("clip-1", REL)];
        let identity = AraArchiveIdentity {
            sources: vec![archived_source(ABS, FRAMES)],
            ..AraArchiveIdentity::default()
        };
        let refused = |identity: Option<&AraArchiveIdentity>, map: AraRestoreMap| {
            plan_restore(
                identity,
                Some(&map),
                AraRestoreScope::Whole,
                &sources,
                &regions,
            )
            .is_err()
        };
        // A current object that is not in the graph.
        assert!(refused(
            None,
            AraRestoreMap {
                sources: vec![(ABS.to_owned(), "missing.wav".into())],
                ..AraRestoreMap::default()
            }
        ));
        assert!(refused(
            None,
            AraRestoreMap {
                modifications: vec![("clip-1".to_owned(), "clip-missing".into())],
                ..AraRestoreMap::default()
            }
        ));
        // Two archived IDs onto one source, and one archived ID twice.
        assert!(refused(
            None,
            AraRestoreMap {
                sources: vec![(ABS.to_owned(), REL.into()), ("x".to_owned(), REL.into())],
                ..AraRestoreMap::default()
            }
        ));
        assert!(refused(
            None,
            AraRestoreMap {
                sources: vec![
                    (ABS.to_owned(), REL.into()),
                    (ABS.to_owned(), "b.wav".into())
                ],
                ..AraRestoreMap::default()
            }
        ));
        // An archived ID the archive does not hold.
        assert!(refused(
            Some(&identity),
            AraRestoreMap {
                sources: vec![("elsewhere.wav".to_owned(), REL.into())],
                ..AraRestoreMap::default()
            }
        ));
        // An archived ID ARA could never have stored.
        assert!(refused(
            None,
            AraRestoreMap {
                sources: vec![("\u{e42}.wav".to_owned(), REL.into())],
                ..AraRestoreMap::default()
            }
        ));
    }

    /// Partial restore, first part: a document holding two sources opens with
    /// one of them offline. The present one is restored with the document
    /// data; the offline one is neither listed nor reported missing. Second
    /// part, once it is back: only it, without the document data, and the
    /// first one is not listed again.
    #[test]
    fn a_scoped_restore_lists_only_its_sources_and_their_modifications() {
        let identity = AraArchiveIdentity {
            sources: vec![archived_source(REL, FRAMES), archived_source("b.wav", 10)],
            modifications: vec![
                archived_modification("clip-1", REL),
                archived_modification("clip-2", "b.wav"),
            ],
            keys: None,
        };
        let present = [source(REL, FRAMES)];
        let present_regions = [region("clip-1", REL)];
        let first = [REL.to_owned()];
        let plan = plan_restore(
            Some(&identity),
            None,
            AraRestoreScope::Sources {
                archived: &first,
                document_data: true,
            },
            &present,
            &present_regions,
        )
        .unwrap();
        let filter = plan.filter.expect("a scope always filters");
        assert!(filter.document_data);
        assert_eq!(filter.sources, pairs(&[(REL, REL)]));
        assert_eq!(filter.modifications, pairs(&[("clip-1", "clip-1")]));
        assert!(plan.unplaced_sources.is_empty() && plan.unplaced_modifications.is_empty());
        assert_eq!(plan.expects, Some(true));
        assert_eq!(plan.targets.len(), 1);
        assert!(plan.targets[0].as_saved, "by its own ID, recorded");

        let whole = [source(REL, FRAMES), source("b.wav", 10)];
        let whole_regions = [region("clip-1", REL), region("clip-2", "b.wav")];
        let second = ["b.wav".to_owned()];
        let plan = plan_restore(
            Some(&identity),
            None,
            AraRestoreScope::Sources {
                archived: &second,
                document_data: false,
            },
            &whole,
            &whole_regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert!(!filter.document_data);
        assert_eq!(filter.sources, pairs(&[("b.wav", "b.wav")]));
        assert_eq!(filter.modifications, pairs(&[("clip-2", "clip-2")]));
        assert_eq!(
            plan.targets
                .iter()
                .map(|target| target.key.as_str())
                .collect::<Vec<_>>(),
            vec!["b.wav"]
        );
    }

    /// Without an identity, a named source is taken to be archived under its
    /// current ID, and so is every clip on it.
    #[test]
    fn a_legacy_scope_takes_its_sources_and_clips_by_their_current_ids() {
        let sources = [source(REL, FRAMES), source("b.wav", 10)];
        let regions = [
            region("clip-1", REL),
            region("clip-2", "b.wav"),
            region("clip-3", "b.wav"),
        ];
        let scope = ["b.wav".to_owned()];
        let plan = plan_restore(
            None,
            None,
            AraRestoreScope::Sources {
                archived: &scope,
                document_data: false,
            },
            &sources,
            &regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert_eq!(filter.sources, pairs(&[("b.wav", "b.wav")]));
        assert_eq!(
            filter.modifications,
            pairs(&[("clip-2", "clip-2"), ("clip-3", "clip-3")])
        );
        assert!(!plan.targets[0].as_saved, "nothing recorded it");
        assert_eq!(plan.expects, Some(true));
    }

    #[test]
    fn a_scoped_source_with_no_place_is_unplaced_and_an_unknown_one_refused() {
        let identity = AraArchiveIdentity {
            sources: vec![archived_source(REL, FRAMES), archived_source("b.wav", 10)],
            modifications: vec![
                archived_modification("clip-1", REL),
                archived_modification("clip-2", "b.wav"),
            ],
            keys: None,
        };
        let sources = [source(REL, FRAMES), source("b.wav", 11)];
        let regions = [region("clip-1", REL), region("clip-2", "b.wav")];
        let plan_for = |names: &[String], identity: Option<&AraArchiveIdentity>| {
            plan_restore(
                identity,
                None,
                AraRestoreScope::Sources {
                    archived: names,
                    document_data: false,
                },
                &sources,
                &regions,
            )
        };
        // Its audio changed shape: no place, and neither has its clip.
        let plan = plan_for(&["b.wav".to_owned()], Some(&identity)).unwrap();
        assert!(plan.targets.is_empty());
        assert_eq!(plan.unplaced_sources, vec!["b.wav".to_owned()]);
        assert_eq!(plan.unplaced_modifications, vec!["clip-2".to_owned()]);
        assert_eq!(
            overall_outcome(&[], plan.expects, true),
            AraRestoreOutcome::Missed
        );
        // Not in the graph at all, legacy: unplaced too.
        let plan = plan_for(&["gone.wav".to_owned()], None).unwrap();
        assert_eq!(plan.unplaced_sources, vec!["gone.wav".to_owned()]);
        // A source the archive does not hold is the caller's mistake.
        assert!(plan_for(&["gone.wav".to_owned()], Some(&identity)).is_err());
        assert!(plan_for(&["\u{e42}.wav".to_owned()], None).is_err());
        // Nothing named: only the document data, with nothing to miss.
        let plan = plan_restore(
            Some(&identity),
            None,
            AraRestoreScope::Sources {
                archived: &[],
                document_data: true,
            },
            &sources,
            &regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert!(filter.document_data && filter.sources.is_empty());
        assert!(filter.modifications.is_empty());
        assert_eq!(
            overall_outcome(&[], plan.expects, false),
            AraRestoreOutcome::Matched
        );
    }

    /// A map still places a moved source in scope; its entries for sources
    /// outside the scope are left for the call that restores them.
    #[test]
    fn a_scoped_restore_follows_the_map_for_its_own_sources_only() {
        let identity = AraArchiveIdentity {
            sources: vec![
                archived_source(ABS, FRAMES),
                archived_source("b-old.wav", 10),
            ],
            modifications: vec![
                archived_modification("clip-1", ABS),
                archived_modification("clip-2", "b-old.wav"),
            ],
            keys: None,
        };
        let sources = [source(REL, FRAMES), source("b.wav", 10)];
        let regions = [region("clip-1", REL), region("clip-2", "b.wav")];
        let map = AraRestoreMap {
            sources: vec![
                (ABS.to_owned(), REL.into()),
                ("b-old.wav".to_owned(), "b.wav".into()),
            ],
            modifications: Vec::new(),
        };
        let scope = [ABS.to_owned()];
        let plan = plan_restore(
            Some(&identity),
            Some(&map),
            AraRestoreScope::Sources {
                archived: &scope,
                document_data: true,
            },
            &sources,
            &regions,
        )
        .unwrap();
        let filter = plan.filter.unwrap();
        assert_eq!(filter.sources, pairs(&[(ABS, REL)]));
        assert_eq!(filter.modifications, pairs(&[("clip-1", "clip-1")]));
        assert!(!plan.targets[0].as_saved, "mapped, not in place");
        assert!(plan.unplaced_sources.is_empty() && plan.unplaced_modifications.is_empty());
    }

    #[test]
    fn identity_pairs_never_reuse_an_id_the_map_moved() {
        // The map moves archived "b.wav" onto "a.wav"; the current "b.wav"
        // must not claim archived "b.wav" as well, and archived "a.wav" is
        // superseded.
        let sources = [source("a.wav", 10), source("b.wav", 10)];
        let regions = [region("clip-1", "a.wav"), region("clip-2", "b.wav")];
        let map = AraRestoreMap {
            sources: vec![("b.wav".to_owned(), "a.wav".into())],
            modifications: Vec::new(),
        };
        let filter = plan_restore(None, Some(&map), AraRestoreScope::Whole, &sources, &regions)
            .unwrap()
            .filter
            .unwrap();
        assert_eq!(filter.sources, pairs(&[("b.wav", "a.wav")]));
        assert_eq!(filter.modifications, pairs(&[("clip-1", "clip-1")]));
    }

    #[test]
    fn non_ascii_keys_are_planned_under_their_encoded_ids() {
        let thai = "/p/\u{e42}\u{e1b}\u{e23}/1.wav";
        let sources = [source(thai, 10)];
        let regions = [region("clip-1", thai)];
        let filter = plan_restore(
            None,
            Some(&AraRestoreMap::default()),
            AraRestoreScope::Whole,
            &sources,
            &regions,
        )
        .unwrap()
        .filter
        .unwrap();
        let id = ara_persistent_id(thai).into_owned();
        assert!(id.is_ascii());
        assert_eq!(filter.sources, vec![(id.clone(), id)]);
    }

    fn key(root_fifths: i32, name: Option<&str>, quarter_position: f64) -> AraKeySignature {
        let mut intervals = [false; 12];
        for interval in [0, 2, 4, 5, 7, 9, 11] {
            intervals[interval] = true;
        }
        AraKeySignature {
            root_fifths,
            intervals,
            name: name.map(str::to_owned),
            quarter_position,
        }
    }

    #[test]
    fn the_key_descriptor_tells_keys_apart() {
        let c = key(0, Some("C Major"), 0.0);
        let base = AraKeyDescriptor::of(std::slice::from_ref(&c));
        assert_eq!(
            base,
            AraKeyDescriptor::of(&[key(0, Some("C Major"), 0.0)]),
            "deterministic"
        );
        assert_eq!(
            base,
            AraKeyDescriptor::of(&[key(0, Some("C Major"), -0.0)]),
            "-0 is 0"
        );

        let mut minor = c.clone();
        minor.intervals[4] = false;
        minor.intervals[3] = true;
        let variants = [
            AraKeyDescriptor::of(&[]),
            AraKeyDescriptor::of(&[key(1, Some("C Major"), 0.0)]),
            AraKeyDescriptor::of(&[minor]),
            AraKeyDescriptor::of(&[key(0, None, 0.0)]),
            AraKeyDescriptor::of(&[key(0, Some("C Major"), 4.0)]),
            AraKeyDescriptor::of(&[c, key(1, None, 8.0)]),
        ];
        let mut seen = HashSet::from([base]);
        for variant in variants {
            assert!(seen.insert(variant), "{variant:?} collides");
        }
    }

    #[test]
    fn the_key_descriptor_is_stable_across_builds() {
        // Persisted in projects: a change here must be a deliberate format
        // decision, not a side effect.
        // Values cross-checked against an independent FNV-1a implementation.
        assert_eq!(AraKeyDescriptor::of(&[]).0, 0x529a_2cdc_8ff5_33ac);
        assert_eq!(
            AraKeyDescriptor::of(&[key(3, Some("A Natural Minor"), 0.0)]).0,
            0xc311_7249_f80c_81da
        );
    }

    #[test]
    fn harmony_is_resynced_only_for_a_known_changed_key() {
        let a = AraKeyDescriptor::of(&[key(3, None, 0.0)]);
        let c = AraKeyDescriptor::of(&[key(0, None, 0.0)]);
        assert!(needs_harmony_resync(Some(a), c, true));
        assert!(!needs_harmony_resync(Some(c), c, true), "unchanged key");
        assert!(!needs_harmony_resync(None, c, true), "legacy: unknown key");
        assert!(
            !needs_harmony_resync(Some(a), c, false),
            "no harmonic content"
        );
    }
}
