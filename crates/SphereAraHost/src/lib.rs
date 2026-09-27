//! ARA 2 host runtime for Futureboard Studio.
//!
//! # Role and boundaries
//!
//! This crate turns Futureboard's project state into an ARA document graph and
//! drives an ARA plug-in against it. It owns the ARA host services, the document
//! session, and the companion binding, and nothing else: it does not decode
//! audio, does not know about GPUI, and never touches the audio engine's types.
//! Everything it needs from the application arrives through the traits in
//! [`model`]; everything it needs from the plug-in arrives as raw
//! `Steinberg::FUnknown*` / `ARAFactory*` pointers produced by
//! `SphereDirectAudioEngine`'s in-process VST3 bridge.
//!
//! That keeps the dependency graph acyclic — the engine never depends on this
//! crate for the model — and keeps every ARA type out of the rest of the tree.
//!
//! # Threading
//!
//! [`AraSession`] is the ARA *model thread* and is deliberately `!Send`: create
//! it, mutate it, and drop it on the application's main thread. Plug-in
//! callbacks are serviced by the provider objects, which resolve their
//! arguments through owned lookup tables and never call back into the session.
//! Most of them arrive re-entrantly on the model thread, from inside one of the
//! session's own calls: content reads during create/update/`endEditing`, model
//! updates during [`AraSession::notify_model_updates`], archive I/O during a
//! store or restore. The session therefore never holds a provider's lock across
//! a call into the plug-in. Audio reads happen on plug-in worker threads through
//! [`model::AraSampleReader`], which is resolved once at reader creation so the
//! read path does no map lookup.
//!
//! # Platforms
//!
//! Windows and macOS build the real runtime. Every other target builds
//! [`stub`], whose entry points return [`AraHostError::Unsupported`] so call
//! sites need no `cfg`.

#![deny(missing_docs)]

mod archive;
mod error;
mod info;
pub mod model;

pub use archive::{
    AraArchiveIdentity, AraArchivedModification, AraArchivedSource, AraKeyDescriptor,
    AraRestoreMap, AraRestoreOutcome, AraRestoreReport, AraRestoreRequest, AraRestoreScope,
    AraSourceRestore, AraStoredArchive,
};
pub use error::{AraHostError, AraResult};
pub use info::{AraFactoryInfo, AraRendererId, AraRoles};
pub use model::{
    ARA_ENCODED_ID_PREFIX, AraAudioAccess, AraAudioSourceDesc, AraBarSignature, AraClipKey,
    AraColor, AraGraph, AraGraphChange, AraKeySignature, AraModelObserver, AraModelUpdate,
    AraMusicalTimeline, AraPlaybackRegionDesc, AraPlaybackTransform, AraRegionSequenceDesc,
    AraSampleReader, AraSourceKey, AraTempoEntry, AraTrackKey, AraTransportControl,
    AraTransportRequest, ara_persistent_id,
};

use std::sync::Arc;

/// Everything the host supplies when opening a document.
pub struct AraSessionConfig {
    /// Document name shown by the plug-in.
    pub document_name: Option<String>,
    /// Random-access sample source for every ARA audio source.
    pub audio: Arc<dyn AraAudioAccess>,
    /// Sink for transport requests coming from the plug-in's editor.
    pub transport: Arc<dyn AraTransportControl>,
    /// Sink for asynchronous model updates.
    pub observer: Arc<dyn AraModelObserver>,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod imp;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod stub;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use stub as imp;

/// Reads the `ARAFactory` out of a VST3 ARA main-factory class instance.
///
/// `SphereDirectAudioEngine`'s in-process VST3 bridge instantiates that class and
/// hands back its `Steinberg::FUnknown*`; this turns it into the `ARAFactory*`
/// [`AraSession::open`] and [`AraSession::probe_factory`] take, without any COM
/// work outside the audited companion shim.
///
/// # Safety
///
/// `main_factory` must be a live `Steinberg::FUnknown*` for the plug-in's ARA
/// main-factory class. The returned `ARAFactory*` is owned by that class
/// instance and is only valid while the caller keeps it alive.
pub unsafe fn vst3_ara_factory(
    main_factory: *mut std::ffi::c_void,
) -> AraResult<*const std::ffi::c_void> {
    // SAFETY: the caller forwards the live-object contract documented above.
    unsafe { imp::vst3_ara_factory(main_factory) }
}

/// One ARA document and the plug-in instances bound to it.
///
/// Futureboard opens one session per (plug-in, track): the plug-in's document
/// controller, one musical context, the track's region sequence with its audio
/// sources, modifications and playback regions, and the bound companion
/// instances that render those regions.
pub struct AraSession {
    inner: imp::Session,
}

impl AraSession {
    /// Whether this build can host ARA plug-ins at all.
    ///
    /// `false` on unsupported platforms; every other entry point then fails with
    /// [`AraHostError::Unsupported`].
    pub fn is_supported() -> bool {
        imp::is_supported()
    }

    /// Copies a plug-in's factory metadata without creating a document.
    ///
    /// Use this to decide whether a scanned plug-in is usable and which archive
    /// identifiers it accepts, before committing a clip to it.
    ///
    /// # Safety
    ///
    /// `factory` must be a live `ARAFactory*` obtained from the plug-in's ARA
    /// main-factory class and must stay readable for the duration of the call.
    pub unsafe fn probe_factory(factory: *const std::ffi::c_void) -> AraResult<AraFactoryInfo> {
        // SAFETY: the caller forwards the live-factory contract documented above.
        unsafe { imp::probe_factory(factory) }
    }

    /// Creates the plug-in's document controller and an empty ARA graph.
    ///
    /// # Safety
    ///
    /// `factory` must be a live `ARAFactory*` from the plug-in's ARA
    /// main-factory class, and must outlive the returned session. The factory
    /// must not already be initialized by another ARA host in this process.
    pub unsafe fn open(
        factory: *const std::ffi::c_void,
        config: AraSessionConfig,
    ) -> AraResult<Self> {
        // SAFETY: the caller forwards the live-factory contract documented above.
        let inner = unsafe { imp::Session::open(factory, config) }?;
        Ok(Self { inner })
    }

    /// Returns the negotiated factory metadata.
    pub fn factory(&self) -> &AraFactoryInfo {
        self.inner.factory()
    }

    /// Publishes the project's tempo map, bar signatures and key.
    ///
    /// Must be called at least once before [`Self::apply_graph`]: ARA 2 playback
    /// regions live on region sequences, and every region sequence needs a
    /// musical context.
    ///
    /// Diffed against the last published timeline: the plug-in is told which
    /// content changed (timing, harmony, or both), and an unchanged timeline
    /// makes no plug-in call at all. Keys are published only from ARA 2.0 Final
    /// on, where key-signature content exists.
    pub fn set_musical_timeline(&mut self, timeline: &AraMusicalTimeline) -> AraResult<()> {
        timeline.validate()?;
        self.inner.set_musical_timeline(timeline)
    }

    /// Reconciles the ARA graph with `graph`, creating, updating, and destroying
    /// objects as needed.
    ///
    /// Idempotent: applying the same graph twice performs no plug-in calls after
    /// the first. Keys are published through [`ara_persistent_id`], so any key
    /// is accepted. A clip that moved to another source or track gets a new
    /// modification and region. Sources it creates are asked for note analysis
    /// straight away; to open a saved document, use
    /// [`Self::apply_graph_restoring`] instead.
    pub fn apply_graph(&mut self, graph: &AraGraph) -> AraResult<()> {
        graph.validate()?;
        self.inner.apply_graph(graph)
    }

    /// Reconciles the ARA graph with `graph` and restores a saved document
    /// into it in the same edit cycle.
    ///
    /// This is the order ARA documents for opening a saved document: begin
    /// editing, build the graph, restore, end editing. Note analysis is then
    /// requested only for new sources the restore left without content, so a
    /// plug-in never analyses what the archive is about to supply. (Asking
    /// first, then restoring in a later cycle, left Melodyne reporting the
    /// analysis incomplete for good.) When the graph needs no change, or the
    /// plug-in predates ARA 2 Final, the restore runs in its own cycle.
    ///
    /// `Err` means the graph was not applied, and the restore did not count:
    /// keep the archive. A restore that cannot run or that the plug-in refuses
    /// never stops the graph; it is reported in [`AraRestoreReport::error`].
    /// `Ok(None)` when `restore` is `None`. Only
    /// [`AraRestoreReport::restored`] means the live document now holds the
    /// archive; storing the document after any other verdict may lose archived
    /// state. See [`Self::restore`] for how a restore is placed and judged.
    pub fn apply_graph_restoring(
        &mut self,
        graph: &AraGraph,
        restore: Option<AraRestoreRequest<'_>>,
    ) -> AraResult<Option<AraRestoreReport>> {
        graph.validate()?;
        self.inner.apply_graph_restoring(graph, restore)
    }

    /// [`Self::apply_graph_restoring`] with several restores, run in the
    /// order given inside the same edit cycle, one report each, in order.
    ///
    /// For an archive restored in parts ([`AraRestoreScope::Sources`]) and a
    /// document restored whole on the same build: ARA allows several
    /// restores in one cycle, and asks for the part that brings the
    /// plug-in's private document data to come last, once the graph has its
    /// final structure and every object's state is in place, so the caller
    /// orders them that way. `Err` means the graph was not applied and no
    /// restore counted; a restore that cannot run or is refused is reported
    /// in its own report and stops neither the graph nor the others.
    pub fn apply_graph_restoring_all(
        &mut self,
        graph: &AraGraph,
        restores: &[AraRestoreRequest<'_>],
    ) -> AraResult<Vec<AraRestoreReport>> {
        graph.validate()?;
        self.inner.apply_graph_restoring_all(graph, restores)
    }

    /// How far `graph` departs from what the document already holds.
    ///
    /// Lets a caller keep the renderer suspension, the rendering toggle and
    /// the region re-assignment for applies that create or destroy regions:
    /// [`AraGraphChange::Unchanged`] makes no plug-in call at all, and
    /// [`AraGraphChange::Properties`] only updates objects in place, which ARA
    /// allows while they render. A graph that fails validation reports
    /// [`AraGraphChange::Structure`], so the full apply runs and returns the
    /// validation error.
    pub fn graph_change(&self, graph: &AraGraph) -> AraGraphChange {
        if graph.validate().is_err() {
            return AraGraphChange::Structure;
        }
        self.inner.graph_change(graph)
    }

    /// Lets the plug-in deliver its pending model notifications.
    ///
    /// ARA plug-ins may report analysis progress, content changes and
    /// document-data changes only from inside this call, so the host must make
    /// it periodically whenever it is neither editing nor restoring. The session
    /// already makes it after each of its own edits.
    pub fn notify_model_updates(&mut self) -> AraResult<()> {
        self.inner.notify_model_updates()
    }

    /// Binds one companion plug-in instance to this document.
    ///
    /// # Safety
    ///
    /// `component` must be the live `Steinberg::FUnknown*` identity of an
    /// initialized VST3 component belonging to the same plug-in as this
    /// session's factory, and must stay alive until the session is closed or
    /// [`Self::unbind_renderer`] is called for the returned id.
    pub unsafe fn bind_renderer(
        &mut self,
        component: *mut std::ffi::c_void,
        roles: AraRoles,
    ) -> AraResult<AraRendererId> {
        if roles.is_empty() {
            return Err(AraHostError::invalid(
                "an ARA binding needs at least one role",
            ));
        }
        // SAFETY: the caller forwards the live-component contract documented above.
        unsafe { self.inner.bind_renderer(component, roles) }
    }

    /// Releases one bound instance's assignments.
    ///
    /// The companion instance itself stays alive; the caller destroys it.
    pub fn unbind_renderer(&mut self, renderer: AraRendererId) -> AraResult<()> {
        self.inner.unbind_renderer(renderer)
    }

    /// Sets exactly which clips a bound instance renders.
    ///
    /// Regions not listed are removed from the instance. Every listed clip must
    /// already exist in the applied graph.
    pub fn set_renderer_regions(
        &mut self,
        renderer: AraRendererId,
        clips: &[AraClipKey],
    ) -> AraResult<()> {
        self.inner.set_renderer_regions(renderer, clips)
    }

    /// Publishes the editor-view selection for one bound instance.
    ///
    /// Renderer assignments decide what a plug-in *processes*; this decides
    /// what its editor *shows*. A plug-in that never receives a selection opens
    /// on an empty canvas, so call this whenever the graph or the user's
    /// selection changes.
    pub fn notify_editor_selection(
        &mut self,
        renderer: AraRendererId,
        clips: &[AraClipKey],
        tracks: &[AraTrackKey],
    ) -> AraResult<()> {
        self.inner.notify_editor_selection(renderer, clips, tracks)
    }

    /// Enables or disables rendering for one bound instance.
    ///
    /// Disable before a graph change that removes its regions, and re-enable
    /// afterwards.
    pub fn set_rendering(&mut self, renderer: AraRendererId, enabled: bool) -> AraResult<()> {
        self.inner.set_rendering(renderer, enabled)
    }

    /// Serialises the plug-in's document state, with the identity it holds.
    ///
    /// The bytes belong to the archive identifier reported by
    /// [`AraFactoryInfo::document_archive_id`]; store that, the bytes and
    /// [`AraStoredArchive::identity`] together. The identity is read from the
    /// graph in the same call, so it describes exactly these bytes: every
    /// source and modification under the persistent ID it was stored with, and
    /// the key the plug-in was offered. A later restore needs it to tell
    /// whether the archive landed and to map IDs that moved.
    pub fn store_archive(&mut self) -> AraResult<AraStoredArchive> {
        self.inner.store_archive()
    }

    /// Restores a saved document into the graph as it stands, in an edit
    /// cycle of its own.
    ///
    /// To open a saved document prefer [`Self::apply_graph_restoring`], which
    /// restores in the cycle that builds the graph.
    ///
    /// Placement: without [`AraRestoreRequest::map`] the plug-in restores
    /// whatever matches by persistent ID (no filter), unless
    /// [`AraRestoreRequest::scope`] names part of the archive, which always
    /// goes through a filter listing only that part. With a map it gets a
    /// filter with document data selected that lists every mapped source,
    /// every other source the archive holds under its current ID, and each
    /// listed source's archived modifications that have a place on it; ARA
    /// skips whatever a filter does not list. A map naming an object missing
    /// from the graph, naming one twice, or (with an identity) mapping onto
    /// audio of another length, rate or channel count is refused.
    ///
    /// Verdict: a plug-in reports success even when nothing matched, so each
    /// targeted source is judged by its note-content grade before and after
    /// (see [`AraRestoreOutcome`]), and the identity, when given, reports
    /// what had no place. When the identity records a key that differs from
    /// the project's now, the plug-in is then told the harmony changed; with
    /// an unknown key it is not.
    pub fn restore(&mut self, request: AraRestoreRequest<'_>) -> AraRestoreReport {
        self.inner.restore(request)
    }

    /// Restores previously stored document state, by matching persistent ID.
    ///
    /// Shorthand for [`Self::restore`] with no identity and no map that keeps
    /// only the error. `Ok` does not mean anything was restored: a plug-in
    /// reports success when no ID matched. Use [`Self::restore`] or
    /// [`Self::apply_graph_restoring`] to find out.
    pub fn restore_archive(&mut self, archive_id: &str, bytes: &[u8]) -> AraResult<()> {
        let report = self.inner.restore(AraRestoreRequest {
            archive_id,
            bytes,
            identity: None,
            map: None,
            scope: AraRestoreScope::Whole,
        });
        report.error.map_or(Ok(()), Err)
    }

    /// Whether the plug-in reports note analysis of `source` incomplete right
    /// now.
    ///
    /// `None` when the source is not in the graph or the plug-in does not
    /// analyse notes. A plug-in that restores a source's analysis may never
    /// complete an analysis it reported started before; `Some(false)` settles
    /// such a report.
    pub fn analysis_incomplete(&mut self, source: &AraSourceKey) -> AraResult<Option<bool>> {
        self.inner.analysis_incomplete(source)
    }

    /// Whether the session was quarantined by an ARA assertion or an impossible
    /// plug-in result. A poisoned session must be closed and rebuilt.
    pub fn is_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }

    /// Tears down the graph and the document controller, reporting every failure.
    pub fn close(self) -> AraResult<()> {
        self.inner.close()
    }
}
