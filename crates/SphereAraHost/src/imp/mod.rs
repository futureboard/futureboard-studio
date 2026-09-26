//! Windows / macOS ARA host runtime.
//!
//! Owns the ARA host services, the plug-in's document controller, the document
//! graph, and every companion binding. See [`crate`] for the boundary rules.

mod services;

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::Arc;

use ara2_bridge_companion::CompanionRoles;
use ara2_bridge_companion::vst3::Vst3HostPlugin;
use ara2_bridge_core::{
    ApiGeneration, AraError, AudioModificationProperties, AudioSourceProperties, Color,
    ContentKind, ContentUpdateScopes, DocumentProperties, MusicalContextProperties, Notes,
    PlaybackRegionProperties, PlaybackTransformationFlags, RegionSequenceProperties, RestoreFilter,
};
use ara2_bridge_host::{
    AudioModificationHandle, AudioSourceHandle, DocumentSession, ExtensionController,
    ExtensionRoles, HostServices, HostServicesBuilder, LoadedFactory, MusicalContextHandle,
    PlaybackRegionAssignment, PlaybackRegionHandle, RegionSequenceHandle, RendererRole,
};

use crate::AraSessionConfig;
use crate::archive::{
    AraArchiveIdentity, AraArchivedModification, AraArchivedSource, AraKeyDescriptor,
    AraRestoreReport, AraRestoreRequest, AraSourceRestore, AraStoredArchive, GRADE_DETECTED,
    GRADE_INITIAL, PlannedFilter, RestorePlan, classify_source, needs_harmony_resync,
    overall_outcome, plan_restore,
};
use crate::error::{AraHostError, AraResult};
use crate::info::{AraFactoryInfo, AraRendererId, AraRoles};
use crate::model::{
    AraAudioSourceDesc, AraClipKey, AraColor, AraGraph, AraGraphChange, AraMusicalTimeline,
    AraPlaybackRegionDesc, AraPlaybackTransform, AraRegionSequenceDesc, AraSourceKey, AraTrackKey,
    HeldGraph, ara_persistent_id,
};

use services::{
    AnalysisLog, ArchiveService, ArchiveSlot, ArchiveStore, AudioService, ContentService,
    GraphIndex, ModelService, SharedContent, TransportService, offers_harmonic_content, trace,
};

/// Generations tried when initializing a factory, best first.
///
/// ARA 1 is deliberately excluded: it has no region sequences, so the whole
/// track-to-sequence mapping this crate is built on does not exist there, and no
/// plug-in Futureboard targets is ARA 1 only.
const GENERATIONS: [ApiGeneration; 4] = [
    ApiGeneration::V23Final,
    ApiGeneration::V2xDraft,
    ApiGeneration::V2Final,
    ApiGeneration::V2Draft,
];

/// Name of the single musical context every region sequence hangs off.
const MUSICAL_CONTEXT_NAME: &str = "Futureboard timeline";

/// Scopes a musical context never touches: it carries timing and harmony, and
/// neither the signal, the notes nor the tuning of any audio.
///
/// ARA's flags name what is *unchanged*, so a host says "only X moved" by
/// listing everything but X.
const CONTEXT_NEVER_CHANGES: ContentUpdateScopes = ContentUpdateScopes::SIGNAL_REMAINS_UNCHANGED
    .union(ContentUpdateScopes::NOTE_REMAINS_UNCHANGED)
    .union(ContentUpdateScopes::TUNING_REMAINS_UNCHANGED);

/// Scopes that stay untouched when only the key moved.
const HARMONY_CHANGED: ContentUpdateScopes =
    CONTEXT_NEVER_CHANGES.union(ContentUpdateScopes::TIMING_REMAINS_UNCHANGED);

/// What publishing a new musical timeline asks of the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextUpdate {
    /// No musical context exists yet.
    Create,
    /// Content the plug-in can read changed; the flags name what did not.
    Notify(ContentUpdateScopes),
    /// Nothing the plug-in can read changed, so no edit cycle at all: even an
    /// empty one is not free, since a plug-in may re-evaluate its internal
    /// state at `endEditing` (Melodyne updates its per-clip scale copies there).
    Nothing,
}

/// Diffs the last published timeline (`None` before the context exists)
/// against `next`, as a document negotiated at `generation` sees them.
///
/// Keys only count from ARA 2.0 Final on: below that the plug-in is offered no
/// key content, so a key-only change is invisible to it, and the harmonic
/// scope flag, itself a 2.0 Final addendum, is never sent.
fn plan_context_update(
    prev: Option<&AraMusicalTimeline>,
    next: &AraMusicalTimeline,
    generation: ApiGeneration,
) -> ContextUpdate {
    let Some(prev) = prev else {
        return ContextUpdate::Create;
    };
    let harmonic = offers_harmonic_content(generation);
    let timing_same = prev.tempo == next.tempo && prev.bars == next.bars;
    let harmony_same = !harmonic || prev.keys == next.keys;
    if timing_same && harmony_same {
        return ContextUpdate::Nothing;
    }
    let mut scopes = CONTEXT_NEVER_CHANGES;
    if timing_same {
        scopes |= ContentUpdateScopes::TIMING_REMAINS_UNCHANGED;
    }
    if harmonic && harmony_same {
        scopes |= ContentUpdateScopes::HARMONIC_REMAINS_UNCHANGED;
    }
    ContextUpdate::Notify(scopes)
}

/// Lets the plug-in deliver pending model notifications right after an edit.
///
/// ARA asks hosts that read plug-in content to do this within the same undo
/// frame as `endEditing`. A failure here costs only a delayed notification —
/// the periodic call from the application retries — so it is traced, not
/// returned.
fn notify_after_edit(document: &mut DocumentSession<'static, 'static>) {
    if let Err(error) = document.notify_model_updates() {
        trace(&format!("model updates after an edit: {error}"));
    }
}

pub(crate) fn is_supported() -> bool {
    true
}

fn map_error(error: AraError) -> AraHostError {
    match error {
        AraError::Poisoned => AraHostError::Poisoned,
        AraError::InvalidArgument(what) | AraError::InvalidState(what) => {
            AraHostError::Invalid(what.to_owned())
        }
        AraError::Unsupported(what) => AraHostError::Unsupported(what.to_owned()),
        other => AraHostError::Plugin(other.to_string()),
    }
}

fn to_color(color: Option<AraColor>) -> AraResult<Option<Color>> {
    color
        .map(|color| Color::new(color.red, color.green, color.blue).map_err(map_error))
        .transpose()
}

fn transform_flags(transform: AraPlaybackTransform) -> PlaybackTransformationFlags {
    let mut flags = PlaybackTransformationFlags::empty();
    flags.set(
        PlaybackTransformationFlags::TIMESTRETCH,
        transform.timestretch,
    );
    flags.set(
        PlaybackTransformationFlags::REFLECT_TEMPO,
        transform.timestretch_reflecting_tempo,
    );
    flags.set(
        PlaybackTransformationFlags::CONTENT_FADE_HEAD,
        transform.content_based_fade_at_head,
    );
    flags.set(
        PlaybackTransformationFlags::CONTENT_FADE_TAIL,
        transform.content_based_fade_at_tail,
    );
    flags
}

fn transform_from_flags(flags: PlaybackTransformationFlags) -> AraPlaybackTransform {
    AraPlaybackTransform {
        timestretch: flags.contains(PlaybackTransformationFlags::TIMESTRETCH),
        timestretch_reflecting_tempo: flags.contains(PlaybackTransformationFlags::REFLECT_TEMPO),
        content_based_fade_at_head: flags.contains(PlaybackTransformationFlags::CONTENT_FADE_HEAD),
        content_based_fade_at_tail: flags.contains(PlaybackTransformationFlags::CONTENT_FADE_TAIL),
    }
}

fn companion_roles(roles: AraRoles) -> CompanionRoles {
    let mut bits = CompanionRoles::empty();
    bits.set(CompanionRoles::PLAYBACK_RENDERER, roles.playback_renderer);
    bits.set(CompanionRoles::EDITOR_RENDERER, roles.editor_renderer);
    bits.set(CompanionRoles::EDITOR_VIEW, roles.editor_view);
    bits
}

fn extension_roles(roles: AraRoles) -> ExtensionRoles {
    let mut bits = ExtensionRoles::empty();
    bits.set(ExtensionRoles::PLAYBACK_RENDERER, roles.playback_renderer);
    bits.set(ExtensionRoles::EDITOR_RENDERER, roles.editor_renderer);
    bits.set(ExtensionRoles::EDITOR_VIEW, roles.editor_view);
    bits
}

/// Reads factory metadata for `generation`, copying every string.
fn describe(factory: &LoadedFactory<'_>, generation: ApiGeneration) -> AraFactoryInfo {
    let metadata = factory.metadata();
    AraFactoryInfo {
        factory_id: metadata.factory_id().to_owned(),
        plug_in_name: metadata.plug_in_name().to_owned(),
        manufacturer_name: metadata.manufacturer_name().to_owned(),
        version: metadata.version().to_owned(),
        information_url: metadata.information_url().to_owned(),
        document_archive_id: metadata.document_archive_id().to_owned(),
        compatible_archive_ids: metadata.compatible_archive_ids().to_vec(),
        supported_transforms: transform_from_flags(PlaybackTransformationFlags::from_bits_retain(
            metadata.playback_transformations() as u32,
        )),
        stores_audio_file_chunks: metadata.stores_audio_file_chunks(),
        api_generation: generation.as_raw(),
    }
}

/// Initializes `factory` at the best generation both sides support.
///
/// # Safety
///
/// `factory` must be a live `ARAFactory*` whose metadata backing stays readable
/// for the lifetime of the returned value.
unsafe fn load_factory(
    factory: *const c_void,
) -> AraResult<(LoadedFactory<'static>, ApiGeneration)> {
    if factory.is_null() {
        return Err(AraHostError::unsupported("plug-in exposes no ARA factory"));
    }
    let raw = factory.cast::<ara2_bridge_sys::ARAFactory>();
    let mut last = AraHostError::unsupported("no shared ARA 2 generation with this plug-in");
    for generation in GENERATIONS {
        // SAFETY: the caller guarantees the factory backing; a generation
        // mismatch is rejected before the factory is initialized, so retrying
        // another generation cannot double-initialize it.
        match unsafe { LoadedFactory::load(raw, generation, None) } {
            Ok(loaded) => return Ok((loaded, generation)),
            Err(error) => last = map_error(error),
        }
    }
    Err(last)
}

/// # Safety
///
/// See [`crate::vst3_ara_factory`].
pub(crate) unsafe fn vst3_ara_factory(main_factory: *mut c_void) -> AraResult<*const c_void> {
    if main_factory.is_null() {
        return Err(AraHostError::unsupported(
            "plug-in exposes no ARA main factory",
        ));
    }
    // SAFETY: the caller guarantees a live VST3 object identity.
    let queried =
        unsafe { ara2_bridge_companion::vst3::Vst3HostMainFactory::discover(main_factory) }
            .map_err(map_error)?;
    // The queried `IMainFactory` reference is released when `queried` drops; the
    // `ARAFactory` it returns belongs to the class instance the caller owns and
    // outlives this call.
    let factory = queried.factory().map_err(map_error)?;
    Ok(factory.cast())
}

/// # Safety
///
/// See [`crate::AraSession::probe_factory`].
pub(crate) unsafe fn probe_factory(factory: *const c_void) -> AraResult<AraFactoryInfo> {
    // SAFETY: the caller forwards the live-factory contract.
    let (loaded, generation) = unsafe { load_factory(factory) }?;
    Ok(describe(&loaded, generation))
}

struct SourceEntry {
    handle: AudioSourceHandle,
    address: usize,
    desc: AraAudioSourceDesc,
}

struct SequenceEntry {
    handle: RegionSequenceHandle,
    desc: AraRegionSequenceDesc,
}

struct ClipEntry {
    modification: AudioModificationHandle,
    modification_address: usize,
    region: PlaybackRegionHandle,
    region_address: usize,
    desc: AraPlaybackRegionDesc,
}

/// One bound companion instance and everything it currently renders.
struct Renderer {
    /// Keeps the companion entry-point COM reference alive; its `Drop` releases.
    _plugin: Vst3HostPlugin<'static>,
    extension: ExtensionController<'static>,
    roles: AraRoles,
    /// RAII assignments for the playback-renderer role — dropping one removes
    /// the region from the plug-in.
    assignments: HashMap<AraClipKey, PlaybackRegionAssignment>,
    /// The same regions again for the editor-renderer role.
    ///
    /// ARA treats the two renderer roles as separate consumers: a region handed
    /// only to the playback renderer is audible but is not something the
    /// plug-in's own editor is working on, which is why an editor opened on it
    /// comes up empty.
    editor_assignments: HashMap<AraClipKey, PlaybackRegionAssignment>,
}

/// Token whose address identifies one archive transfer to the plug-in.
///
/// ARA reports archive handles as the address of the reader/writer the host
/// passed in, so this must be a real, uniquely addressed allocation.
struct ArchiveToken {
    _sequence: u64,
}

/// Serves `bytes`, written under `archive_id`, to the plug-in for the length
/// of `call`, through a freshly addressed token.
fn with_archive<T>(
    archives: &ArchiveStore,
    sequence: u64,
    archive_id: &str,
    bytes: &[u8],
    call: impl FnOnce(&ArchiveToken) -> T,
) -> T {
    let token = Box::new(ArchiveToken {
        _sequence: sequence,
    });
    let address = std::ptr::from_ref(token.as_ref()) as usize;
    // The ID the bytes were written under, not the plug-in's current one: an
    // archive from an older version must be read as that version.
    archives.open(
        address,
        ArchiveSlot {
            bytes: bytes.to_vec(),
            archive_id: Some(archive_id.to_owned()),
        },
    );
    let out = call(token.as_ref());
    archives.take(address);
    out
}

/// Builds the bridge's restore filter from a planned one.
///
/// Document data is selected as planned: always for a whole restore, where
/// leaving it out would leave the plug-in's private document state behind,
/// and for a partial one only in the part that brings it (see
/// [`crate::AraRestoreScope`]).
fn restore_filter(planned: &PlannedFilter) -> AraResult<RestoreFilter> {
    let mut builder = RestoreFilter::builder().document_data(planned.document_data);
    for (archived, current) in &planned.sources {
        builder = builder.audio_source(archived.clone(), current.clone());
    }
    for (archived, current) in &planned.modifications {
        builder = builder.audio_modification(archived.clone(), current.clone());
    }
    builder
        .build()
        .map_err(|error| AraHostError::invalid(format!("restore filter: {error}")))
}

/// Clips whose region and modification must go before the graph is rebuilt:
/// those no longer wanted, and those that moved to another source or track.
///
/// ARA creates a modification on its source for good, and an audio source
/// cannot be destroyed while a modification still sits on it. A clip whose
/// audio changed therefore loses its region and modification first and gets
/// new ones on the new source; updating the region in place left the
/// modification on the old source and made its teardown fail.
fn doomed_clips<'a>(
    held: impl Iterator<Item = (&'a AraClipKey, &'a AraPlaybackRegionDesc)>,
    wanted: &[AraPlaybackRegionDesc],
) -> Vec<AraClipKey> {
    let wanted: HashMap<&AraClipKey, &AraPlaybackRegionDesc> =
        wanted.iter().map(|desc| (&desc.key, desc)).collect();
    let mut doomed: Vec<AraClipKey> = held
        .filter(|(key, held)| {
            wanted
                .get(key)
                .is_none_or(|desc| desc.source != held.source || desc.track != held.track)
        })
        .map(|(key, _)| key.clone())
        .collect();
    doomed.sort();
    doomed
}

/// A restore settled before the plug-in is touched.
struct PreparedRestore {
    plan: RestorePlan,
    filter: Option<RestoreFilter>,
}

/// A restore under way: its plan plus what was read before it ran.
struct RestoreRun {
    plan: RestorePlan,
    filter: Option<RestoreFilter>,
    /// Each target's note grade before the restore.
    before: Vec<Option<i32>>,
    /// Each target's analysis-progress report count before the restore.
    progress: Vec<u64>,
}

/// A restore that runs inside an edit cycle: the one building the graph, or
/// one of its own.
struct InEditRestore<'a, 'b> {
    request: &'a AraRestoreRequest<'b>,
    filter: Option<&'a RestoreFilter>,
    sequence: u64,
    result: Option<AraResult<()>>,
}

pub(crate) struct Session {
    /// Leaked so the borrow checker sees `'static`; reclaimed in `Drop` strictly
    /// after `document`, which holds the controller that borrows both.
    services_ptr: *mut HostServices,
    factory_ptr: *mut LoadedFactory<'static>,
    document: Option<DocumentSession<'static, 'static>>,
    info: AraFactoryInfo,
    /// The negotiated generation, which decides whether keys are published.
    generation: ApiGeneration,

    index: Arc<GraphIndex>,
    archives: Arc<ArchiveStore>,
    content: Arc<ContentService>,
    /// Analysis progress per source, shared with the model-update service.
    analysis: Arc<AnalysisLog>,
    /// Whether the plug-in analyses notes, whose content grade is how a
    /// restore is verified.
    notes_analysable: bool,

    /// Last document name pushed to the plug-in, so an unchanged name performs
    /// no ABI call on re-apply.
    document_name: Option<String>,
    musical_context: Option<MusicalContextHandle>,
    /// The timeline the plug-in last acknowledged, set together with
    /// `musical_context`; what the next one is diffed against.
    published: Option<AraMusicalTimeline>,
    sources: HashMap<AraSourceKey, SourceEntry>,
    sequences: HashMap<AraTrackKey, SequenceEntry>,
    clips: HashMap<AraClipKey, ClipEntry>,

    renderers: HashMap<AraRendererId, Renderer>,
    next_renderer: u64,
    next_archive: u64,

    /// `ExtensionController` is `Rc`-backed, and ARA model calls belong to one
    /// thread anyway. Making that explicit stops the session being moved.
    _not_send: PhantomData<*const ()>,
}

impl Session {
    /// # Safety
    ///
    /// See [`crate::AraSession::open`].
    pub(crate) unsafe fn open(factory: *const c_void, config: AraSessionConfig) -> AraResult<Self> {
        // SAFETY: the caller forwards the live-factory contract.
        let (loaded, generation) = unsafe { load_factory(factory) }?;
        let info = describe(&loaded, generation);
        let notes_analysable = loaded
            .metadata()
            .analyzable_content_types()
            .contains(&<Notes as ContentKind>::RAW_TYPE);

        let index = Arc::new(GraphIndex::default());
        let archives = Arc::new(ArchiveStore::default());
        let content = Arc::new(ContentService::new(Arc::clone(&index), generation));
        let analysis = Arc::new(AnalysisLog::default());

        let services = HostServicesBuilder::new()
            .audio(AudioService::new(config.audio, Arc::clone(&index)))
            .archiving(ArchiveService::new(Arc::clone(&archives)))
            .model_updates(ModelService::new(
                config.observer,
                Arc::clone(&index),
                Arc::clone(&analysis),
            ))
            .playback(TransportService::new(config.transport))
            .content(SharedContent(Arc::clone(&content)))
            .build(generation)
            .map_err(map_error)?;

        let properties =
            DocumentProperties::new(config.document_name.as_deref()).map_err(map_error)?;

        // The controller borrows both the factory and the services for its whole
        // life, which cannot be expressed inside one owning struct. Leaking both
        // and reclaiming them in `Drop` — after the document, in this exact
        // order — keeps that invariant explicit and checkable in one place.
        let services_ptr = Box::into_raw(Box::new(services));
        let factory_ptr = Box::into_raw(Box::new(loaded));

        // SAFETY: both boxes were just created here, are not aliased, and stay
        // live until `Drop` reclaims them after `document` is gone.
        let (services_ref, factory_ref) = unsafe { (&*services_ptr, &*factory_ptr) };

        let document = match DocumentSession::new(factory_ref, services_ref, properties) {
            Ok(document) => document,
            Err(error) => {
                // SAFETY: nothing borrows either box on this path.
                unsafe {
                    drop(Box::from_raw(factory_ptr));
                    drop(Box::from_raw(services_ptr));
                }
                return Err(map_error(error));
            }
        };

        Ok(Self {
            services_ptr,
            factory_ptr,
            document: Some(document),
            info,
            generation,
            index,
            archives,
            content,
            analysis,
            notes_analysable,
            document_name: config.document_name,
            musical_context: None,
            published: None,
            sources: HashMap::new(),
            sequences: HashMap::new(),
            clips: HashMap::new(),
            renderers: HashMap::new(),
            next_renderer: 1,
            next_archive: 1,
            _not_send: PhantomData,
        })
    }

    pub(crate) fn factory(&self) -> &AraFactoryInfo {
        &self.info
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        self.document
            .as_ref()
            .is_some_and(DocumentSession::is_poisoned)
    }

    fn document_mut(&mut self) -> AraResult<&mut DocumentSession<'static, 'static>> {
        let document = self
            .document
            .as_mut()
            .ok_or_else(|| AraHostError::invalid("ARA session is already closed"))?;
        if document.is_poisoned() {
            return Err(AraHostError::Poisoned);
        }
        Ok(document)
    }

    pub(crate) fn set_musical_timeline(&mut self, timeline: &AraMusicalTimeline) -> AraResult<()> {
        let plan = match self.musical_context {
            Some(_) => plan_context_update(self.published.as_ref(), timeline, self.generation),
            None => ContextUpdate::Create,
        };
        if plan == ContextUpdate::Nothing {
            return Ok(());
        }

        // Content goes in before the edit: the plug-in reads it back from
        // inside the calls below, re-entering `ContentService` on this thread.
        // `publish` has released its lock by the time any of them runs.
        self.content.publish(timeline);

        let Self {
            document,
            index,
            musical_context,
            published,
            ..
        } = self;
        let document = document
            .as_mut()
            .ok_or_else(|| AraHostError::invalid("ARA session is already closed"))?;
        if document.is_poisoned() {
            return Err(AraHostError::Poisoned);
        }

        // The context object itself carries only a name, order, and colour, none
        // of which ever change; the tempo map, meter and key reach the plug-in
        // through the content reader, so an existing context only ever gets a
        // content-changed notification.
        let mut edit = document.edit().map_err(map_error)?;
        let handle = match (plan, *musical_context) {
            (ContextUpdate::Notify(scopes), Some(handle)) => {
                edit.update_musical_context_content(handle, None, scopes)
                    .map_err(map_error)?;
                handle
            }
            _ => {
                let properties = MusicalContextProperties::new(Some(MUSICAL_CONTEXT_NAME), 0, None)
                    .map_err(map_error)?;
                let handle = edit.create_musical_context(properties).map_err(map_error)?;
                // Index before the edit closes: the plug-in reads the context's
                // content from inside `endEditing`, and an identity registered
                // afterwards makes every one of those reads come back empty.
                let address = edit
                    .musical_context_ref(handle)
                    .map_err(map_error)?
                    .as_raw() as usize;
                index.set_musical_context(Some(address));
                // Reads made from inside `createMusicalContext` itself could not
                // be answered — the identity did not exist yet — so the plug-in
                // is told that everything changed, now that it can re-read.
                edit.update_musical_context_content(handle, None, ContentUpdateScopes::empty())
                    .map_err(map_error)?;
                handle
            }
        };
        edit.finish().map_err(map_error)?;

        *musical_context = Some(handle);
        *published = Some(timeline.clone());
        notify_after_edit(document);
        Ok(())
    }

    /// How far `graph` departs from what the document holds.
    pub(crate) fn graph_change(&self, graph: &AraGraph) -> AraGraphChange {
        graph.change_from(self)
    }

    pub(crate) fn notify_model_updates(&mut self) -> AraResult<()> {
        self.document_mut()?
            .notify_model_updates()
            .map_err(map_error)
    }

    pub(crate) fn apply_graph(&mut self, graph: &AraGraph) -> AraResult<()> {
        self.apply_graph_restoring(graph, None).map(|_| ())
    }

    /// Reconciles the graph and, when asked, restores an archive into it.
    ///
    /// From ARA 2 Final on the restore runs inside the edit cycle that builds
    /// the graph, after every object exists and before the cycle ends: the
    /// order `ARAInterface.h` documents for `restoreObjectsFromArchive`. A
    /// plug-in then never starts analysing a source whose analysis the
    /// archive is about to supply. Note analysis is requested afterwards, and
    /// only for new sources the restore left without content.
    ///
    /// `Err` means the graph was not applied. A restore that is refused or
    /// fails is reported in [`AraRestoreReport::error`] and never stops the
    /// graph.
    pub(crate) fn apply_graph_restoring(
        &mut self,
        graph: &AraGraph,
        restore: Option<AraRestoreRequest<'_>>,
    ) -> AraResult<Option<AraRestoreReport>> {
        let requests = restore.as_slice();
        let mut reports = self.apply_graph_restoring_all(graph, requests)?;
        Ok(reports.pop())
    }

    /// [`Self::apply_graph_restoring`] with any number of restores, run one
    /// after another, in the order given, inside the same edit cycle (ARA
    /// allows several calls in one cycle). One report per request, in order.
    ///
    /// The caller puts the restore that brings the document data last: ARA
    /// asks for that part to be restored once the graph has its final
    /// structure and every object's state is in place.
    pub(crate) fn apply_graph_restoring_all(
        &mut self,
        graph: &AraGraph,
        requests: &[AraRestoreRequest<'_>],
    ) -> AraResult<Vec<AraRestoreReport>> {
        let context = self.musical_context.ok_or_else(|| {
            AraHostError::invalid("set_musical_timeline must run before apply_graph")
        })?;
        // No edit cycle for a graph that is already in place: `endEditing` is
        // not free for the plug-in even when the edit was empty.
        let unchanged = self.graph_change(graph) == AraGraphChange::Unchanged;
        if requests.is_empty() {
            if !unchanged {
                let created = self.build_graph(context, graph, &mut [])?;
                if let Ok(document) = self.document_mut() {
                    notify_after_edit(document);
                }
                self.request_note_analysis(&created);
            }
            return Ok(Vec::new());
        }

        // Everything about each restore is settled before the graph is
        // touched, so one that cannot run never gets in the build's way.
        let prepared: Vec<AraResult<PreparedRestore>> = requests
            .iter()
            .map(|request| self.prepare_restore(request, &graph.sources, &graph.regions))
            .collect();
        if unchanged {
            // Nothing to build: the restores get an edit cycle of their own.
            return Ok(self.restore_all_in_own_cycle(requests, prepared));
        }
        if self.generation < ApiGeneration::V2Final {
            // Before ARA 2 Final a restore is a scope of its own, which cannot
            // share the build's edit cycle.
            let created = self.build_graph(context, graph, &mut [])?;
            // The build's own reports go first, so none of them is taken for
            // analysis during a restore.
            if let Ok(document) = self.document_mut() {
                notify_after_edit(document);
            }
            let reports = self.restore_all_in_own_cycle(requests, prepared);
            self.request_note_analysis(&created);
            return Ok(reports);
        }

        let runs: Vec<AraResult<RestoreRun>> = prepared
            .into_iter()
            .map(|prepared| prepared.map(|prepared| self.begin_restore(prepared)))
            .collect();
        let mut steps: Vec<InEditRestore<'_, '_>> = Vec::with_capacity(runs.len());
        let mut step_of: Vec<Option<usize>> = Vec::with_capacity(runs.len());
        for (request, run) in requests.iter().zip(&runs) {
            match run {
                Ok(run) => {
                    step_of.push(Some(steps.len()));
                    steps.push(InEditRestore {
                        request,
                        filter: run.filter.as_ref(),
                        sequence: self.next_archive,
                        result: None,
                    });
                    self.next_archive += 1;
                }
                Err(_) => step_of.push(None),
            }
        }
        let created = self.build_graph(context, graph, &mut steps)?;
        let mut results: Vec<Option<AraResult<()>>> =
            steps.into_iter().map(|step| step.result).collect();
        let concluded: Vec<AraResult<(RestoreRun, AraResult<()>)>> = runs
            .into_iter()
            .zip(step_of)
            .map(|(run, step)| {
                run.map(|run| {
                    let result = step
                        .and_then(|index| results[index].take())
                        .unwrap_or_else(|| Err(AraHostError::invalid("the restore did not run")));
                    (run, result)
                })
            })
            .collect();
        let reports = self.conclude_restores(requests, concluded);
        self.request_note_analysis(&created);
        Ok(reports)
    }

    /// Brings the document's graph in line with `graph` in one edit cycle,
    /// restoring each of `restores` into it, in order, just before the cycle
    /// ends, and returns the sources it created.
    ///
    /// Leaves delivering the plug-in's model updates to the caller, which may
    /// have to read content first.
    fn build_graph(
        &mut self,
        context: MusicalContextHandle,
        graph: &AraGraph,
        restores: &mut [InEditRestore<'_, '_>],
    ) -> AraResult<Vec<(AraSourceKey, AudioSourceHandle)>> {
        let supported = self.info.supported_transforms;
        let doomed_clips = doomed_clips(
            self.clips.iter().map(|(key, entry)| (key, &entry.desc)),
            &graph.regions,
        );

        // An open editor view is holding the regions it was last shown. Before
        // any of them is destroyed, show it the set without them, so no view
        // keeps a reference to a region that no longer exists.
        if !doomed_clips.is_empty() {
            let doomed: HashSet<&AraClipKey> = doomed_clips.iter().collect();
            let wanted_sequences: HashSet<&AraTrackKey> = graph
                .sequences
                .iter()
                .map(|sequence| &sequence.key)
                .collect();
            let remaining: Vec<AraClipKey> = self
                .clips
                .keys()
                .filter(|key| !doomed.contains(key))
                .cloned()
                .collect();
            let tracks: Vec<AraTrackKey> = self
                .sequences
                .keys()
                .filter(|key| wanted_sequences.contains(key))
                .cloned()
                .collect();
            let renderers: Vec<AraRendererId> = self.renderers.keys().copied().collect();
            for renderer in renderers {
                if let Err(error) = self.notify_editor_selection(renderer, &remaining, &tracks) {
                    trace(&format!("editor selection before teardown: {error}"));
                }
            }
        }

        // One destructuring borrow: `document` is mutated through the edit scope
        // while the graph maps below are updated in the same pass, and the
        // borrow checker only allows that on disjoint fields.
        let Self {
            document,
            document_name,
            index,
            archives,
            analysis,
            sources,
            sequences,
            clips,
            renderers,
            ..
        } = self;
        let document = document
            .as_mut()
            .ok_or_else(|| AraHostError::invalid("ARA session is already closed"))?;
        if document.is_poisoned() {
            return Err(AraHostError::Poisoned);
        }

        let wanted_sources: HashSet<&AraSourceKey> =
            graph.sources.iter().map(|source| &source.key).collect();
        let wanted_sequences: HashSet<&AraTrackKey> = graph
            .sequences
            .iter()
            .map(|sequence| &sequence.key)
            .collect();

        // A playback region may not be destroyed while a renderer still holds
        // it, so drop those RAII assignments first — in **both** roles. The
        // editor-renderer assignment used to survive: the region was destroyed
        // under it, and dropping it afterwards (in `set_renderer_regions`)
        // handed the plug-in a dangling region in `removePlaybackRegion`.
        // Cutting or deleting a clip on an ARA track with the ARA panel open
        // took the app down that way.
        for key in &doomed_clips {
            for renderer in renderers.values_mut() {
                renderer.assignments.remove(key);
                renderer.editor_assignments.remove(key);
            }
        }

        let mut edit = document.edit().map_err(map_error)?;

        // The document name is what the plug-in shows in its own title bar, so
        // it follows the track it belongs to rather than being set once at open.
        if let Some(name) = graph.name.as_deref() {
            if document_name.as_deref() != Some(name) {
                edit.update_document_properties(
                    DocumentProperties::new(Some(name)).map_err(map_error)?,
                )
                .map_err(map_error)?;
                *document_name = Some(name.to_owned());
            }
        }

        // Teardown is leaf-first: regions, then modifications, then the sources
        // and sequences they referenced. A clip that moved to another source or
        // track goes here too and is created again below.
        for key in &doomed_clips {
            if let Some(entry) = clips.remove(key) {
                edit.destroy_playback_region(entry.region)
                    .map_err(map_error)?;
                edit.destroy_audio_modification(entry.modification)
                    .map_err(map_error)?;
                index.remove_region(entry.region_address);
                index.remove_modification(entry.modification_address);
            }
        }

        let stale_sources: Vec<AraSourceKey> = sources
            .keys()
            .filter(|key| !wanted_sources.contains(key))
            .cloned()
            .collect();
        for key in stale_sources {
            let Some(entry) = sources.get(&key) else {
                continue;
            };
            let (handle, address) = (entry.handle, entry.address);
            // Access off before the source goes: the plug-in may be reading it
            // on an analysis thread, and disabling access is what makes it
            // close those readers. Destroying a source that is still readable
            // is an ARA API violation the SDK's controllers assert on.
            edit.set_audio_source_samples_access(handle, false)
                .map_err(map_error)?;
            if let Err(error) = edit.destroy_audio_source(handle) {
                // The plug-in still has the source, so the host keeps it too,
                // readable again; forgetting it here left a source the host
                // could never destroy, holding the clip's modification.
                let _ = edit.set_audio_source_samples_access(handle, true);
                return Err(map_error(error));
            }
            sources.remove(&key);
            index.remove_source(address);
            analysis.forget(&key);
        }

        let stale_sequences: Vec<AraTrackKey> = sequences
            .keys()
            .filter(|key| !wanted_sequences.contains(key))
            .cloned()
            .collect();
        for key in stale_sequences {
            if let Some(entry) = sequences.remove(&key) {
                edit.destroy_region_sequence(entry.handle)
                    .map_err(map_error)?;
            }
        }

        // Build-up runs in dependency order: sequences and sources first, then
        // the modifications and regions that reference them.
        let context_ref = edit.musical_context_ref(context).map_err(map_error)?;
        for desc in &graph.sequences {
            let properties = RegionSequenceProperties::new(
                Some(desc.name.as_str()),
                desc.order_index,
                context_ref,
                to_color(desc.color)?,
            )
            .map_err(map_error)?;
            match sequences.get_mut(&desc.key) {
                Some(entry) if entry.desc == *desc => {}
                Some(entry) => {
                    edit.update_region_sequence(entry.handle, properties)
                        .map_err(map_error)?;
                    entry.desc = desc.clone();
                }
                None => {
                    let handle = edit.create_region_sequence(properties).map_err(map_error)?;
                    sequences.insert(
                        desc.key.clone(),
                        SequenceEntry {
                            handle,
                            desc: desc.clone(),
                        },
                    );
                }
            }
        }

        let mut new_sources: Vec<(AraSourceKey, AudioSourceHandle)> = Vec::new();
        for desc in &graph.sources {
            let properties = AudioSourceProperties::new(
                Some(desc.name.as_str()),
                &ara_persistent_id(desc.key.as_str()),
                desc.frame_count,
                desc.sample_rate,
                desc.channel_count,
                false.into(),
            )
            .map_err(map_error)?;
            match sources.get_mut(&desc.key) {
                Some(entry) if entry.desc == *desc => {}
                Some(entry) => {
                    // ARA lets a source's length, rate and channel count change
                    // only while the plug-in cannot read it.
                    let reshaped = entry.desc.frame_count != desc.frame_count
                        || entry.desc.sample_rate != desc.sample_rate
                        || entry.desc.channel_count != desc.channel_count;
                    if reshaped {
                        edit.set_audio_source_samples_access(entry.handle, false)
                            .map_err(map_error)?;
                    }
                    edit.update_audio_source(entry.handle, properties)
                        .map_err(map_error)?;
                    if reshaped {
                        edit.set_audio_source_samples_access(entry.handle, true)
                            .map_err(map_error)?;
                    }
                    entry.desc = desc.clone();
                }
                None => {
                    let handle = edit.create_audio_source(properties).map_err(map_error)?;
                    // Index before anything else runs: the plug-in calls
                    // `createAudioReaderForSource` synchronously from inside
                    // `createAudioSource`, so an identity registered after the
                    // edit closes arrives too late and the plug-in's first --
                    // often only -- request to read the audio is refused.
                    let address =
                        edit.audio_source_ref(handle).map_err(map_error)?.as_raw() as usize;
                    index.insert_source(address, desc.key.clone());
                    // The plug-in may only read samples once access is enabled;
                    // without this every analysis request comes back empty.
                    edit.set_audio_source_samples_access(handle, true)
                        .map_err(map_error)?;
                    trace(&format!(
                        "created audio source '{}' frames={} rate={} channels={} (access enabled)",
                        desc.key.as_str(),
                        desc.frame_count,
                        desc.sample_rate,
                        desc.channel_count
                    ));
                    new_sources.push((desc.key.clone(), handle));
                    sources.insert(
                        desc.key.clone(),
                        SourceEntry {
                            handle,
                            address,
                            desc: desc.clone(),
                        },
                    );
                }
            }
        }

        for desc in &graph.regions {
            let transform = desc.transform.intersect(supported);
            let sequence = sequences
                .get(&desc.track)
                .ok_or_else(|| AraHostError::invalid("region references an unbuilt track"))?
                .handle;
            let sequence_ref = edit.region_sequence_ref(sequence).map_err(map_error)?;
            let region_properties = PlaybackRegionProperties::for_ara2(
                transform_flags(transform).bits() as i32,
                desc.start_in_modification,
                desc.duration_in_modification,
                desc.start_in_playback,
                desc.duration_in_playback,
                sequence_ref,
                Some(desc.name.as_str()),
                to_color(desc.color)?,
            )
            .map_err(map_error)?;

            match clips.get_mut(&desc.key) {
                Some(entry) if entry.desc == *desc => {}
                // Same source and track: `doomed_clips` took every other one.
                Some(entry) => {
                    edit.update_playback_region(entry.region, region_properties)
                        .map_err(map_error)?;
                    entry.desc = desc.clone();
                }
                None => {
                    let source = sources
                        .get(&desc.source)
                        .ok_or_else(|| {
                            AraHostError::invalid("region references an unbuilt audio source")
                        })?
                        .handle;
                    let modification_properties = AudioModificationProperties::new(
                        Some(desc.name.as_str()),
                        &ara_persistent_id(desc.key.as_str()),
                    )
                    .map_err(map_error)?;
                    let modification = edit
                        .create_audio_modification(source, modification_properties)
                        .map_err(map_error)?;
                    let modification_address = edit
                        .audio_modification_ref(modification)
                        .map_err(map_error)?
                        .as_raw() as usize;
                    index.insert_modification(modification_address, desc.key.clone());
                    let region = edit
                        .create_playback_region(modification, region_properties)
                        .map_err(map_error)?;
                    let region_address = edit
                        .playback_region_ref(region)
                        .map_err(map_error)?
                        .as_raw() as usize;
                    index.insert_region(region_address, desc.key.clone());
                    clips.insert(
                        desc.key.clone(),
                        ClipEntry {
                            modification,
                            modification_address,
                            region,
                            region_address,
                            desc: desc.clone(),
                        },
                    );
                }
            }
        }

        // The graph is whole: its archived state goes in now, before the
        // cycle ends. Whatever the plug-in says, the cycle still ends below,
        // so a refused archive never leaves the document stuck in editing.
        for step in restores.iter_mut() {
            let restored = with_archive(
                archives,
                step.sequence,
                step.request.archive_id,
                step.request.bytes,
                |token| edit.restore_objects_from_archive(token, step.filter),
            );
            step.result = Some(restored.map_err(map_error));
        }

        edit.finish().map_err(map_error)?;
        Ok(new_sources)
    }

    /// Asks for note analysis on sources the host just created, unless they
    /// already have content (a restore supplied it).
    ///
    /// A plug-in is entitled to wait for the host to ask before spending the
    /// CPU, and one that does shows an empty editor until then. Asking before
    /// a restore instead left Melodyne reporting the analysis incomplete for
    /// good. `Unsupported` only means this plug-in does not analyse notes.
    fn request_note_analysis(&mut self, created: &[(AraSourceKey, AudioSourceHandle)]) {
        for (key, handle) in created {
            if self
                .notes_grade(*handle)
                .is_some_and(|grade| grade >= GRADE_DETECTED)
            {
                trace(&format!(
                    "'{}' has note content already; no analysis requested",
                    key.as_str()
                ));
                continue;
            }
            let Ok(document) = self.document_mut() else {
                return;
            };
            match document.request_audio_source_content_analysis::<Notes>(*handle) {
                Ok(()) => trace("requested note analysis for a new audio source"),
                Err(AraError::Unsupported(_)) => {
                    trace("plug-in does not analyse notes; skipping the request")
                }
                Err(error) => trace(&format!("note analysis request failed: {error}")),
            }
        }
    }

    /// The plug-in's note-content grade for one source, or `None` when it
    /// does not analyse notes or the query fails. Outside editing only.
    fn notes_grade(&mut self, handle: AudioSourceHandle) -> Option<i32> {
        if !self.notes_analysable {
            return None;
        }
        let document = self.document_mut().ok()?;
        document
            .audio_source_content_grade::<Notes>(handle)
            .ok()
            .map(|grade| grade.as_raw())
    }

    /// Whether the plug-in reports note analysis of one source incomplete,
    /// or `None` when it does not analyse notes or the query fails.
    fn notes_incomplete(&mut self, handle: AudioSourceHandle) -> Option<bool> {
        if !self.notes_analysable {
            return None;
        }
        let document = self.document_mut().ok()?;
        document
            .audio_source_content_analysis_incomplete::<Notes>(handle)
            .ok()
    }

    /// Whether note analysis of `key` is still incomplete, as the plug-in
    /// reports it right now; `None` when the source is not in the graph or
    /// the plug-in does not analyse notes.
    pub(crate) fn analysis_incomplete(&mut self, key: &AraSourceKey) -> AraResult<Option<bool>> {
        let Some(handle) = self.sources.get(key).map(|entry| entry.handle) else {
            return Ok(None);
        };
        if !self.notes_analysable {
            return Ok(None);
        }
        match self
            .document_mut()?
            .audio_source_content_analysis_incomplete::<Notes>(handle)
        {
            Ok(incomplete) => Ok(Some(incomplete)),
            Err(AraError::Unsupported(_)) => Ok(None),
            Err(error) => Err(map_error(error)),
        }
    }

    /// # Safety
    ///
    /// See [`crate::AraSession::bind_renderer`].
    pub(crate) unsafe fn bind_renderer(
        &mut self,
        component: *mut c_void,
        roles: AraRoles,
    ) -> AraResult<AraRendererId> {
        if component.is_null() {
            return Err(AraHostError::invalid("null VST3 component for ARA binding"));
        }
        // SAFETY: the caller guarantees a live, initialized VST3 component
        // identity that outlives this session's use of the binding.
        let plugin = unsafe { Vst3HostPlugin::discover(component) }.map_err(map_error)?;

        let known = companion_roles(AraRoles::ALL);
        let assigned = companion_roles(roles);
        let Self {
            document,
            renderers,
            next_renderer,
            ..
        } = self;
        let document = document
            .as_mut()
            .ok_or_else(|| AraHostError::invalid("ARA session is already closed"))?;
        if document.is_poisoned() {
            return Err(AraHostError::Poisoned);
        }
        let controller = document.controller_ref();

        // SAFETY: `controller` belongs to this session's factory, which is the
        // same plug-in the caller instantiated, and outlives the binding.
        let instance = unsafe {
            plugin.bind(
                controller,
                known,
                assigned,
                // ARA 1 style binding takes every role at once; only accept that
                // fallback when the caller actually asked for every role.
                roles == AraRoles::ALL,
            )
        }
        .map_err(map_error)?;

        // SAFETY: the instance was produced by the binding above for this exact
        // document controller and role set, and `plugin` keeps it alive.
        let extension = unsafe {
            document.bind_extension(
                instance,
                extension_roles(AraRoles::ALL),
                extension_roles(roles),
            )
        }
        .map_err(map_error)?;

        let id = AraRendererId(*next_renderer);
        *next_renderer += 1;
        renderers.insert(
            id,
            Renderer {
                _plugin: plugin,
                extension,
                roles,
                assignments: HashMap::new(),
                editor_assignments: HashMap::new(),
            },
        );
        Ok(id)
    }

    pub(crate) fn unbind_renderer(&mut self, renderer: AraRendererId) -> AraResult<()> {
        self.renderers
            .remove(&renderer)
            .map(|_| ())
            .ok_or_else(|| AraHostError::invalid("unknown ARA renderer"))
    }

    pub(crate) fn set_renderer_regions(
        &mut self,
        renderer: AraRendererId,
        clips: &[AraClipKey],
    ) -> AraResult<()> {
        let wanted: HashSet<&AraClipKey> = clips.iter().collect();
        let Self {
            document,
            clips: graph_clips,
            renderers,
            ..
        } = self;
        let document = document
            .as_ref()
            .ok_or_else(|| AraHostError::invalid("ARA session is already closed"))?;
        let Some(entry) = renderers.get_mut(&renderer) else {
            return Err(AraHostError::invalid("unknown ARA renderer"));
        };
        if !entry.roles.playback_renderer && !entry.roles.editor_renderer {
            return Err(AraHostError::invalid(
                "this ARA instance holds no renderer role",
            ));
        }

        // Dropping an assignment removes the region from the plug-in.
        entry.assignments.retain(|key, _| wanted.contains(key));
        entry
            .editor_assignments
            .retain(|key, _| wanted.contains(key));

        // Every role the instance holds gets the same region set. Assigning only
        // the playback renderer leaves the plug-in's editor with nothing to
        // work on even though the audio is already routed through it.
        let roles: [(bool, RendererRole); 2] = [
            (entry.roles.playback_renderer, RendererRole::Playback),
            (entry.roles.editor_renderer, RendererRole::Editor),
        ];
        for (held, role) in roles {
            if !held {
                continue;
            }
            for key in clips {
                let existing = match role {
                    RendererRole::Playback => &entry.assignments,
                    RendererRole::Editor => &entry.editor_assignments,
                };
                if existing.contains_key(key) {
                    continue;
                }
                let region = graph_clips
                    .get(key)
                    .ok_or_else(|| AraHostError::invalid("clip is not in the ARA graph"))?
                    .region;
                let assignment = entry
                    .extension
                    .assign_playback_region(document, role, region)
                    .map_err(map_error)?;
                match role {
                    RendererRole::Playback => entry.assignments.insert(key.clone(), assignment),
                    RendererRole::Editor => {
                        entry.editor_assignments.insert(key.clone(), assignment)
                    }
                };
            }
        }
        Ok(())
    }

    /// Tells a bound instance which regions its editor is looking at.
    ///
    /// ARA 2 splits "the plug-in renders this region" from "the user is editing
    /// this region": the first is a renderer assignment, the second is an
    /// editor-view selection. A plug-in that was never told the second opens an
    /// editor on an empty canvas even though the document is fully built, so
    /// this is published every time the graph is rebuilt.
    pub(crate) fn notify_editor_selection(
        &mut self,
        renderer: AraRendererId,
        clips: &[AraClipKey],
        tracks: &[AraTrackKey],
    ) -> AraResult<()> {
        let Self {
            document,
            clips: graph_clips,
            sequences,
            renderers,
            ..
        } = self;
        let document = document
            .as_ref()
            .ok_or_else(|| AraHostError::invalid("ARA session is already closed"))?;
        let entry = renderers
            .get(&renderer)
            .ok_or_else(|| AraHostError::invalid("unknown ARA renderer"))?;
        if !entry.roles.editor_view {
            // Nothing to publish to: the instance was bound without the role.
            return Ok(());
        }
        let regions: Vec<_> = clips
            .iter()
            .filter_map(|key| graph_clips.get(key).map(|clip| clip.region))
            .collect();
        let sequence_handles: Vec<_> = tracks
            .iter()
            .filter_map(|key| sequences.get(key).map(|entry| entry.handle))
            .collect();
        entry
            .extension
            .notify_selection(document, &regions, &sequence_handles, None)
            .map_err(map_error)
    }

    pub(crate) fn set_rendering(
        &mut self,
        renderer: AraRendererId,
        enabled: bool,
    ) -> AraResult<()> {
        self.renderers
            .get(&renderer)
            .ok_or_else(|| AraHostError::invalid("unknown ARA renderer"))?
            .extension
            .set_rendering(enabled)
            .map_err(map_error)
    }

    pub(crate) fn store_archive(&mut self) -> AraResult<AraStoredArchive> {
        let token = Box::new(ArchiveToken {
            _sequence: self.next_archive,
        });
        self.next_archive += 1;
        let address = std::ptr::from_ref(token.as_ref()) as usize;
        let archive_id = self.info.document_archive_id.clone();
        self.archives.open(
            address,
            ArchiveSlot {
                bytes: Vec::new(),
                archive_id: Some(archive_id),
            },
        );

        let document = self.document_mut()?;
        // ARA 2 Final replaced whole-document persistence with object
        // persistence; store with the call the restore below will read with.
        let result = if document.generation() >= ApiGeneration::V2Final {
            document.store_objects_to_archive(token.as_ref(), None)
        } else {
            document.store_document_to_archive(token.as_ref())
        };
        let slot = self.archives.take(address);
        drop(token);
        result.map_err(map_error)?;
        // No filter stores every object in the graph, so the identity read
        // from the same maps in the same call describes exactly these bytes.
        Ok(AraStoredArchive {
            bytes: slot.map(|slot| slot.bytes).unwrap_or_default(),
            identity: self.archive_identity(),
        })
    }

    /// What a store made now holds: every source and modification the host
    /// published, under the IDs it published them with, and the key the
    /// plug-in was offered.
    fn archive_identity(&self) -> AraArchiveIdentity {
        let mut sources: Vec<AraArchivedSource> = self
            .sources
            .values()
            .map(|entry| AraArchivedSource {
                persistent_id: ara_persistent_id(entry.desc.key.as_str()).into_owned(),
                key: entry.desc.key.clone(),
                sample_rate: entry.desc.sample_rate,
                frame_count: entry.desc.frame_count,
                channel_count: entry.desc.channel_count,
            })
            .collect();
        sources.sort_by(|a, b| a.key.cmp(&b.key));
        let mut modifications: Vec<AraArchivedModification> = self
            .clips
            .values()
            .map(|entry| AraArchivedModification {
                persistent_id: ara_persistent_id(entry.desc.key.as_str()).into_owned(),
                clip: entry.desc.key.clone(),
                source_persistent_id: ara_persistent_id(entry.desc.source.as_str()).into_owned(),
            })
            .collect();
        modifications.sort_by(|a, b| a.clip.cmp(&b.clip));
        AraArchiveIdentity {
            sources,
            modifications,
            keys: Some(self.published_key_descriptor()),
        }
    }

    /// The key signatures the plug-in is offered right now.
    fn published_key_descriptor(&self) -> AraKeyDescriptor {
        let keys = match &self.published {
            Some(timeline) if offers_harmonic_content(self.generation) => timeline.keys.as_slice(),
            _ => &[],
        };
        AraKeyDescriptor::of(keys)
    }

    /// Restores an archive into the graph as it stands, in an edit cycle of
    /// its own.
    pub(crate) fn restore(&mut self, request: AraRestoreRequest<'_>) -> AraRestoreReport {
        let mut sources: Vec<AraAudioSourceDesc> = self
            .sources
            .values()
            .map(|entry| entry.desc.clone())
            .collect();
        sources.sort_by(|a, b| a.key.cmp(&b.key));
        let mut regions: Vec<AraPlaybackRegionDesc> = self
            .clips
            .values()
            .map(|entry| entry.desc.clone())
            .collect();
        regions.sort_by(|a, b| a.key.cmp(&b.key));
        let prepared = self.prepare_restore(&request, &sources, &regions);
        let requests = [request];
        self.restore_all_in_own_cycle(&requests, vec![prepared])
            .pop()
            .unwrap_or_else(|| {
                AraRestoreReport::not_run(AraHostError::invalid("the restore did not run"))
            })
    }

    /// Settles a restore against the graph `sources` and `regions` describe:
    /// the archive must be one this plug-in reads, the placement must be
    /// valid, and a filter (a map, or part of an archive) needs ARA 2 Final.
    fn prepare_restore(
        &self,
        request: &AraRestoreRequest<'_>,
        sources: &[AraAudioSourceDesc],
        regions: &[AraPlaybackRegionDesc],
    ) -> AraResult<PreparedRestore> {
        if !self.info.can_restore_archive(request.archive_id) {
            return Err(AraHostError::unsupported(format!(
                "archive '{}' cannot be restored by '{}'",
                request.archive_id, self.info.document_archive_id
            )));
        }
        let plan = plan_restore(
            request.identity,
            request.map,
            request.scope,
            sources,
            regions,
        )?;
        let filter = match &plan.filter {
            None => None,
            Some(_) if self.generation < ApiGeneration::V2Final => {
                return Err(AraHostError::unsupported(
                    "restoring through a map, or part of an archive, needs ARA 2 Final or later",
                ));
            }
            Some(planned) => Some(restore_filter(planned)?),
        };
        Ok(PreparedRestore { plan, filter })
    }

    /// Reads what judges a restore before it runs: each target's note grade,
    /// and how much analysis progress it has reported.
    ///
    /// A target the host has not created yet is created by the very edit the
    /// restore runs in, where no analysis has happened: its grade is initial.
    fn begin_restore(&mut self, prepared: PreparedRestore) -> RestoreRun {
        let mut before = Vec::with_capacity(prepared.plan.targets.len());
        let mut progress = Vec::with_capacity(prepared.plan.targets.len());
        for target in &prepared.plan.targets {
            let grade = match self.sources.get(&target.key).map(|entry| entry.handle) {
                Some(handle) => self.notes_grade(handle),
                None => self.notes_analysable.then_some(GRADE_INITIAL),
            };
            before.push(grade);
            progress.push(self.analysis.reports(&target.key));
        }
        RestoreRun {
            plan: prepared.plan,
            filter: prepared.filter,
            before,
            progress,
        }
    }

    /// Runs prepared restores against the graph as it stands and reports
    /// each, in order.
    ///
    /// ARA 2 Final and later restore objects inside an ordinary edit cycle,
    /// here one cycle for all of them. Earlier plug-ins get the legacy
    /// restore scope, one per restore, which (in every ARA-library
    /// controller) is an edit cycle that restores all live objects when it
    /// ends. The legacy call is refused outright at 2 Final and later —
    /// which, since this host negotiates the newest generation first and
    /// Apple Silicon allows nothing older, used to be every session: saved
    /// ARA edits were silently dropped on every reopen.
    fn restore_all_in_own_cycle(
        &mut self,
        requests: &[AraRestoreRequest<'_>],
        prepared: Vec<AraResult<PreparedRestore>>,
    ) -> Vec<AraRestoreReport> {
        let runs: Vec<AraResult<RestoreRun>> = prepared
            .into_iter()
            .map(|prepared| prepared.map(|prepared| self.begin_restore(prepared)))
            .collect();
        let archives = Arc::clone(&self.archives);
        let sequence = self.next_archive;
        self.next_archive += requests.len() as u64;
        let mut results: Vec<Option<AraResult<()>>> = Vec::with_capacity(runs.len());
        match self.document_mut() {
            Err(error) => {
                results.extend(runs.iter().map(|_| Some(Err(error.clone()))));
            }
            Ok(document) if document.generation() >= ApiGeneration::V2Final => {
                match document.edit() {
                    Err(error) => {
                        let error = map_error(error);
                        results.extend(runs.iter().map(|_| Some(Err(error.clone()))));
                    }
                    Ok(mut edit) => {
                        for (index, (request, run)) in requests.iter().zip(&runs).enumerate() {
                            let Ok(run) = run else {
                                results.push(None);
                                continue;
                            };
                            let restored = with_archive(
                                &archives,
                                sequence + index as u64,
                                request.archive_id,
                                request.bytes,
                                |token| {
                                    edit.restore_objects_from_archive(token, run.filter.as_ref())
                                },
                            );
                            results.push(Some(restored.map_err(map_error)));
                        }
                        // End the cycle whatever the restores said, so a
                        // refused archive does not leave the document stuck
                        // in editing.
                        if let Err(error) = edit.finish() {
                            let error = map_error(error);
                            for result in results.iter_mut().flatten() {
                                if result.is_ok() {
                                    *result = Err(error.clone());
                                }
                            }
                        }
                    }
                }
            }
            Ok(document) => {
                for (index, (request, run)) in requests.iter().zip(&runs).enumerate() {
                    if run.is_err() {
                        results.push(None);
                        continue;
                    }
                    let restored = with_archive(
                        &archives,
                        sequence + index as u64,
                        request.archive_id,
                        request.bytes,
                        |token| {
                            document
                                .restore_document_from_archive(token)
                                .and_then(|edit| edit.finish())
                        },
                    );
                    results.push(Some(restored.map_err(map_error)));
                }
            }
        }
        let concluded = runs
            .into_iter()
            .zip(results)
            .map(|(run, result)| {
                run.map(|run| {
                    let result = result
                        .unwrap_or_else(|| Err(AraHostError::invalid("the restore did not run")));
                    (run, result)
                })
            })
            .collect();
        self.conclude_restores(requests, concluded)
    }

    /// Judges restores that have run, one report per request, then brings
    /// the key back in line.
    ///
    /// Grades are read first, straight after the edit cycle and before the
    /// plug-in may deliver anything: a plug-in that found nothing to restore
    /// starts analysing at the end of the cycle, and its progress reports,
    /// delivered next, catch an analysis fast enough to finish before the
    /// read. A restore that could not run is reported as not run. The
    /// harmony is re-synced once, when the key any of the archives was
    /// stored with is known and differs from today's.
    fn conclude_restores(
        &mut self,
        requests: &[AraRestoreRequest<'_>],
        runs: Vec<AraResult<(RestoreRun, AraResult<()>)>>,
    ) -> Vec<AraRestoreReport> {
        let handles: Vec<Vec<Option<AudioSourceHandle>>> = runs
            .iter()
            .map(|run| match run {
                Ok((run, _)) => run
                    .plan
                    .targets
                    .iter()
                    .map(|target| self.sources.get(&target.key).map(|entry| entry.handle))
                    .collect(),
                Err(_) => Vec::new(),
            })
            .collect();
        let after: Vec<Vec<Option<i32>>> = handles
            .iter()
            .map(|handles| {
                handles
                    .iter()
                    .map(|handle| handle.and_then(|handle| self.notes_grade(handle)))
                    .collect()
            })
            .collect();
        if let Ok(document) = self.document_mut() {
            notify_after_edit(document);
        }

        let current_keys = self.published_key_descriptor();
        let harmonic = offers_harmonic_content(self.generation);
        let mut reports = Vec::with_capacity(runs.len());
        let mut resync = Vec::with_capacity(runs.len());
        for (index, (request, run)) in requests.iter().zip(runs).enumerate() {
            let (run, result) = match run {
                Ok(run) => run,
                Err(error) => {
                    trace(&format!(
                        "restore of '{}' ({} bytes) did not run: {error}",
                        request.archive_id,
                        request.bytes.len()
                    ));
                    reports.push(AraRestoreReport::not_run(error));
                    resync.push(false);
                    continue;
                }
            };
            let mut sources = Vec::with_capacity(handles[index].len());
            for (target_index, target) in run.plan.targets.into_iter().enumerate() {
                let analysis_seen =
                    self.analysis.reports(&target.key) != run.progress[target_index];
                let analysis_incomplete =
                    handles[index][target_index].and_then(|handle| self.notes_incomplete(handle));
                sources.push(AraSourceRestore {
                    outcome: classify_source(
                        run.before[target_index],
                        after[index][target_index],
                        analysis_seen,
                        target.as_saved,
                    ),
                    key: target.key,
                    persistent_id: target.current_id,
                    archived_id: target.archived_id,
                    grade_before: run.before[target_index],
                    grade_after: after[index][target_index],
                    analysis_incomplete,
                });
            }
            let unplaced = !run.plan.unplaced_sources.is_empty()
                || !run.plan.unplaced_modifications.is_empty();
            let outcomes: Vec<_> = sources.iter().map(|source| source.outcome).collect();
            let outcome = overall_outcome(&outcomes, run.plan.expects, unplaced);
            resync.push(
                result.is_ok()
                    && needs_harmony_resync(
                        request.identity.and_then(|identity| identity.keys),
                        current_keys,
                        harmonic,
                    ),
            );
            trace(&format!(
                "restore of '{}' ({} bytes, filter={}, document data={}): {outcome:?}, \
                 error={:?}, sources={:?}, unplaced sources={:?}, unplaced modifications={:?}",
                request.archive_id,
                request.bytes.len(),
                run.filter.is_some(),
                run.plan
                    .filter
                    .as_ref()
                    .is_none_or(|filter| filter.document_data),
                result.as_ref().err(),
                sources
                    .iter()
                    .map(|source| (
                        source.persistent_id.as_str(),
                        source.outcome,
                        source.grade_before,
                        source.grade_after
                    ))
                    .collect::<Vec<_>>(),
                run.plan.unplaced_sources,
                run.plan.unplaced_modifications,
            ));
            reports.push(AraRestoreReport {
                error: result.err(),
                outcome,
                sources,
                unplaced_sources: run.plan.unplaced_sources,
                unplaced_modifications: run.plan.unplaced_modifications,
                harmony_resynced: false,
            });
        }
        if resync.iter().any(|needed| *needed) && self.resync_harmony() {
            for (report, needed) in reports.iter_mut().zip(resync) {
                report.harmony_resynced = needed;
            }
        }
        trace(&format!(
            "harmony resynced after the restores: {:?}",
            reports
                .iter()
                .map(|report| report.harmony_resynced)
                .collect::<Vec<_>>()
        ));
        reports
    }

    /// Tells the plug-in the harmony changed, after a restore whose archive
    /// was stored with a known key that differs from the project's now
    /// ([`needs_harmony_resync`]).
    ///
    /// The archive brings back each clip's copy of the key it was saved with
    /// (Melodyne keeps scales per audio modification, copied from the musical
    /// context), and the key may have changed since — while the plug-in was
    /// missing, say. The timeline itself is unchanged, so the diff in
    /// `set_musical_timeline` would never say so. With the same key the
    /// notification would only replace scales the user set, and with an
    /// unknown one the host cannot tell, so neither sends it. The restore
    /// itself stands either way, so a failure here is traced, not returned.
    fn resync_harmony(&mut self) -> bool {
        let Some(handle) = self.musical_context else {
            return false;
        };
        let Ok(document) = self.document_mut() else {
            return false;
        };
        let outcome = document.edit().and_then(|mut edit| {
            let updated = edit.update_musical_context_content(handle, None, HARMONY_CHANGED);
            // End the cycle whatever the update said.
            let finished = edit.finish();
            updated.and(finished)
        });
        match outcome {
            Ok(()) => {
                notify_after_edit(document);
                true
            }
            Err(error) => {
                trace(&format!("harmony re-sync after restore: {error}"));
                false
            }
        }
    }

    pub(crate) fn close(mut self) -> AraResult<()> {
        // Renderers must release their assignments before the graph they point
        // into is torn down.
        self.renderers.clear();
        let Some(document) = self.document.take() else {
            return Ok(());
        };
        document
            .close()
            .map_err(|error| AraHostError::Plugin(error.to_string()))
    }
}

impl HeldGraph for Session {
    fn document_name(&self) -> Option<&str> {
        self.document_name.as_deref()
    }

    fn counts(&self) -> (usize, usize, usize) {
        (self.sources.len(), self.sequences.len(), self.clips.len())
    }

    fn source(&self, key: &AraSourceKey) -> Option<&AraAudioSourceDesc> {
        self.sources.get(key).map(|entry| &entry.desc)
    }

    fn sequence(&self, key: &AraTrackKey) -> Option<&AraRegionSequenceDesc> {
        self.sequences.get(key).map(|entry| &entry.desc)
    }

    fn region(&self, key: &AraClipKey) -> Option<&AraPlaybackRegionDesc> {
        self.clips.get(key).map(|entry| &entry.desc)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Order is the whole point: assignments, then the document (which
        // destroys the controller), then the factory (which uninitializes ARA),
        // and only then the host services the controller was calling into.
        self.renderers.clear();
        drop(self.document.take());
        // SAFETY: both pointers came from `Box::into_raw` in `open`, are never
        // aliased elsewhere, and nothing borrows them once `document` is gone.
        unsafe {
            drop(Box::from_raw(self.factory_ptr));
            drop(Box::from_raw(self.services_ptr));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AraBarSignature, AraKeySignature, AraTempoEntry};

    fn timeline(bpm: f64, key_root: Option<i32>) -> AraMusicalTimeline {
        let mut intervals = [false; 12];
        for interval in [0, 2, 4, 5, 7, 9, 11] {
            intervals[interval] = true;
        }
        AraMusicalTimeline {
            tempo: vec![
                AraTempoEntry {
                    time_seconds: 0.0,
                    quarter_position: 0.0,
                },
                AraTempoEntry {
                    time_seconds: 60.0 / bpm,
                    quarter_position: 1.0,
                },
            ],
            bars: vec![AraBarSignature {
                numerator: 4,
                denominator: 4,
                quarter_position: 0.0,
            }],
            keys: key_root
                .map(|root_fifths| AraKeySignature {
                    root_fifths,
                    intervals,
                    name: None,
                    quarter_position: 0.0,
                })
                .into_iter()
                .collect(),
        }
    }

    fn scopes(update: ContextUpdate) -> ContentUpdateScopes {
        match update {
            ContextUpdate::Notify(scopes) => scopes,
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    const FINAL: ApiGeneration = ApiGeneration::V23Final;

    #[test]
    fn the_first_timeline_creates_the_context() {
        let next = timeline(120.0, Some(0));
        assert_eq!(
            plan_context_update(None, &next, FINAL),
            ContextUpdate::Create
        );
    }

    #[test]
    fn an_unchanged_timeline_needs_no_edit_at_all() {
        let prev = timeline(120.0, Some(0));
        assert_eq!(
            plan_context_update(Some(&prev), &prev.clone(), FINAL),
            ContextUpdate::Nothing
        );
    }

    #[test]
    fn a_key_change_is_harmonic_only() {
        let prev = timeline(120.0, Some(0));
        let next = timeline(120.0, Some(-1));
        let scopes = scopes(plan_context_update(Some(&prev), &next, FINAL));
        assert!(scopes.contains(ContentUpdateScopes::TIMING_REMAINS_UNCHANGED));
        assert!(!scopes.contains(ContentUpdateScopes::HARMONIC_REMAINS_UNCHANGED));
        assert!(scopes.contains(CONTEXT_NEVER_CHANGES));
        assert_eq!(scopes, HARMONY_CHANGED);
    }

    #[test]
    fn a_tempo_change_is_timing_only() {
        let prev = timeline(120.0, Some(0));
        let next = timeline(90.0, Some(0));
        let scopes = scopes(plan_context_update(Some(&prev), &next, FINAL));
        assert!(!scopes.contains(ContentUpdateScopes::TIMING_REMAINS_UNCHANGED));
        assert!(scopes.contains(ContentUpdateScopes::HARMONIC_REMAINS_UNCHANGED));
        assert!(scopes.contains(CONTEXT_NEVER_CHANGES));
    }

    #[test]
    fn clearing_the_key_is_a_harmonic_change() {
        let prev = timeline(120.0, Some(3));
        let next = timeline(120.0, None);
        assert_eq!(
            scopes(plan_context_update(Some(&prev), &next, FINAL)),
            HARMONY_CHANGED
        );
    }

    #[test]
    fn tempo_and_key_together_change_both_scopes() {
        let prev = timeline(120.0, Some(3));
        let next = timeline(90.0, Some(-2));
        assert_eq!(
            scopes(plan_context_update(Some(&prev), &next, FINAL)),
            CONTEXT_NEVER_CHANGES
        );
    }

    #[test]
    fn a_draft_generation_never_sees_keys_or_the_harmonic_flag() {
        let draft = ApiGeneration::V2Draft;
        let prev = timeline(120.0, Some(0));
        assert_eq!(
            plan_context_update(Some(&prev), &timeline(120.0, Some(-1)), draft),
            ContextUpdate::Nothing
        );
        let scopes = scopes(plan_context_update(
            Some(&prev),
            &timeline(90.0, Some(-1)),
            draft,
        ));
        assert!(!scopes.contains(ContentUpdateScopes::HARMONIC_REMAINS_UNCHANGED));
        assert_eq!(scopes, CONTEXT_NEVER_CHANGES);
    }

    const THAI_PATH: &str =
        "/Users/doppio/Documents/Futureboard Studio/Projects/โปรเจกต์ไม่มีชื่อ-1/Recordings/เสียง.wav";

    #[test]
    fn published_ids_pass_the_bridge_where_raw_keys_did_not() {
        // What failed a whole Thai-named project's ARA track.
        assert!(AudioSourceProperties::new(None, THAI_PATH, 1, 44_100.0, 1, false.into()).is_err());

        for key in [
            THAI_PATH,
            "Assets/Audio/1.wav",
            "clip-1",
            "",
            "a\0b",
            "fbx:x",
            "🎵",
        ] {
            let id = ara_persistent_id(key);
            AudioSourceProperties::new(Some("n"), &id, 1, 44_100.0, 1, false.into())
                .unwrap_or_else(|error| panic!("source ID for {key:?}: {error}"));
            AudioModificationProperties::new(None, &id)
                .unwrap_or_else(|error| panic!("modification ID for {key:?}: {error}"));
            RestoreFilter::builder()
                .audio_source(id.clone().into_owned(), id.clone().into_owned())
                .build()
                .unwrap_or_else(|error| panic!("filter ID for {key:?}: {error}"));
        }
    }

    #[test]
    fn a_planned_filter_restores_document_data_and_identity_pairs() {
        let planned = PlannedFilter {
            document_data: true,
            sources: vec![(
                "/Users/doppio/Documents/CodeProject/1.wav".to_owned(),
                "Assets/Audio/1.wav".to_owned(),
            )],
            modifications: vec![("clip-1".to_owned(), "clip-1".to_owned())],
        };
        let filter = restore_filter(&planned).unwrap();
        assert!(filter.includes_document_data());
        assert_eq!(filter.audio_sources().len(), 1);
        assert_eq!(
            filter.audio_sources()[0].archive_id(),
            "/Users/doppio/Documents/CodeProject/1.wav"
        );
        assert_eq!(filter.audio_sources()[0].current_id(), "Assets/Audio/1.wav");
        assert_eq!(filter.audio_modifications().len(), 1);
        assert_eq!(filter.audio_modifications()[0].archive_id(), "clip-1");
        assert_eq!(filter.audio_modifications()[0].current_id(), "clip-1");

        // A filter the bridge would refuse is refused here, as invalid.
        let duplicate = PlannedFilter {
            document_data: true,
            sources: vec![
                ("a".to_owned(), "x".to_owned()),
                ("b".to_owned(), "x".to_owned()),
            ],
            modifications: Vec::new(),
        };
        assert!(matches!(
            restore_filter(&duplicate),
            Err(AraHostError::Invalid(_))
        ));

        // The later part of a partial restore leaves the document data out.
        let later = PlannedFilter {
            document_data: false,
            sources: vec![("b.wav".to_owned(), "b.wav".to_owned())],
            modifications: vec![("clip-2".to_owned(), "clip-2".to_owned())],
        };
        let filter = restore_filter(&later).unwrap();
        assert!(!filter.includes_document_data());
        assert_eq!(filter.audio_sources().len(), 1);
        // A part with nothing but the document data is a valid filter too.
        let data_only = PlannedFilter {
            document_data: true,
            ..PlannedFilter::default()
        };
        let filter = restore_filter(&data_only).unwrap();
        assert!(filter.includes_document_data());
        assert!(filter.audio_sources().is_empty() && filter.audio_modifications().is_empty());
    }

    fn region(key: &str, source: &str, track: &str) -> AraPlaybackRegionDesc {
        AraPlaybackRegionDesc {
            key: key.into(),
            source: source.into(),
            track: track.into(),
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
    fn a_clip_that_moves_to_other_audio_or_another_track_is_rebuilt() {
        let held = [
            region("kept", "a.wav", "track-1"),
            region("moved-later", "a.wav", "track-1"),
            region("retargeted", "/abs/b.wav", "track-1"),
            region("rehomed", "a.wav", "track-1"),
            region("deleted", "a.wav", "track-1"),
        ];
        let mut moved = region("moved-later", "a.wav", "track-1");
        moved.start_in_playback = 4.0;
        let wanted = [
            region("kept", "a.wav", "track-1"),
            moved,
            region("retargeted", "Assets/Audio/b.wav", "track-1"),
            region("rehomed", "a.wav", "track-2"),
        ];
        let doomed = doomed_clips(held.iter().map(|desc| (&desc.key, desc)), &wanted);
        assert_eq!(
            doomed,
            vec![
                AraClipKey::from("deleted"),
                AraClipKey::from("rehomed"),
                AraClipKey::from("retargeted"),
            ]
        );
    }

    #[test]
    fn harmonic_content_starts_at_ara_2_final() {
        assert!(!offers_harmonic_content(ApiGeneration::V2Draft));
        assert!(offers_harmonic_content(ApiGeneration::V2Final));
        assert!(offers_harmonic_content(ApiGeneration::V2xDraft));
        assert!(offers_harmonic_content(ApiGeneration::V23Final));
    }
}
