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
    ContentUpdateScopes, DocumentProperties, MusicalContextProperties, Notes,
    PlaybackRegionProperties, PlaybackTransformationFlags, RegionSequenceProperties,
};
use ara2_bridge_host::{
    AudioModificationHandle, AudioSourceHandle, DocumentSession, ExtensionController,
    ExtensionRoles, HostServices, HostServicesBuilder, LoadedFactory, MusicalContextHandle,
    PlaybackRegionAssignment, PlaybackRegionHandle, RegionSequenceHandle, RendererRole,
};

use crate::AraSessionConfig;
use crate::error::{AraHostError, AraResult};
use crate::info::{AraFactoryInfo, AraRendererId, AraRoles};
use crate::model::{
    AraAudioSourceDesc, AraClipKey, AraColor, AraGraph, AraGraphChange, AraMusicalTimeline,
    AraPlaybackRegionDesc, AraPlaybackTransform, AraRegionSequenceDesc, AraSourceKey, AraTrackKey,
    HeldGraph,
};

use services::{
    ArchiveService, ArchiveSlot, ArchiveStore, AudioService, ContentService, GraphIndex,
    ModelService, SharedContent, TransportService, offers_harmonic_content, trace,
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

        let index = Arc::new(GraphIndex::default());
        let archives = Arc::new(ArchiveStore::default());
        let content = Arc::new(ContentService::new(Arc::clone(&index), generation));

        let services = HostServicesBuilder::new()
            .audio(AudioService::new(config.audio, Arc::clone(&index)))
            .archiving(ArchiveService::new(Arc::clone(&archives)))
            .model_updates(ModelService::new(config.observer, Arc::clone(&index)))
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
        let context = self.musical_context.ok_or_else(|| {
            AraHostError::invalid("set_musical_timeline must run before apply_graph")
        })?;
        // No edit cycle for a graph that is already in place: `endEditing` is
        // not free for the plug-in even when the edit was empty.
        if self.graph_change(graph) == AraGraphChange::Unchanged {
            return Ok(());
        }

        let supported = self.info.supported_transforms;

        // An open editor view is holding the regions it was last shown. Before
        // any of them is destroyed, show it the set without them, so no view
        // keeps a reference to a region that no longer exists.
        {
            let wanted_clips: HashSet<&AraClipKey> =
                graph.regions.iter().map(|region| &region.key).collect();
            if self.clips.keys().any(|key| !wanted_clips.contains(key)) {
                let wanted_sequences: HashSet<&AraTrackKey> = graph
                    .sequences
                    .iter()
                    .map(|sequence| &sequence.key)
                    .collect();
                let remaining: Vec<AraClipKey> = self
                    .clips
                    .keys()
                    .filter(|key| wanted_clips.contains(key))
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
                    if let Err(error) = self.notify_editor_selection(renderer, &remaining, &tracks)
                    {
                        trace(&format!("editor selection before teardown: {error}"));
                    }
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
        let wanted_clips: HashSet<&AraClipKey> =
            graph.regions.iter().map(|region| &region.key).collect();

        let stale_clips: Vec<AraClipKey> = clips
            .keys()
            .filter(|key| !wanted_clips.contains(key))
            .cloned()
            .collect();

        // A playback region may not be destroyed while a renderer still holds
        // it, so drop those RAII assignments first — in **both** roles. The
        // editor-renderer assignment used to survive: the region was destroyed
        // under it, and dropping it afterwards (in `set_renderer_regions`)
        // handed the plug-in a dangling region in `removePlaybackRegion`.
        // Cutting or deleting a clip on an ARA track with the ARA panel open
        // took the app down that way.
        for key in &stale_clips {
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
        // and sequences they referenced.
        for key in &stale_clips {
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
            if let Some(entry) = sources.remove(&key) {
                // Access off before the source goes: the plug-in may be reading
                // it on an analysis thread, and disabling access is what makes
                // it close those readers. Destroying a source that is still
                // readable is an ARA API violation the SDK's controllers assert
                // on.
                edit.set_audio_source_samples_access(entry.handle, false)
                    .map_err(map_error)?;
                edit.destroy_audio_source(entry.handle).map_err(map_error)?;
                index.remove_source(entry.address);
            }
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
                desc.key.as_str(),
                desc.frame_count,
                desc.sample_rate,
                desc.channel_count,
                false.into(),
            )
            .map_err(map_error)?;
            match sources.get_mut(&desc.key) {
                Some(entry) if entry.desc == *desc => {}
                Some(entry) => {
                    edit.update_audio_source(entry.handle, properties)
                        .map_err(map_error)?;
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

        let mut new_clips: Vec<AraClipKey> = Vec::new();
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
                        desc.key.as_str(),
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
                    new_clips.push(desc.key.clone());
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

        edit.finish().map_err(map_error)?;
        notify_after_edit(document);

        // Identities are indexed as they are created -- they have to be, because
        // the plug-in calls back during the edit. What is left here is the work
        // that is only legal once the edit has closed.
        let _ = new_clips;
        for (_, handle) in new_sources {
            // Ask for note analysis on every source the host just published.
            // A plug-in is entitled to wait for the host to ask before spending
            // the CPU, and one that does shows an empty editor until then.
            // `Unsupported` only means this plug-in does not analyse notes.
            match document.request_audio_source_content_analysis::<Notes>(handle) {
                Ok(()) => trace("requested note analysis for a new audio source"),
                Err(AraError::Unsupported(_)) => {
                    trace("plug-in does not analyse notes; skipping the request")
                }
                Err(error) => trace(&format!("note analysis request failed: {error}")),
            }
        }

        Ok(())
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

    pub(crate) fn store_archive(&mut self) -> AraResult<Vec<u8>> {
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
        Ok(slot.map(|slot| slot.bytes).unwrap_or_default())
    }

    /// Restores `bytes`, stored under `archive_id`, into the document.
    ///
    /// The graph must already exist with the persistent IDs it was saved
    /// with: both paths match the archive's objects to live ones by ID.
    ///
    /// ARA 2 Final and later restore objects inside an ordinary edit cycle.
    /// Earlier plug-ins get the legacy restore scope, which (in every
    /// ARA-library controller) is an edit cycle that restores all live
    /// objects when it ends. The legacy call is refused outright at 2 Final
    /// and later — which, since this host negotiates the newest generation
    /// first and Apple Silicon allows nothing older, used to be every
    /// session: saved ARA edits were silently dropped on every reopen.
    pub(crate) fn restore_archive(&mut self, archive_id: &str, bytes: &[u8]) -> AraResult<()> {
        let token = Box::new(ArchiveToken {
            _sequence: self.next_archive,
        });
        self.next_archive += 1;
        let address = std::ptr::from_ref(token.as_ref()) as usize;
        // The ID the bytes were written under, not the plug-in's current one:
        // an archive from an older version must be read as that version.
        self.archives.open(
            address,
            ArchiveSlot {
                bytes: bytes.to_vec(),
                archive_id: Some(archive_id.to_owned()),
            },
        );

        let document = self.document_mut()?;
        let outcome = if document.generation() >= ApiGeneration::V2Final {
            document.edit().and_then(|mut edit| {
                let restored = edit.restore_objects_from_archive(token.as_ref(), None);
                // End the cycle whatever the restore said, so a refused
                // archive does not leave the document stuck in editing.
                let finished = edit.finish();
                restored.and(finished)
            })
        } else {
            document
                .restore_document_from_archive(token.as_ref())
                .and_then(|edit| edit.finish())
        };
        self.archives.take(address);
        drop(token);
        outcome.map_err(map_error)?;
        self.resync_harmony_after_restore();
        if let Ok(document) = self.document_mut() {
            notify_after_edit(document);
        }
        Ok(())
    }

    /// Tells the plug-in the harmony changed, after a restore.
    ///
    /// The archive brings back each clip's copy of the key it was saved with
    /// (Melodyne keeps scales per audio modification, copied from the musical
    /// context), and the key may have changed since — while the plug-in was
    /// missing, say. The timeline itself is unchanged, so the diff in
    /// `set_musical_timeline` would never say so. The restore itself has
    /// succeeded either way, so a failure here is traced, not returned.
    fn resync_harmony_after_restore(&mut self) {
        let has_keys = self
            .published
            .as_ref()
            .is_some_and(|timeline| !timeline.keys.is_empty());
        let Some(handle) = self.musical_context else {
            return;
        };
        if !has_keys || !offers_harmonic_content(self.generation) {
            return;
        }
        let Ok(document) = self.document_mut() else {
            return;
        };
        let outcome = document.edit().and_then(|mut edit| {
            let updated = edit.update_musical_context_content(handle, None, HARMONY_CHANGED);
            // End the cycle whatever the update said.
            let finished = edit.finish();
            updated.and(finished)
        });
        if let Err(error) = outcome {
            trace(&format!("harmony re-sync after restore: {error}"));
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

    #[test]
    fn harmonic_content_starts_at_ara_2_final() {
        assert!(!offers_harmonic_content(ApiGeneration::V2Draft));
        assert!(offers_harmonic_content(ApiGeneration::V2Final));
        assert!(offers_harmonic_content(ApiGeneration::V2xDraft));
        assert!(offers_harmonic_content(ApiGeneration::V23Final));
    }
}
