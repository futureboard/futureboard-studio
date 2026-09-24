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
//! callbacks arrive on foreign threads and are serviced by the provider objects,
//! which resolve their arguments through owned lookup tables and never call back
//! into the session. Audio reads happen on plug-in worker threads through
//! [`model::AraSampleReader`], which is resolved once at reader creation so the
//! read path does no map lookup.
//!
//! # Platforms
//!
//! Windows and macOS build the real runtime. Every other target builds
//! [`stub`], whose entry points return [`AraHostError::Unsupported`] so call
//! sites need no `cfg`.

#![deny(missing_docs)]

mod error;
mod info;
pub mod model;

pub use error::{AraHostError, AraResult};
pub use info::{AraFactoryInfo, AraRendererId, AraRoles};
pub use model::{
    AraAudioAccess, AraAudioSourceDesc, AraBarSignature, AraClipKey, AraColor, AraGraph,
    AraModelObserver, AraModelUpdate, AraMusicalTimeline, AraPlaybackRegionDesc,
    AraPlaybackTransform, AraRegionSequenceDesc, AraSampleReader, AraSourceKey, AraTempoEntry,
    AraTrackKey, AraTransportControl, AraTransportRequest,
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
/// A session maps one Futureboard project onto one ARA plug-in: the plug-in's
/// document controller, the graph of audio sources / region sequences /
/// modifications / playback regions, and the bound companion instances that
/// render those regions.
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

    /// Publishes the project's tempo map and bar signatures.
    ///
    /// Must be called at least once before [`Self::apply_graph`]: ARA 2 playback
    /// regions live on region sequences, and every region sequence needs a
    /// musical context.
    pub fn set_musical_timeline(&mut self, timeline: &AraMusicalTimeline) -> AraResult<()> {
        timeline.validate()?;
        self.inner.set_musical_timeline(timeline)
    }

    /// Reconciles the ARA graph with `graph`, creating, updating, and destroying
    /// objects as needed.
    ///
    /// Idempotent: applying the same graph twice performs no plug-in calls after
    /// the first.
    pub fn apply_graph(&mut self, graph: &AraGraph) -> AraResult<()> {
        graph.validate()?;
        self.inner.apply_graph(graph)
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

    /// Serialises the plug-in's document state.
    ///
    /// The returned bytes belong to the archive identifier reported by
    /// [`AraFactoryInfo::document_archive_id`]; store both together so
    /// [`Self::restore_archive`] can refuse an incompatible pairing later.
    pub fn store_archive(&mut self) -> AraResult<Vec<u8>> {
        self.inner.store_archive()
    }

    /// Restores previously stored document state.
    ///
    /// Call after [`Self::apply_graph`] has recreated the graph with the same
    /// persistent identifiers, and before playback or editor use.
    pub fn restore_archive(&mut self, archive_id: &str, bytes: &[u8]) -> AraResult<()> {
        if !self.factory().can_restore_archive(archive_id) {
            return Err(AraHostError::unsupported(format!(
                "archive '{archive_id}' cannot be restored by '{}'",
                self.factory().document_archive_id
            )));
        }
        self.inner.restore_archive(archive_id, bytes)
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
