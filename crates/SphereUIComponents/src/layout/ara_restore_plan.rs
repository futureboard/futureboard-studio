//! Where a saved ARA document lands in the project as it is now.
//!
//! A plug-in restores archived state by persistent ID and reports success
//! when nothing matched, so the host has to decide on its own terms, before
//! the plug-in is touched, whether the archive can land and where. This reads
//! the identity recorded with the archive ([`ProjectAraIdentity`]) against the
//! graph the session is about to build. Pure: no plug-in, no GPUI, no I/O.
//!
//! Also the one place the project's identity record and the host's
//! ([`AraArchiveIdentity`]) are converted into each other.

use std::collections::{HashMap, HashSet};

use sphere_ara_host::{
    ara_persistent_id, AraArchiveIdentity, AraArchivedModification, AraArchivedSource,
    AraAudioSourceDesc, AraClipKey, AraGraph, AraKeyDescriptor, AraRestoreMap, AraSourceKey,
};

use crate::project::{
    ProjectAraArchivedModification, ProjectAraArchivedSource, ProjectAraIdentity,
};

/// How one saved document is restored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AraRestorePlan {
    /// Every archived source the graph can take is there under the ID it was
    /// saved with: restore by ID, and judge the result against the identity.
    AsSaved,
    /// Some archived sources are in the graph under another ID, holding the
    /// same audio: restore through this map. Objects it does not name are
    /// restored by ID.
    Mapped(AraRestoreMap),
    /// No archived source has a place in the graph (moved to other audio,
    /// or deleted): nothing can land, so the plug-in is not asked, and the
    /// archive is kept as it is, as an orphan. Audio that is offline is kept
    /// back for later instead (see [`restore_timing`]).
    NothingMatches,
    /// No identity was recorded (a document saved before it was): restore by
    /// ID and let the plug-in's verdict decide.
    Legacy,
}

impl AraRestorePlan {
    /// Short name for the restore log line.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::AsSaved => "as saved",
            Self::Mapped(_) => "mapped",
            Self::NothingMatches => "nothing matches",
            Self::Legacy => "legacy",
        }
    }
}

/// Whether an archived source describes audio of `desc`'s shape.
fn same_shape(archived: &ProjectAraArchivedSource, desc: &AraAudioSourceDesc) -> bool {
    archived.sample_rate == desc.sample_rate
        && archived.frames == desc.frame_count
        && archived.channels == desc.channel_count
}

/// Decides how the document `identity` describes is restored into `graph`.
///
/// `fingerprint_of` answers the content fingerprint (as
/// [`crate::project::io::content_fingerprint`] spells it) of a current
/// source by its key, when the project's asset records know it.
///
/// Placement of each archived source:
///
/// 1. A current source published under its persistent ID takes it when the
///    shape matches. One whose shape changed holds other audio: it is left
///    out, which needs a filter.
/// 2. Otherwise the source moved. Its place is the current source of the
///    clips its archived modifications belonged to (all of them agreeing), or
///    else the one current source whose content fingerprint equals the
///    archived one. Either way it is mapped only when the content is known on
///    both sides and equal and the shape matches, and only onto a source the
///    archive does not hold under its own ID and no other archived source has
///    taken.
///
/// Nothing placed at all is [`AraRestorePlan::NothingMatches`] (an archive
/// that holds no source has nothing to miss and restores as saved). Anything
/// that moved or was left out needs [`AraRestorePlan::Mapped`]; otherwise
/// [`AraRestorePlan::AsSaved`]. Clips are matched by ID only: a clip that got
/// a new ID (a split, say) has new edits of its own.
pub(crate) fn plan_restore(
    identity: Option<&ProjectAraIdentity>,
    graph: &AraGraph,
    fingerprint_of: &dyn Fn(&AraSourceKey) -> Option<String>,
) -> AraRestorePlan {
    let Some(identity) = identity else {
        return AraRestorePlan::Legacy;
    };
    if identity.sources.is_empty() {
        return AraRestorePlan::AsSaved;
    }

    let current: HashMap<String, &AraAudioSourceDesc> = graph
        .sources
        .iter()
        .map(|desc| (ara_persistent_id(desc.key.as_str()).into_owned(), desc))
        .collect();
    let archived_ids: HashSet<&str> = identity
        .sources
        .iter()
        .map(|source| source.persistent_id.as_str())
        .collect();
    let source_of_clip: HashMap<&str, &AraSourceKey> = graph
        .regions
        .iter()
        .map(|region| (region.key.as_str(), &region.source))
        .collect();

    let mut placed = 0usize;
    let mut needs_filter = false;
    // Current sources already spoken for: by their own archived state, or by
    // a mapping.
    let mut taken: HashSet<&AraSourceKey> = HashSet::new();
    let mut moved: Vec<&ProjectAraArchivedSource> = Vec::new();
    for archived in &identity.sources {
        match current.get(&archived.persistent_id) {
            Some(desc) => {
                taken.insert(&desc.key);
                if same_shape(archived, desc) {
                    placed += 1;
                } else {
                    needs_filter = true;
                }
            }
            None => moved.push(archived),
        }
    }

    let mut map = AraRestoreMap::default();
    for archived in moved {
        let via_clips: HashSet<&AraSourceKey> = identity
            .modifications
            .iter()
            .filter(|modification| modification.source_persistent_id == archived.persistent_id)
            .filter_map(|modification| source_of_clip.get(modification.clip_id.as_str()).copied())
            .collect();
        let known = archived.fingerprint.as_deref();
        let candidate = if via_clips.len() == 1 {
            via_clips.into_iter().next()
        } else if via_clips.is_empty() && known.is_some() {
            let mut same_content = graph.sources.iter().filter(|desc| {
                !taken.contains(&desc.key)
                    && same_shape(archived, desc)
                    && fingerprint_of(&desc.key).as_deref() == known
            });
            match (same_content.next(), same_content.next()) {
                (Some(only), None) => Some(&only.key),
                _ => None,
            }
        } else {
            None
        };
        let Some(key) = candidate else {
            continue;
        };
        let Some(desc) = graph.sources.iter().find(|desc| &desc.key == key) else {
            continue;
        };
        let holds_same_audio = known.is_some()
            && fingerprint_of(key).as_deref() == known
            && same_shape(archived, desc);
        let free = !taken.contains(key)
            && !archived_ids.contains(ara_persistent_id(key.as_str()).as_ref());
        if holds_same_audio && free {
            taken.insert(key);
            placed += 1;
            needs_filter = true;
            map.sources
                .push((archived.persistent_id.clone(), key.clone()));
        }
    }

    if placed == 0 {
        AraRestorePlan::NothingMatches
    } else if needs_filter {
        AraRestorePlan::Mapped(map)
    } else {
        AraRestorePlan::AsSaved
    }
}

/// How a parked document is restored by one sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestoreTiming {
    /// Restore it whole now.
    Now,
    /// Some of the audio it holds is offline. Restore everything else now,
    /// with the plug-in's document data, and keep the archive for the
    /// sources `deferred` names (archived persistent IDs), to be restored on
    /// their own once their audio is back (ARA partial persistency: see
    /// `sphere_ara_host::AraRestoreScope`).
    Partly { deferred: Vec<String> },
    /// Nothing of it can land in this graph, which holds no audio at all:
    /// keep it parked, whole, and try again on the next sync that builds
    /// audio. `offline`: some of the track's media is offline, which is what
    /// the user is told.
    AwaitMedia { offline: bool },
}

/// Decides how the document `identity` describes is restored into `graph`
/// by this sync. `offline` lists the session's clips that were left out of
/// the graph because their media could not be probed, with their sources.
///
/// A restore lands only what has a place in the graph, and a plug-in accepts
/// it anyway, so whatever of the archive is offline would be judged missing,
/// and the next store would no longer hold it. So the offline part is kept
/// back ([`RestoreTiming::Partly`], see [`offline_sources`]) and the rest is
/// restored now: work on the audio that is there is then saved with the live
/// document from the start, and the offline part is restored into that same
/// document when its audio comes back, without touching what is there.
///
/// The document waits whole only when the graph holds no audio at all (and
/// the document may hold some: an empty recorded document has nothing to
/// wait for); then the live document cannot hold any work to lose. Recorded
/// audio that is gone for good (its clips deleted) or replaced (its clips on
/// other audio) is not kept back: that is a miss, and the restore says so.
pub(crate) fn restore_timing(
    identity: Option<&ProjectAraIdentity>,
    graph: &AraGraph,
    offline: &[(AraClipKey, AraSourceKey)],
) -> RestoreTiming {
    if identity.is_some_and(|identity| identity.sources.is_empty()) {
        return RestoreTiming::Now;
    }
    if graph.sources.is_empty() {
        return RestoreTiming::AwaitMedia {
            offline: !offline.is_empty(),
        };
    }
    let deferred = offline_sources(identity, graph, offline);
    if deferred.is_empty() {
        RestoreTiming::Now
    } else {
        RestoreTiming::Partly { deferred }
    }
}

/// The archived sources of the document `identity` describes whose audio is
/// offline, by archived persistent ID, in record order.
///
/// Recorded: each archived source not in the graph under its own ID whose ID
/// is an offline source's, or one of whose clips is offline (a source that
/// was renamed, say). Legacy (no record): nothing says what the archive
/// holds, so each offline source is taken to be archived under the ID it is
/// published with.
pub(crate) fn offline_sources(
    identity: Option<&ProjectAraIdentity>,
    graph: &AraGraph,
    offline: &[(AraClipKey, AraSourceKey)],
) -> Vec<String> {
    let offline_ids: Vec<String> = offline
        .iter()
        .map(|(_, source)| ara_persistent_id(source.as_str()).into_owned())
        .collect();
    let Some(identity) = identity else {
        let mut ids: Vec<String> = Vec::new();
        for id in offline_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        return ids;
    };
    let present: HashSet<String> = graph
        .sources
        .iter()
        .map(|desc| ara_persistent_id(desc.key.as_str()).into_owned())
        .collect();
    identity
        .sources
        .iter()
        .filter(|archived| !present.contains(&archived.persistent_id))
        .filter(|archived| {
            offline_ids.contains(&archived.persistent_id)
                || identity.modifications.iter().any(|modification| {
                    modification.source_persistent_id == archived.persistent_id
                        && offline
                            .iter()
                            .any(|(clip, _)| clip.as_str() == modification.clip_id)
                })
        })
        .map(|archived| archived.persistent_id.clone())
        .collect()
}

/// Every archived source of the document `identity` describes except
/// `deferred`: what the first part of a partial restore brings back.
pub(crate) fn sources_except(identity: &ProjectAraIdentity, deferred: &[String]) -> Vec<String> {
    identity
        .sources
        .iter()
        .map(|archived| archived.persistent_id.clone())
        .filter(|id| !deferred.contains(id))
        .collect()
}

/// Which of the sources an archive kept back (`remaining`, archived
/// persistent IDs) have a place in `graph` now, and the map that places the
/// ones that moved: what a later part of a partial restore brings back.
///
/// Recorded: a source in the graph under its own ID with its recorded shape,
/// or one [`plan_restore`] maps onto a source holding the same audio.
/// Legacy: a source in the graph under the ID it is taken to be archived
/// under (see [`offline_sources`]).
pub(crate) fn returning_sources(
    identity: Option<&ProjectAraIdentity>,
    remaining: &[String],
    graph: &AraGraph,
    fingerprint_of: &dyn Fn(&AraSourceKey) -> Option<String>,
) -> (Vec<String>, AraRestoreMap) {
    let current: HashMap<String, &AraAudioSourceDesc> = graph
        .sources
        .iter()
        .map(|desc| (ara_persistent_id(desc.key.as_str()).into_owned(), desc))
        .collect();
    let Some(identity) = identity else {
        let due = remaining
            .iter()
            .filter(|id| current.contains_key(id.as_str()))
            .cloned()
            .collect();
        return (due, AraRestoreMap::default());
    };
    let moved = match plan_restore(Some(identity), graph, fingerprint_of) {
        AraRestorePlan::Mapped(map) => map,
        _ => AraRestoreMap::default(),
    };
    let mut map = AraRestoreMap::default();
    let mut due = Vec::new();
    for id in remaining {
        let Some(archived) = identity
            .sources
            .iter()
            .find(|source| &source.persistent_id == id)
        else {
            continue;
        };
        if let Some((_, key)) = moved
            .sources
            .iter()
            .find(|(archived_id, _)| archived_id == id)
        {
            map.sources.push((id.clone(), key.clone()));
            due.push(id.clone());
        } else if current
            .get(id.as_str())
            .is_some_and(|desc| same_shape(archived, desc))
        {
            due.push(id.clone());
        }
    }
    (due, map)
}

/// Whether everything `identity` records (only the sources `part` names, and
/// their modifications, when given) is in `graph` exactly where it was
/// stored from: each archived source under its own persistent ID with the
/// same shape, and each archived modification's clip under its own ID on
/// that same source.
///
/// A restore by ID into such a graph puts every archived object back into
/// the object it was stored from, so a verdict with no evidence either way
/// (a plug-in without a note grade, an archive stored before its analysis
/// finished, or a source already analysed when its part came back) is
/// trusted as restored.
pub(crate) fn restores_in_place(
    identity: &ProjectAraIdentity,
    graph: &AraGraph,
    part: Option<&[String]>,
) -> bool {
    let in_part = |id: &str| part.is_none_or(|part| part.iter().any(|named| named == id));
    let sources: HashMap<String, &AraAudioSourceDesc> = graph
        .sources
        .iter()
        .map(|desc| (ara_persistent_id(desc.key.as_str()).into_owned(), desc))
        .collect();
    let clips: HashMap<String, String> = graph
        .regions
        .iter()
        .map(|region| {
            (
                ara_persistent_id(region.key.as_str()).into_owned(),
                ara_persistent_id(region.source.as_str()).into_owned(),
            )
        })
        .collect();
    identity
        .sources
        .iter()
        .filter(|archived| in_part(&archived.persistent_id))
        .all(|archived| {
            sources
                .get(&archived.persistent_id)
                .is_some_and(|desc| same_shape(archived, desc))
        })
        && identity
            .modifications
            .iter()
            .filter(|modification| in_part(&modification.source_persistent_id))
            .all(|modification| {
                clips.get(&modification.persistent_id) == Some(&modification.source_persistent_id)
            })
}

/// The host's form of a recorded identity.
pub(crate) fn host_identity(identity: &ProjectAraIdentity) -> AraArchiveIdentity {
    AraArchiveIdentity {
        sources: identity
            .sources
            .iter()
            .map(|source| AraArchivedSource {
                persistent_id: source.persistent_id.clone(),
                key: AraSourceKey(source.asset_id.clone()),
                sample_rate: source.sample_rate,
                frame_count: source.frames,
                channel_count: source.channels,
            })
            .collect(),
        modifications: identity
            .modifications
            .iter()
            .map(|modification| AraArchivedModification {
                persistent_id: modification.persistent_id.clone(),
                clip: AraClipKey(modification.clip_id.clone()),
                source_persistent_id: modification.source_persistent_id.clone(),
            })
            .collect(),
        keys: identity.key_descriptor.map(AraKeyDescriptor),
    }
}

/// The project's form of the identity a store just returned. Content
/// fingerprints are filled in by the save, which is the one place that knows
/// the asset records (see `project::io`).
pub(crate) fn project_identity(identity: &AraArchiveIdentity) -> ProjectAraIdentity {
    ProjectAraIdentity {
        sources: identity
            .sources
            .iter()
            .map(|source| ProjectAraArchivedSource {
                persistent_id: source.persistent_id.clone(),
                asset_id: source.key.as_str().to_owned(),
                sample_rate: source.sample_rate,
                frames: source.frame_count,
                channels: source.channel_count,
                fingerprint: None,
            })
            .collect(),
        modifications: identity
            .modifications
            .iter()
            .map(|modification| ProjectAraArchivedModification {
                persistent_id: modification.persistent_id.clone(),
                clip_id: modification.clip.as_str().to_owned(),
                source_persistent_id: modification.source_persistent_id.clone(),
            })
            .collect(),
        key_descriptor: identity.keys.map(|keys| keys.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_ara_host::{AraPlaybackRegionDesc, AraPlaybackTransform, AraTrackKey};

    const ABS: &str = "/Users/doppio/Documents/CodeProject/1.wav";
    const REL: &str = "Assets/Audio/1.wav";
    const FRAMES: i64 = 1_205_470;
    const FP: &str = "498ba4-9e3e795e";

    fn desc(key: &str, frames: i64) -> AraAudioSourceDesc {
        AraAudioSourceDesc {
            key: AraSourceKey::from(key),
            name: key.to_owned(),
            sample_rate: 44_100.0,
            frame_count: frames,
            channel_count: 2,
        }
    }

    fn region(clip: &str, source: &str) -> AraPlaybackRegionDesc {
        AraPlaybackRegionDesc {
            key: AraClipKey::from(clip),
            source: AraSourceKey::from(source),
            track: AraTrackKey::from("track-1"),
            name: clip.to_owned(),
            start_in_modification: 0.0,
            duration_in_modification: 1.0,
            start_in_playback: 0.0,
            duration_in_playback: 1.0,
            transform: AraPlaybackTransform::NONE,
            color: None,
        }
    }

    fn graph(sources: &[(&str, i64)], regions: &[(&str, &str)]) -> AraGraph {
        AraGraph {
            name: Some("Vocals".to_owned()),
            sources: sources
                .iter()
                .map(|(key, frames)| desc(key, *frames))
                .collect(),
            sequences: Vec::new(),
            regions: regions
                .iter()
                .map(|(clip, source)| region(clip, source))
                .collect(),
        }
    }

    fn archived(
        asset_id: &str,
        frames: i64,
        fingerprint: Option<&str>,
    ) -> ProjectAraArchivedSource {
        ProjectAraArchivedSource {
            persistent_id: ara_persistent_id(asset_id).into_owned(),
            asset_id: asset_id.to_owned(),
            sample_rate: 44_100.0,
            frames,
            channels: 2,
            fingerprint: fingerprint.map(str::to_owned),
        }
    }

    fn modification(clip: &str, asset_id: &str) -> ProjectAraArchivedModification {
        ProjectAraArchivedModification {
            persistent_id: ara_persistent_id(clip).into_owned(),
            clip_id: clip.to_owned(),
            source_persistent_id: ara_persistent_id(asset_id).into_owned(),
        }
    }

    fn identity(
        sources: Vec<ProjectAraArchivedSource>,
        modifications: Vec<ProjectAraArchivedModification>,
    ) -> ProjectAraIdentity {
        ProjectAraIdentity {
            sources,
            modifications,
            key_descriptor: Some(7),
        }
    }

    fn fingerprints(known: &[(&str, &str)]) -> impl Fn(&AraSourceKey) -> Option<String> {
        let known: HashMap<String, String> = known
            .iter()
            .map(|(key, fp)| ((*key).to_owned(), (*fp).to_owned()))
            .collect();
        move |key: &AraSourceKey| known.get(key.as_str()).cloned()
    }

    #[test]
    fn no_record_is_a_legacy_restore() {
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(None, &graph, &fingerprints(&[])),
            AraRestorePlan::Legacy
        );
    }

    #[test]
    fn unchanged_ids_restore_as_saved() {
        let identity = identity(
            vec![archived(REL, FRAMES, Some(FP))],
            vec![modification("clip-1", REL)],
        );
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[])),
            AraRestorePlan::AsSaved,
            "matching IDs need no content check and no filter"
        );
    }

    /// The evidence project: the archive holds the absolute drop path, the
    /// project now reads the copy under 'Assets/Audio/1.wav', clip-1 is the
    /// same clip, and the copy holds the same bytes.
    #[test]
    fn a_source_renamed_after_archiving_is_mapped_through_its_clip() {
        let identity = identity(
            vec![archived(ABS, FRAMES, Some(FP))],
            vec![modification("clip-1", ABS)],
        );
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[(REL, FP)])),
            AraRestorePlan::Mapped(AraRestoreMap {
                sources: vec![(ABS.to_owned(), AraSourceKey::from(REL))],
                modifications: Vec::new(),
            })
        );
    }

    #[test]
    fn a_renamed_source_with_other_content_is_not_mapped() {
        let identity = identity(
            vec![archived(ABS, FRAMES, Some(FP))],
            vec![modification("clip-1", ABS)],
        );
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        // A render of the same length: other bytes.
        assert_eq!(
            plan_restore(
                Some(&identity),
                &graph,
                &fingerprints(&[(REL, "498ba4-00000000")])
            ),
            AraRestorePlan::NothingMatches
        );
        // Content unknown on either side proves nothing.
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[])),
            AraRestorePlan::NothingMatches
        );
        let unknown = super::tests::identity(
            vec![archived(ABS, FRAMES, None)],
            vec![modification("clip-1", ABS)],
        );
        assert_eq!(
            plan_restore(Some(&unknown), &graph, &fingerprints(&[(REL, FP)])),
            AraRestorePlan::NothingMatches
        );
    }

    #[test]
    fn a_renamed_source_of_another_length_is_not_mapped() {
        let identity = identity(
            vec![archived(ABS, FRAMES, Some(FP))],
            vec![modification("clip-1", ABS)],
        );
        let graph = graph(&[(REL, FRAMES + 1)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[(REL, FP)])),
            AraRestorePlan::NothingMatches
        );
    }

    /// Clip IDs changed too (a re-drop, say): the one current source holding
    /// the same audio is still found by its content.
    #[test]
    fn a_renamed_source_without_its_clip_is_found_by_its_unique_content() {
        let identity = identity(
            vec![archived(ABS, FRAMES, Some(FP))],
            vec![modification("clip-1", ABS)],
        );
        let graph = graph(
            &[(REL, FRAMES), ("Assets/Audio/2.wav", FRAMES)],
            &[("clip-7", REL), ("clip-8", "Assets/Audio/2.wav")],
        );
        let known = fingerprints(&[(REL, FP), ("Assets/Audio/2.wav", "11-22222222")]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &known),
            AraRestorePlan::Mapped(AraRestoreMap {
                sources: vec![(ABS.to_owned(), AraSourceKey::from(REL))],
                modifications: Vec::new(),
            })
        );
        // Two copies of the same audio: which one is not for the host to guess.
        let both = fingerprints(&[(REL, FP), ("Assets/Audio/2.wav", FP)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &both),
            AraRestorePlan::NothingMatches
        );
    }

    #[test]
    fn media_that_is_offline_leaves_nothing_to_restore() {
        let identity = identity(
            vec![archived(REL, FRAMES, Some(FP))],
            vec![modification("clip-1", REL)],
        );
        let empty = graph(&[], &[]);
        assert_eq!(
            plan_restore(Some(&identity), &empty, &fingerprints(&[])),
            AraRestorePlan::NothingMatches
        );
    }

    /// One source in place and one offline: restore what can land, and the
    /// host reports the rest unplaced.
    #[test]
    fn a_partly_offline_archive_restores_what_is_there_as_saved() {
        let identity = identity(
            vec![
                archived(REL, FRAMES, Some(FP)),
                archived("Assets/Audio/2.wav", 10, None),
            ],
            vec![
                modification("clip-1", REL),
                modification("clip-2", "Assets/Audio/2.wav"),
            ],
        );
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[])),
            AraRestorePlan::AsSaved
        );
    }

    /// The same ID on audio of another shape (the file was replaced in
    /// place): a filter has to leave it out.
    #[test]
    fn a_source_whose_audio_changed_shape_under_its_id_is_left_out() {
        let identity = identity(
            vec![archived(REL, FRAMES, Some(FP)), archived("b.wav", 10, None)],
            vec![modification("clip-1", REL), modification("clip-2", "b.wav")],
        );
        let graph = graph(
            &[(REL, FRAMES - 100), ("b.wav", 10)],
            &[("clip-1", REL), ("clip-2", "b.wav")],
        );
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[])),
            AraRestorePlan::Mapped(AraRestoreMap::default())
        );
        let alone = super::tests::identity(
            vec![archived(REL, FRAMES, Some(FP))],
            vec![modification("clip-1", REL)],
        );
        let changed = super::tests::graph(&[(REL, FRAMES - 100)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(Some(&alone), &changed, &fingerprints(&[])),
            AraRestorePlan::NothingMatches
        );
    }

    /// A current source the archive holds under its own ID keeps its own
    /// state; a moved source with the same audio is not mapped over it.
    #[test]
    fn a_moved_source_never_takes_a_source_restored_as_itself() {
        let identity = identity(
            vec![
                archived(ABS, FRAMES, Some(FP)),
                archived(REL, FRAMES, Some(FP)),
            ],
            vec![modification("clip-1", ABS), modification("clip-2", REL)],
        );
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL), ("clip-2", REL)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[(REL, FP)])),
            AraRestorePlan::AsSaved
        );
    }

    /// Clips of one archived source now on two different sources give no
    /// single place.
    #[test]
    fn clips_that_disagree_give_no_place() {
        let identity = identity(
            vec![archived(ABS, FRAMES, Some(FP))],
            vec![modification("clip-1", ABS), modification("clip-2", ABS)],
        );
        let graph = graph(
            &[(REL, FRAMES), ("Assets/Audio/1-1.wav", FRAMES)],
            &[("clip-1", REL), ("clip-2", "Assets/Audio/1-1.wav")],
        );
        let known = fingerprints(&[(REL, FP), ("Assets/Audio/1-1.wav", FP)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &known),
            AraRestorePlan::NothingMatches
        );
    }

    /// Thai file names are published under encoded IDs, and the record holds
    /// those, so an unchanged Thai project restores as saved.
    #[test]
    fn non_ascii_keys_match_under_their_published_ids() {
        let thai = "/Users/doppio/Projects/โปรเจกต์ไม่มีชื่อ-1/Recordings/take.rauf";
        let identity = identity(
            vec![archived(thai, FRAMES, None)],
            vec![modification("คลิป-1", thai)],
        );
        assert!(identity.sources[0].persistent_id.starts_with("fbx:"));
        let graph = graph(&[(thai, FRAMES)], &[("คลิป-1", thai)]);
        assert_eq!(
            plan_restore(Some(&identity), &graph, &fingerprints(&[])),
            AraRestorePlan::AsSaved
        );
    }

    #[test]
    fn an_empty_document_restores_as_saved() {
        let empty = ProjectAraIdentity::default();
        let graph = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            plan_restore(Some(&empty), &graph, &fingerprints(&[])),
            AraRestorePlan::AsSaved
        );
    }

    fn offline(clips: &[(&str, &str)]) -> Vec<(AraClipKey, AraSourceKey)> {
        clips
            .iter()
            .map(|(clip, source)| (AraClipKey::from(*clip), AraSourceKey::from(*source)))
            .collect()
    }

    fn timing(
        identity: Option<&ProjectAraIdentity>,
        graph: &AraGraph,
        offline: &[(AraClipKey, AraSourceKey)],
    ) -> RestoreTiming {
        restore_timing(identity, graph, offline)
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Finding (review, verified): a legacy document restored into a graph
    /// with no sources was accepted by the plug-in, judged unconfirmed, never
    /// retried when the audio came back, and dropped on unbind. With no audio
    /// in the graph nothing can land and the live document holds no work, so
    /// it waits, whole.
    #[test]
    fn a_graph_without_sources_waits_for_its_audio() {
        let empty = graph(&[], &[]);
        let gone = offline(&[("clip-1", REL)]);
        assert_eq!(
            timing(None, &empty, &gone),
            RestoreTiming::AwaitMedia { offline: true },
            "the evidence project opened with its media unplugged"
        );
        let recorded = identity(
            vec![archived(REL, FRAMES, Some(FP))],
            vec![modification("clip-1", REL)],
        );
        assert_eq!(
            timing(Some(&recorded), &empty, &gone),
            RestoreTiming::AwaitMedia { offline: true }
        );
        // No clip at all: it waits too, but nothing is offline to report.
        assert_eq!(
            timing(None, &empty, &[]),
            RestoreTiming::AwaitMedia { offline: false }
        );
        // A document that holds no audio has nothing to wait for.
        assert_eq!(
            timing(Some(&ProjectAraIdentity::default()), &empty, &gone),
            RestoreTiming::Now
        );
    }

    /// Finding (review, verified with Melodyne): a document waiting whole for
    /// part of its audio overwrote the work done meanwhile on the audio that
    /// was there, and piled up a copy of the live document per save. Now the
    /// offline part is kept back and the rest restored at once.
    #[test]
    fn a_recorded_document_with_one_of_two_sources_offline_restores_the_other_now() {
        let recorded = identity(
            vec![archived(REL, FRAMES, Some(FP)), archived("b.wav", 10, None)],
            vec![modification("clip-1", REL), modification("clip-2", "b.wav")],
        );
        let partial = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            timing(Some(&recorded), &partial, &offline(&[("clip-2", "b.wav")])),
            RestoreTiming::Partly {
                deferred: ids(&["b.wav"])
            }
        );
        assert_eq!(
            sources_except(&recorded, &ids(&["b.wav"])),
            ids(&[REL]),
            "what the first part restores"
        );
        // Found by its clip alone: its source was renamed, and is offline
        // under the new name.
        assert_eq!(
            timing(
                Some(&recorded),
                &partial,
                &offline(&[("clip-2", "Assets/Audio/b-renamed.wav")])
            ),
            RestoreTiming::Partly {
                deferred: ids(&["b.wav"])
            }
        );
        // Everything back: restored whole, now.
        let whole = graph(
            &[(REL, FRAMES), ("b.wav", 10)],
            &[("clip-1", REL), ("clip-2", "b.wav")],
        );
        assert_eq!(timing(Some(&recorded), &whole, &[]), RestoreTiming::Now);
    }

    /// The recorded audio is all offline while the track plays other audio:
    /// the live document can hold work on that audio, so it does not wait
    /// whole either; the first part holds no source, only the document data.
    #[test]
    fn recorded_audio_all_offline_beside_other_audio_is_kept_back_whole() {
        let recorded = identity(
            vec![archived(ABS, FRAMES, Some(FP))],
            vec![modification("clip-1", ABS)],
        );
        let other = graph(&[("new.wav", 10)], &[("clip-9", "new.wav")]);
        assert_eq!(
            timing(Some(&recorded), &other, &offline(&[("clip-1", ABS)])),
            RestoreTiming::Partly {
                deferred: ids(&[ABS])
            }
        );
        assert!(sources_except(&recorded, &ids(&[ABS])).is_empty());
    }

    /// Recorded audio that is not offline but gone (its clips deleted) or
    /// replaced (its clip now on other audio) is a miss, not kept back.
    #[test]
    fn recorded_audio_deleted_or_replaced_is_not_kept_back() {
        let recorded = identity(
            vec![archived(REL, FRAMES, Some(FP))],
            vec![modification("clip-1", REL)],
        );
        let other = graph(&[("new.wav", 10)], &[("clip-9", "new.wav")]);
        assert_eq!(
            timing(Some(&recorded), &other, &[]),
            RestoreTiming::Now,
            "deleted"
        );
        // Offline media the document never held changes nothing.
        assert_eq!(
            timing(
                Some(&recorded),
                &other,
                &offline(&[("clip-5", "other.wav")])
            ),
            RestoreTiming::Now
        );
        let replaced = graph(&[("render.wav", FRAMES)], &[("clip-1", "render.wav")]);
        assert_eq!(
            timing(Some(&recorded), &replaced, &offline(&[("clip-5", "b.wav")])),
            RestoreTiming::Now,
            "clip-1 is in the graph on other audio"
        );
        // A source in the graph under its own ID is there, whatever clip of
        // it is offline.
        let two = identity(
            vec![archived(REL, FRAMES, Some(FP))],
            vec![modification("clip-1", REL), modification("clip-2", REL)],
        );
        let here = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(
            timing(Some(&two), &here, &offline(&[("clip-2", "copy.wav")])),
            RestoreTiming::Now
        );
    }

    /// A legacy document has no record of which audio it holds: each offline
    /// source is taken to be archived under its published ID, and kept back.
    #[test]
    fn a_legacy_document_keeps_back_the_sources_of_its_offline_clips() {
        let partial = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert_eq!(timing(None, &partial, &[]), RestoreTiming::Now);
        assert_eq!(
            timing(
                None,
                &partial,
                &offline(&[
                    ("clip-2", "b.wav"),
                    ("clip-3", "b.wav"),
                    ("clip-4", "c.wav")
                ])
            ),
            RestoreTiming::Partly {
                deferred: ids(&["b.wav", "c.wav"])
            }
        );
        // Under their published IDs.
        let thai = "/p/โ/1.wav";
        assert_eq!(
            offline_sources(None, &partial, &offline(&[("clip-2", thai)])),
            vec![ara_persistent_id(thai).into_owned()]
        );
    }

    /// Which kept-back sources a later sync brings back, and through which
    /// map: by their own ID and shape, or moved onto the same audio.
    #[test]
    fn a_kept_back_source_returns_by_its_own_id_or_through_its_clip() {
        let recorded = identity(
            vec![
                archived(REL, FRAMES, Some(FP)),
                archived(ABS, FRAMES, Some(FP)),
            ],
            vec![modification("clip-1", REL), modification("clip-2", ABS)],
        );
        let remaining = ids(&[ABS]);
        let none = fingerprints(&[]);
        // Still offline.
        let without = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert!(
            returning_sources(Some(&recorded), &remaining, &without, &none)
                .0
                .is_empty()
        );
        // Back under its own ID.
        let back = graph(
            &[(REL, FRAMES), (ABS, FRAMES)],
            &[("clip-1", REL), ("clip-2", ABS)],
        );
        assert_eq!(
            returning_sources(Some(&recorded), &remaining, &back, &none),
            (ids(&[ABS]), AraRestoreMap::default())
        );
        // Back under its own ID but other audio: not its place.
        let reshaped = graph(
            &[(REL, FRAMES), (ABS, FRAMES + 1)],
            &[("clip-1", REL), ("clip-2", ABS)],
        );
        assert!(
            returning_sources(Some(&recorded), &remaining, &reshaped, &none)
                .0
                .is_empty()
        );
        // Back as a copy of the same audio, found through its clip.
        let copied = graph(
            &[(REL, FRAMES), ("Assets/Audio/2.wav", FRAMES)],
            &[("clip-1", REL), ("clip-2", "Assets/Audio/2.wav")],
        );
        let known = fingerprints(&[(REL, FP), ("Assets/Audio/2.wav", FP)]);
        assert_eq!(
            returning_sources(Some(&recorded), &remaining, &copied, &known),
            (
                ids(&[ABS]),
                AraRestoreMap {
                    sources: vec![(ABS.to_owned(), AraSourceKey::from("Assets/Audio/2.wav"))],
                    modifications: Vec::new(),
                }
            )
        );
        // Legacy: by the ID it is taken to be archived under.
        assert_eq!(
            returning_sources(None, &ids(&["b.wav"]), &graph(&[("b.wav", 3)], &[]), &none),
            (ids(&["b.wav"]), AraRestoreMap::default())
        );
    }

    #[test]
    fn only_a_record_whose_every_object_is_where_it_was_restores_in_place() {
        let recorded = identity(
            vec![archived(REL, FRAMES, Some(FP)), archived("b.wav", 10, None)],
            vec![modification("clip-1", REL), modification("clip-2", "b.wav")],
        );
        let whole = graph(
            &[(REL, FRAMES), ("b.wav", 10)],
            &[("clip-1", REL), ("clip-2", "b.wav")],
        );
        assert!(restores_in_place(&recorded, &whole, None));
        // Extra audio the record never held changes nothing.
        let more = graph(
            &[(REL, FRAMES), ("b.wav", 10), ("c.wav", 5)],
            &[("clip-1", REL), ("clip-2", "b.wav"), ("clip-3", "c.wav")],
        );
        assert!(restores_in_place(&recorded, &more, None));
        // A source missing, of another shape, or a clip on other audio.
        let missing = graph(&[(REL, FRAMES)], &[("clip-1", REL)]);
        assert!(!restores_in_place(&recorded, &missing, None));
        let reshaped = graph(
            &[(REL, FRAMES), ("b.wav", 11)],
            &[("clip-1", REL), ("clip-2", "b.wav")],
        );
        assert!(!restores_in_place(&recorded, &reshaped, None));
        let moved = graph(
            &[(REL, FRAMES), ("b.wav", 10)],
            &[("clip-1", REL), ("clip-2", REL)],
        );
        assert!(!restores_in_place(&recorded, &moved, None));
        // One part of the record: only its own objects count.
        assert!(restores_in_place(&recorded, &missing, Some(&ids(&[REL]))));
        assert!(!restores_in_place(
            &recorded,
            &missing,
            Some(&ids(&["b.wav"]))
        ));
        assert!(restores_in_place(&recorded, &missing, Some(&[])));
        // An empty record holds nothing to misplace.
        assert!(restores_in_place(
            &ProjectAraIdentity::default(),
            &missing,
            None
        ));
        // Thai keys are compared under their published IDs.
        let thai = "/Users/doppio/Projects/โปรเจกต์ไม่มีชื่อ-1/Recordings/take.rauf";
        let thai_record = identity(
            vec![archived(thai, FRAMES, None)],
            vec![modification("คลิป-1", thai)],
        );
        assert!(restores_in_place(
            &thai_record,
            &graph(&[(thai, FRAMES)], &[("คลิป-1", thai)]),
            None
        ));
    }

    #[test]
    fn identities_convert_both_ways_without_loss() {
        let recorded = identity(
            vec![archived(ABS, FRAMES, None), archived("โ.wav", 3, None)],
            vec![modification("clip-1", ABS)],
        );
        let host = host_identity(&recorded);
        assert_eq!(host.sources[1].key.as_str(), "โ.wav");
        assert_eq!(host.keys, Some(AraKeyDescriptor(7)));
        assert_eq!(project_identity(&host), recorded);
    }
}
