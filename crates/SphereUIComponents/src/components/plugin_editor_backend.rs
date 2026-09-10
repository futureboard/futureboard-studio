//! Which mechanism this platform uses to put a plug-in's own view in front of
//! the user — and, just as importantly, what the editor window is therefore
//! waiting for before it can attach.
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
//! What macOS does instead is what Linux already does: the plug-in's view is
//! attached to a top-level window **inside the host process**, which owns it,
//! shows it, and reports when the user closes it. That is a whole backend of
//! its own, not a degraded version of the Windows one, and it has been
//! implemented on the far side of the bridge for as long as the Linux path has
//! — `sphere_daux_vst3_embed_editor` takes its `__APPLE__` branch into
//! `open_editor_mac`, and the host process brings up `NSApplication` and pumps
//! it. The gap was only ever on this side: an editor window that would not ask.
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

use SpherePluginHost::native_editor::PluginEditorPresentationMode;

/// How a plug-in's own view gets in front of the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorBackendKind {
    /// **Windows.** The main app creates a content child window under the GPUI
    /// window and sends its handle to the plug-in host process, which calls
    /// `IPlugView::attached` on it. The view lives in the host process; only
    /// the window belongs to the app.
    RemoteNativeWindow,
    /// **macOS and Linux.** The plug-in host process creates a top-level window
    /// of its own and attaches `IPlugView` inside it. Nothing is embedded in the
    /// GPUI window, so this process supplies no parent handle and creates no
    /// content child — it asks, and then it reports what came back.
    ///
    /// Not a fallback. Neither platform has public cross-process view
    /// reparenting (`NSView` has none at all; XEmbed across processes is the
    /// thing host-owned mode exists to avoid), so the window belongs to
    /// whichever process owns the view, and that is the host.
    HostOwnedNativeWindow,
    /// The plug-in's `NSView` created and attached *in this* process, inside a
    /// container view owned by the GPUI window.
    ///
    /// Groundwork only — see [`local_native_view_ready`]. It would put the
    /// editor back inside the Studio window on macOS, at the cost of an
    /// in-process `IEditController` beside a processor that lives in the host.
    LocalNativeView,
    /// No implementation on this platform.
    Unimplemented,
}

impl EditorBackendKind {
    /// The backend this build actually has.
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::RemoteNativeWindow
        } else if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            Self::HostOwnedNativeWindow
        } else {
            Self::Unimplemented
        }
    }

    /// Whether this backend waits for a native parent handle supplied by the
    /// windowing system before it can attach.
    ///
    /// Only the remote path does. A host-owned window has no parent in this
    /// process at all, and a local view is attached to a container this process
    /// made — asking either for a foreign handle was the bug.
    pub const fn waits_for_foreign_parent_handle(self) -> bool {
        matches!(self, Self::RemoteNativeWindow)
    }

    /// Whether the plug-in's view lives inside the GPUI editor window.
    ///
    /// False for the host-owned backend, and that single answer is what the
    /// editor shell needs: no content child to create, no region to push, no
    /// shell to grow to the plug-in's size, and a window of the plug-in's own
    /// to watch for the user closing.
    pub const fn embeds_in_editor_window(self) -> bool {
        matches!(self, Self::RemoteNativeWindow | Self::LocalNativeView)
    }

    /// How an attached editor on this backend is presented.
    ///
    /// Resolved here rather than at the call site so the shell's rendering, its
    /// teardown watch, and its geometry all read the same answer.
    pub const fn presentation(self) -> PluginEditorPresentationMode {
        match self {
            Self::HostOwnedNativeWindow => PluginEditorPresentationMode::DetachedNativeWindow,
            _ => PluginEditorPresentationMode::ChildHwndEmbed,
        }
    }

    /// What the editor window is waiting on before it can attach.
    pub const fn readiness(self) -> HostRegionReadiness {
        match self {
            Self::RemoteNativeWindow => HostRegionReadiness::NativeParentHandle,
            Self::HostOwnedNativeWindow => HostRegionReadiness::HostProcessEditorWindow,
            Self::LocalNativeView => HostRegionReadiness::AppKitHostView,
            Self::Unimplemented => HostRegionReadiness::Nothing,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::RemoteNativeWindow => "remote native window",
            Self::HostOwnedNativeWindow => "host-owned native window",
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
    /// The window the plug-in host process opens for the view (macOS, Linux).
    /// Nothing in this process gates it; the host answers with `EditorAttached`.
    HostProcessEditorWindow,
    /// An AppKit container view mounted in the GPUI window (the local backend).
    AppKitHostView,
    /// Nothing; this platform has no path to attach at all.
    Nothing,
}

impl HostRegionReadiness {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeParentHandle => "native parent handle",
            Self::HostProcessEditorWindow => "the host process's editor window",
            Self::AppKitHostView => "AppKit host view",
            Self::Nothing => "nothing (no editor backend on this platform)",
        }
    }
}

/// Whether the local-native-view backend is finished enough to attach.
///
/// Separate from [`EditorBackendKind::current`] on purpose, and today no
/// platform selects that backend *for a bridged plug-in*: putting one back
/// inside the Studio window on macOS would need an in-process
/// `IEditController` beside a processor that lives in the host, plus a bridge
/// between the two. Until that lands, a bridged editor opens in the host
/// process's own window, which is a finished path rather than a degraded one.
///
/// The container view this would use is not idle in the meantime. A plug-in the
/// app hosts *itself* has no process boundary to cross, and ARA is exactly
/// that: its editor is mounted in the studio's own window through
/// [`crate::components::plugin_editor_mac_region::DockedPluginSurface`], on
/// macOS as much as on Windows. What this flag gates is the harder case — a
/// plug-in whose DSP is somewhere else.
pub const fn local_native_view_ready() -> bool {
    false
}

/// Whether this build can put a plug-in's editor in front of the user at all.
pub const fn native_editor_available() -> bool {
    match EditorBackendKind::current() {
        EditorBackendKind::RemoteNativeWindow | EditorBackendKind::HostOwnedNativeWindow => true,
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
                 Futureboard has no way to put this plug-in's own view on \
                 screen here."
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
        // Nothing in this process gates a host-owned window, so this is only
        // ever reached before the request has gone out — the next tick sends
        // it. Refusing here would refuse the one backend macOS has.
        EditorBackendKind::HostOwnedNativeWindow => HostRegionWait::KeepWaiting,
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
        } else if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            assert_eq!(
                backend,
                EditorBackendKind::HostOwnedNativeWindow,
                "neither platform can reparent a plug-in's view across the \
                 process boundary, so the host process owns the window"
            );
        } else {
            assert_eq!(backend, EditorBackendKind::Unimplemented);
        }
    }

    #[test]
    fn every_platform_that_ships_has_an_editor() {
        // The regression this guards: macOS reported "not available on this
        // platform" while the host process had been able to open the editor all
        // along. A platform Futureboard ships on has an editor path or the
        // refusal is a bug in this table, not a gap in the product.
        assert!(native_editor_available());
    }

    #[test]
    fn only_the_remote_backend_waits_for_a_foreign_handle() {
        // The heart of the bug: macOS was asked for a handle that is not part
        // of its design, then blamed for not producing one.
        assert!(EditorBackendKind::RemoteNativeWindow.waits_for_foreign_parent_handle());
        assert!(!EditorBackendKind::HostOwnedNativeWindow.waits_for_foreign_parent_handle());
        assert!(!EditorBackendKind::LocalNativeView.waits_for_foreign_parent_handle());
        assert!(!EditorBackendKind::Unimplemented.waits_for_foreign_parent_handle());
    }

    #[test]
    fn only_a_host_owned_editor_lives_outside_this_window() {
        assert!(EditorBackendKind::RemoteNativeWindow.embeds_in_editor_window());
        assert!(EditorBackendKind::LocalNativeView.embeds_in_editor_window());
        assert!(!EditorBackendKind::HostOwnedNativeWindow.embeds_in_editor_window());
    }

    #[test]
    fn presentation_follows_where_the_view_actually_is() {
        // A host-owned editor presented as a child embed would leave the shell
        // pushing region updates at a window in another process and growing
        // itself to a size no surface of its own will ever use.
        assert_eq!(
            EditorBackendKind::HostOwnedNativeWindow.presentation(),
            PluginEditorPresentationMode::DetachedNativeWindow
        );
        assert_eq!(
            EditorBackendKind::RemoteNativeWindow.presentation(),
            PluginEditorPresentationMode::ChildHwndEmbed
        );
    }

    #[test]
    fn readiness_is_named_per_backend_not_shared() {
        assert_eq!(
            EditorBackendKind::RemoteNativeWindow.readiness(),
            HostRegionReadiness::NativeParentHandle
        );
        assert_eq!(
            EditorBackendKind::HostOwnedNativeWindow.readiness(),
            HostRegionReadiness::HostProcessEditorWindow
        );
        assert_eq!(
            EditorBackendKind::LocalNativeView.readiness(),
            HostRegionReadiness::AppKitHostView
        );
        // A macOS log line must not say "native parent handle" — that is the
        // other platform's vocabulary and it sent people looking for a race.
        for backend in [
            EditorBackendKind::HostOwnedNativeWindow,
            EditorBackendKind::LocalNativeView,
        ] {
            assert!(!backend.readiness().label().contains("parent handle"));
        }
    }

    #[test]
    fn the_backends_that_ship_wait_and_the_unbuilt_ones_report() {
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
        // The host-owned backend has nothing of its own to be short of, so it
        // must never refuse — refusing here is exactly what macOS did.
        assert_eq!(
            host_region_wait(EditorBackendKind::HostOwnedNativeWindow, false),
            HostRegionWait::KeepWaiting
        );
        assert_eq!(
            host_region_wait(EditorBackendKind::LocalNativeView, false),
            HostRegionWait::GiveUp(LocalEditorCompatibility::BackendNotImplemented)
        );
        assert_eq!(
            host_region_wait(EditorBackendKind::LocalNativeView, true),
            HostRegionWait::KeepWaiting
        );
        assert_eq!(
            host_region_wait(EditorBackendKind::Unimplemented, true),
            HostRegionWait::GiveUp(LocalEditorCompatibility::BackendNotImplemented)
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
    fn native_editor_availability_follows_the_backend() {
        assert_eq!(
            native_editor_available(),
            !matches!(
                EditorBackendKind::current(),
                EditorBackendKind::Unimplemented
            )
        );
    }
}
