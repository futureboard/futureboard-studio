//! Platform stub.
//!
//! Built where ARA hosting is not supported (currently everything that is not
//! Windows or macOS). It mirrors [`crate::imp`]'s surface exactly so call sites
//! never branch on the platform; every operation reports
//! [`AraHostError::Unsupported`].

use std::ffi::c_void;
use std::marker::PhantomData;

use crate::AraSessionConfig;
use crate::archive::{AraRestoreReport, AraRestoreRequest, AraStoredArchive};
use crate::error::{AraHostError, AraResult};
use crate::info::{AraFactoryInfo, AraRendererId, AraRoles};
use crate::model::{
    AraClipKey, AraGraph, AraGraphChange, AraMusicalTimeline, AraSourceKey, AraTrackKey,
};

fn unsupported<T>() -> AraResult<T> {
    Err(AraHostError::unsupported(
        "ARA hosting is only available on Windows and macOS",
    ))
}

pub(crate) fn is_supported() -> bool {
    false
}

/// # Safety
///
/// Matches the real implementation's contract; the pointer is never read.
pub(crate) unsafe fn vst3_ara_factory(_main_factory: *mut c_void) -> AraResult<*const c_void> {
    unsupported()
}

/// # Safety
///
/// Matches the real implementation's contract; the pointer is never read.
pub(crate) unsafe fn probe_factory(_factory: *const c_void) -> AraResult<AraFactoryInfo> {
    unsupported()
}

/// Never constructed on this platform.
pub(crate) struct Session {
    /// Keeps the type `!Send`, matching the real session.
    _not_send: PhantomData<*const ()>,
    factory: AraFactoryInfo,
}

impl Session {
    /// # Safety
    ///
    /// Matches the real implementation's contract; the pointer is never read.
    pub(crate) unsafe fn open(
        _factory: *const c_void,
        _config: AraSessionConfig,
    ) -> AraResult<Self> {
        unsupported()
    }

    pub(crate) fn factory(&self) -> &AraFactoryInfo {
        &self.factory
    }

    pub(crate) fn set_musical_timeline(&mut self, _timeline: &AraMusicalTimeline) -> AraResult<()> {
        unsupported()
    }

    pub(crate) fn apply_graph(&mut self, _graph: &AraGraph) -> AraResult<()> {
        unsupported()
    }

    pub(crate) fn apply_graph_restoring(
        &mut self,
        _graph: &AraGraph,
        _restore: Option<AraRestoreRequest<'_>>,
    ) -> AraResult<Option<AraRestoreReport>> {
        unsupported()
    }

    pub(crate) fn apply_graph_restoring_all(
        &mut self,
        _graph: &AraGraph,
        _restores: &[AraRestoreRequest<'_>],
    ) -> AraResult<Vec<AraRestoreReport>> {
        unsupported()
    }

    pub(crate) fn graph_change(&self, _graph: &AraGraph) -> AraGraphChange {
        AraGraphChange::Structure
    }

    pub(crate) fn notify_model_updates(&mut self) -> AraResult<()> {
        unsupported()
    }

    /// # Safety
    ///
    /// Matches the real implementation's contract; the pointer is never read.
    pub(crate) unsafe fn bind_renderer(
        &mut self,
        _component: *mut c_void,
        _roles: AraRoles,
    ) -> AraResult<AraRendererId> {
        unsupported()
    }

    pub(crate) fn unbind_renderer(&mut self, _renderer: AraRendererId) -> AraResult<()> {
        unsupported()
    }

    pub(crate) fn set_renderer_regions(
        &mut self,
        _renderer: AraRendererId,
        _clips: &[AraClipKey],
    ) -> AraResult<()> {
        unsupported()
    }

    pub(crate) fn notify_editor_selection(
        &mut self,
        _renderer: AraRendererId,
        _clips: &[AraClipKey],
        _tracks: &[AraTrackKey],
    ) -> AraResult<()> {
        unsupported()
    }

    pub(crate) fn set_rendering(
        &mut self,
        _renderer: AraRendererId,
        _enabled: bool,
    ) -> AraResult<()> {
        unsupported()
    }

    pub(crate) fn store_archive(&mut self) -> AraResult<AraStoredArchive> {
        unsupported()
    }

    pub(crate) fn restore(&mut self, _request: AraRestoreRequest<'_>) -> AraRestoreReport {
        AraRestoreReport::not_run(AraHostError::unsupported(
            "ARA hosting is only available on Windows and macOS",
        ))
    }

    pub(crate) fn analysis_incomplete(&mut self, _key: &AraSourceKey) -> AraResult<Option<bool>> {
        unsupported()
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        false
    }

    pub(crate) fn close(self) -> AraResult<()> {
        Ok(())
    }
}
