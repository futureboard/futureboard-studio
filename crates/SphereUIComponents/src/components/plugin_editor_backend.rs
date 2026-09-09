//! Which mechanism this platform uses to put a plug-in's own view inside the
//! GPUI plug-in editor window — and, just as importantly, what the editor
//! window is therefore waiting for before it can attach.
//!
//! # Why this module exists
//!
//! The editor window was written against one mechanism and then asked every
//! platform to imitate it. Windows creates a `WS_CHILD` content window under
//! the GPUI window, hands that handle to the plug-in host process, and the host
//! attaches `IPlugView` to it from over there — cross-process window
//! reparenting being a Win32 facility. So "do we have a native parent handle
//! yet?" became the editor window's universal readiness question.
//!
//! macOS cannot answer that question, ever. `NSView` has no public
//! cross-process reparenting, so the handle the Windows path waits for is not
//! late on macOS, it is *not part of the design*. The editor window spun out its
//! wait ticks against a `None` that could never change and then reported "host
//! region never became ready" — a timeout, invented for a mechanism that was
//! never going to run.
//!
//! The fix is not to give macOS something handle-shaped. It is to stop treating
//! one platform's mechanism as the shared vocabulary: the backend names what it
//! does, and the readiness question follows from the backend rather than the
//! other way round.
//!
//! # Boundaries
//!
//! Everything here is plain data. No `NSView`, no `Retained<_>`, no `HWND`, no
//! `RawWindowHandle` variant — platform types stay inside the platform modules
//! that own them, so this file compiles and is type-checked identically on every
//! target. The `cfg!` expressions below are the *only* platform knowledge in it,
//! and they are expressions rather than `#[cfg]` attributes precisely so that
//! every branch stays compiled everywhere: the branch that runs on macOS is
//! never taken by a Windows build, and an untested branch on the platform that
//! needs it is how the original mis-report survived.

/// How a plug-in's native view gets inside the editor window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorBackendKind {
    /// **Windows.** The main app creates a content child window under the GPUI
    /// window and sends its handle to the plug-in host process, which calls
    /// `IPlugView::attached` on it. The view lives in the host process; only
    /// the window belongs to the app.
    RemoteNativeWindow,
    /// **macOS.** The plug-in's `NSView` is created and attached *in this
    /// process*, inside a container view owned by the GPUI window. Nothing
    /// crosses a process boundary, because on macOS nothing can.
    LocalNativeView,
    /// No implementation on this platform.
    Unimplemented,
}

impl EditorBackendKind {
    /// The backend this build actually has.
    ///
    /// macOS reports [`Self::LocalNativeView`] as the *intended* backend even
    /// while the AppKit container is still being built out — see
    /// [`local_native_view_ready`]. Naming the backend and reporting whether it
    /// is finished are separate questions, and collapsing them is what produced
    /// a message about parent handles on a platform that has no parent handles.
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::RemoteNativeWindow
        } else if cfg!(target_os = "macos") {
            Self::LocalNativeView
        } else {
            Self::Unimplemented
        }
    }

    /// Whether this backend waits for a native parent handle supplied by the
    /// windowing system before it can attach.
    ///
    /// Only the remote path does. A local view is attached to a container this
    /// process made, so there is no foreign handle in the picture at all — and
    /// asking for one was the bug.
    pub const fn waits_for_foreign_parent_handle(self) -> bool {
        matches!(self, Self::RemoteNativeWindow)
    }

    /// What the editor window is waiting on before it can attach.
    pub const fn readiness(self) -> HostRegionReadiness {
        match self {
            Self::RemoteNativeWindow => HostRegionReadiness::NativeParentHandle,
            Self::LocalNativeView => HostRegionReadiness::AppKitHostView,
            Self::Unimplemented => HostRegionReadiness::Nothing,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::RemoteNativeWindow => "remote native window",
            Self::LocalNativeView => "local native view",
            Self::Unimplemented => "unimplemented",
        }
    }
}

/// The thing a backend needs before `IPlugView` can be attached.
///
/// Named per backend rather than shared, so a log line or an error message says
/// what this platform was actually waiting for instead of borrowing another
/// platform's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRegionReadiness {
    /// A native parent window handle from the GPUI window (Windows).
    NativeParentHandle,
    /// An AppKit container view mounted in the GPUI window (macOS).
    AppKitHostView,
    /// Nothing; this platform has no path to attach at all.
    Nothing,
}

impl HostRegionReadiness {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeParentHandle => "native parent handle",
            Self::AppKitHostView => "AppKit host view",
            Self::Nothing => "nothing (no editor backend on this platform)",
        }
    }
}

/// Whether the local-native-view backend is finished enough to attach.
///
/// Separate from [`EditorBackendKind::current`] on purpose. macOS *is* a
/// local-native-view platform — that is the design and it is not going to
/// change — but the AppKit container view, the in-process `IEditController`,
/// and the processor/controller IPC bridge are still being built. Until all
/// three land this returns `false`, and the editor window reports a specific,
/// permanent reason rather than a fabricated timeout.
///
/// The C++ side is the gate: `sphere_daux_vst3_embed_editor` in
/// `vst3bridge/src/vst3_processor.cpp` is `#ifdef _WIN32` with a `return 0`
/// for everything else. Flip this the same day that gains a macOS branch.
pub const fn local_native_view_ready() -> bool {
    false
}

/// Whether this build can embed a plug-in's native view at all.
pub const fn native_embedding_available() -> bool {
    match EditorBackendKind::current() {
        EditorBackendKind::RemoteNativeWindow => true,
        EditorBackendKind::LocalNativeView => local_native_view_ready(),
        EditorBackendKind::Unimplemented => false,
    }
}

/// Why a local (in-process) editor could not be embedded.
///
/// Real plug-ins do not all split cleanly into component and controller: some
/// are one object, some lean on `IConnectionPoint` for internal messaging, some
/// carry licensing that assumes UI and DSP share a process. Each of those is a
/// clean refusal with a reason, never a crash and never a silently duplicated
/// DSP instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEditorCompatibility {
    /// The controller was created and the view attached.
    Supported,
    /// The AppKit container / in-process controller path is not built yet.
    BackendNotImplemented,
    /// The plug-in does not separate `IComponent` from `IEditController`
    /// safely, so hosting the UI here would mean a second DSP instance.
    UnsupportedSplitController,
    /// `IEditController` could not be created in this process.
    ControllerCreationFailed,
    /// The view refused `kPlatformTypeNSView`.
    ViewUnsupported,
    /// `IPlugView::attached` failed.
    ViewAttachFailed,
}

impl LocalEditorCompatibility {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }

    /// One sentence for the editor shell.
    ///
    /// Always about the *editor window*. Whether the plug-in itself is loaded
    /// and processing is a separate question with its own reporting, and a
    /// message that guessed at it would be wrong half the time — which is
    /// exactly the trap the old "host region never became ready" fell into.
    pub fn message(&self) -> &'static str {
        match self {
            Self::Supported => "",
            Self::BackendNotImplemented => {
                "Plug-in editor windows are not available on this platform yet. \
                 The plug-in's own view has to be created and attached inside \
                 Futureboard on macOS, which is still being built."
            }
            Self::UnsupportedSplitController => {
                "This plug-in cannot show its editor in isolated mode on macOS: \
                 it does not separate its processor from its controller, so its \
                 window cannot be opened without loading a second copy of it."
            }
            Self::ControllerCreationFailed => {
                "This plug-in's editor controller could not be created."
            }
            Self::ViewUnsupported => "This plug-in does not offer a macOS (NSView) editor.",
            Self::ViewAttachFailed => "This plug-in's editor could not be attached to its window.",
        }
    }

    /// Short tag for `[MacPluginEditor]` / `[PluginEditorBackend]` logging.
    pub fn log_reason(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::BackendNotImplemented => "backend_not_implemented",
            Self::UnsupportedSplitController => "unsupported_split_controller",
            Self::ControllerCreationFailed => "controller_creation_failed",
            Self::ViewUnsupported => "view_unsupported",
            Self::ViewAttachFailed => "view_attach_failed",
        }
    }
}

/// What "the readiness condition is not met" means right now.
///
/// The two cases look identical at the call site — both are "we have nothing
/// yet" — and reporting the second as the first is what put a Retry button on a
/// dialog that could never succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRegionWait {
    /// Genuinely not ready *yet*. Keep ticking.
    KeepWaiting,
    /// Never going to be ready. Report it and stop.
    GiveUp(LocalEditorCompatibility),
}

/// Decide whether an unmet readiness condition is worth waiting through.
///
/// Takes the backend rather than reading it, so both answers are exercised on
/// every platform: the branch that matters on macOS is never taken by a Windows
/// build.
pub fn host_region_wait(backend: EditorBackendKind, backend_ready: bool) -> HostRegionWait {
    match backend {
        // The handle really does arrive a frame or two late; giving up on the
        // first miss would break opening an editor on the platform that works.
        EditorBackendKind::RemoteNativeWindow => HostRegionWait::KeepWaiting,
        EditorBackendKind::LocalNativeView if backend_ready => HostRegionWait::KeepWaiting,
        EditorBackendKind::LocalNativeView => {
            HostRegionWait::GiveUp(LocalEditorCompatibility::BackendNotImplemented)
        }
        EditorBackendKind::Unimplemented => {
            HostRegionWait::GiveUp(LocalEditorCompatibility::BackendNotImplemented)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_platform_names_its_own_backend() {
        let backend = EditorBackendKind::current();
        if cfg!(target_os = "windows") {
            assert_eq!(backend, EditorBackendKind::RemoteNativeWindow);
        } else if cfg!(target_os = "macos") {
            assert_eq!(
                backend,
                EditorBackendKind::LocalNativeView,
                "macOS is a local-native-view platform by design, whether or \
                 not the container view is finished"
            );
        } else {
            assert_eq!(backend, EditorBackendKind::Unimplemented);
        }
    }

    #[test]
    fn only_the_remote_backend_waits_for_a_foreign_handle() {
        // The heart of the bug: macOS was asked for a handle that is not part
        // of its design, then blamed for not producing one.
        assert!(EditorBackendKind::RemoteNativeWindow.waits_for_foreign_parent_handle());
        assert!(!EditorBackendKind::LocalNativeView.waits_for_foreign_parent_handle());
        assert!(!EditorBackendKind::Unimplemented.waits_for_foreign_parent_handle());
    }

    #[test]
    fn readiness_is_named_per_backend_not_shared() {
        assert_eq!(
            EditorBackendKind::RemoteNativeWindow.readiness(),
            HostRegionReadiness::NativeParentHandle
        );
        assert_eq!(
            EditorBackendKind::LocalNativeView.readiness(),
            HostRegionReadiness::AppKitHostView
        );
        // A macOS log line must not say "native parent handle" — that is the
        // other platform's vocabulary and it sent people looking for a race.
        assert!(!EditorBackendKind::LocalNativeView
            .readiness()
            .label()
            .contains("parent handle"));
    }

    #[test]
    fn the_working_backend_waits_and_the_unbuilt_one_reports() {
        assert_eq!(
            host_region_wait(EditorBackendKind::RemoteNativeWindow, true),
            HostRegionWait::KeepWaiting
        );
        // Windows keeps waiting regardless: its readiness does not depend on
        // the local-view build-out, and a regression there is the one thing
        // this change must not cause.
        assert_eq!(
            host_region_wait(EditorBackendKind::RemoteNativeWindow, false),
            HostRegionWait::KeepWaiting
        );
        assert_eq!(
            host_region_wait(EditorBackendKind::LocalNativeView, false),
            HostRegionWait::GiveUp(LocalEditorCompatibility::BackendNotImplemented)
        );
        // Once the AppKit container lands, macOS waits like any other backend
        // rather than refusing — the state machine is already correct for it.
        assert_eq!(
            host_region_wait(EditorBackendKind::LocalNativeView, true),
            HostRegionWait::KeepWaiting
        );
    }

    #[test]
    fn every_refusal_carries_a_reason_and_a_log_tag() {
        for reason in [
            LocalEditorCompatibility::BackendNotImplemented,
            LocalEditorCompatibility::UnsupportedSplitController,
            LocalEditorCompatibility::ControllerCreationFailed,
            LocalEditorCompatibility::ViewUnsupported,
            LocalEditorCompatibility::ViewAttachFailed,
        ] {
            assert!(!reason.is_supported());
            assert!(!reason.message().is_empty(), "{reason:?} has no message");
            assert!(!reason.log_reason().is_empty());
            // The shell's message is about the editor, never a claim about
            // whether the plug-in is loaded and processing audio.
            assert!(
                !reason.message().contains("audio"),
                "{reason:?} speculates about DSP state"
            );
        }
        assert!(LocalEditorCompatibility::Supported.is_supported());
        assert!(LocalEditorCompatibility::Supported.message().is_empty());
    }

    #[test]
    fn native_embedding_availability_follows_the_backend() {
        // Windows is available today; macOS becomes available the day
        // `local_native_view_ready` flips, with nothing else to change here.
        assert_eq!(
            native_embedding_available(),
            cfg!(target_os = "windows") || (cfg!(target_os = "macos") && local_native_view_ready())
        );
    }
}
