//! The Objective-C half of the host region: an `NSView` container mounted in
//! the GPUI window, positioned by [`super::HostRegionModel`].
//!
//! Public AppKit only, and only what the repo already depends on. The message
//! sends follow `SphereWebView::runtime::platform_set_bounds`, which does the
//! same job for the CEF view on macOS — same crates, same pinned versions
//! (`objc2` 0.6, `objc2-foundation` 0.3), same untyped `AnyObject` style. The
//! workspace enables `objc2-app-kit` with `default-features = false` and only
//! the `NSGraphics` feature, so `NSView` is *not* available as a typed class
//! here; going through `AnyObject` is not laziness, it is the API surface this
//! build actually has.
//!
//! # Threading
//!
//! Every function here must run on the main thread. AppKit requires it, and so
//! does VST3 for GUI calls. The callers are GPUI window methods, which are
//! already main-thread; [`assert_main_thread`] is the guard for anything that
//! grows a different caller later, and it refuses rather than corrupting the
//! view hierarchy from a worker.
//!
//! # Ownership
//!
//! [`MacHostRegion`] owns one retained `NSView`. It is released on `Drop`,
//! after being removed from its superview. The plug-in's own view is *not*
//! owned here — `IPlugView::removed()` takes it out, and that call belongs to
//! the VST3 layer, not to this container.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::{FramePoints, ParentGeometry};

/// True when running on the AppKit main thread.
///
/// `NSThread.isMainThread` rather than a cached thread id: the main thread is
/// AppKit's notion, not ours, and a cached id would be wrong in a host process
/// that did not start the run loop on the thread we happened to initialise on.
fn is_main_thread() -> bool {
    // SAFETY: `+[NSThread isMainThread]` is a class method taking no arguments
    // and returning BOOL. It is safe to call from any thread — answering the
    // question is the whole point.
    unsafe {
        let cls = objc2::runtime::AnyClass::get(c"NSThread");
        match cls {
            Some(cls) => msg_send![cls, isMainThread],
            // No AppKit at all. Refusing is correct: everything below would be
            // undefined, and the caller reports "not ready" rather than
            // guessing.
            None => false,
        }
    }
}

/// The container's class: an `NSView` that answers `isFlipped` with `YES`.
///
/// Registered once, at first use. A plain `NSView` counts from the bottom-left,
/// so a plug-in that puts its own view at the origin — which is what every
/// `IPlugView` does — lands at the *bottom* of the panel and grows upward,
/// straight over the rest of the studio window. Every host parents plug-in
/// views into a flipped container for exactly this reason, and it is the other
/// half of the clip: clipping stops the overflow, this puts the editor where
/// the panel actually is.
fn container_class() -> Option<&'static objc2::runtime::AnyClass> {
    use std::sync::OnceLock;

    static CLASS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        // A name of our own so this can never collide with a plug-in's class.
        let Some(builder) = objc2::runtime::ClassBuilder::new(
            c"FutureboardPluginHostRegionView",
            objc2::runtime::AnyClass::get(c"NSView")
                .expect("AppKit is loaded before any editor is hosted"),
        ) else {
            // Already registered by an earlier process-wide init, which is the
            // only way `new` fails here; take the existing one.
            return objc2::runtime::AnyClass::get(c"FutureboardPluginHostRegionView")
                .map(|cls| cls as *const _ as usize)
                .unwrap_or(0);
        };
        let mut builder = builder;
        // SAFETY: `-[NSView isFlipped]` is a BOOL getter taking no arguments;
        // this override matches that signature exactly.
        unsafe {
            builder.add_method(
                objc2::sel!(isFlipped),
                is_flipped as unsafe extern "C-unwind" fn(_, _) -> _,
            );
        }
        builder.register() as *const _ as usize
    });
    if ptr == 0 {
        return None;
    }
    // SAFETY: the pointer came from `register`/`AnyClass::get` above and a
    // registered class lives for the life of the process.
    Some(unsafe { &*(ptr as *const objc2::runtime::AnyClass) })
}

/// `-[FutureboardPluginHostRegionView isFlipped]`.
unsafe extern "C-unwind" fn is_flipped(
    _this: *mut AnyObject,
    _cmd: objc2::runtime::Sel,
) -> objc2::runtime::Bool {
    objc2::runtime::Bool::YES
}

/// An `NSView` this process owns, mounted inside the GPUI window's view, that a
/// plug-in's `IPlugView` is attached into.
pub struct MacHostRegion {
    container: Retained<NSObject>,
}

impl MacHostRegion {
    /// Create the container and add it as a subview of `parent_ns_view`.
    ///
    /// `parent_ns_view` is the pointer from `RawWindowHandle::AppKit`, which
    /// `gpui_macos` provides for its windows (`MacWindow`'s `HasWindowHandle`
    /// impl). It is borrowed for the duration of this call only — the container
    /// holds its own strong reference through the view hierarchy, exactly as
    /// any other subview does.
    ///
    /// Returns `None` off the main thread, on a null parent, or if AppKit is
    /// not present — never a panic, and never a partially mounted view.
    ///
    /// # Safety
    ///
    /// `parent_ns_view` must be a valid `NSView*` or null.
    pub unsafe fn mount(parent_ns_view: *mut AnyObject, frame: FramePoints) -> Option<Self> {
        if parent_ns_view.is_null() || !is_main_thread() {
            return None;
        }
        let cls = container_class()?;
        let rect = ns_rect(frame);

        // SAFETY: `-[NSView initWithFrame:]` on a freshly allocated view of our
        // own subclass. The result is owned (alloc/init), so
        // `Retained::from_raw` takes that ownership rather than adding a
        // reference.
        let container: *mut NSObject = unsafe {
            let allocated: *mut NSObject = msg_send![cls, alloc];
            msg_send![allocated, initWithFrame: rect]
        };
        let container = unsafe { Retained::from_raw(container) }?;

        unsafe {
            // A layer-backed container keeps the plug-in's own layers composited
            // against something, rather than against whatever GPUI last drew
            // there — and gives the clip below something to clip with.
            let _: () = msg_send![&*container, setWantsLayer: true];
            // Clip to the panel. `NSView` does not clip its subviews by
            // default, and a plug-in's own view is whatever size the plug-in
            // wants: without this, an editor larger than the panel simply draws
            // over the rest of the studio window. This is what the Windows path
            // gets for free from child-window clipping.
            let layer: *mut AnyObject = msg_send![&*container, layer];
            if !layer.is_null() {
                let _: () = msg_send![&*layer, setMasksToBounds: true];
            }
            let parent: &AnyObject = &*parent_ns_view;
            let _: () = msg_send![parent, addSubview: &*container];
        }
        Some(Self { container })
    }

    /// The container's `NSView*`, for `IPlugView::attached(..., "NSView")`.
    ///
    /// Borrowed, never transferred: the caller passes it straight to the VST3
    /// bridge and must not retain or release it.
    pub fn view_ptr(&self) -> *mut AnyObject {
        Retained::as_ptr(&self.container) as *mut AnyObject
    }

    /// Move/resize the container. Main thread only; a no-op elsewhere.
    pub fn set_frame(&self, frame: FramePoints) {
        if !is_main_thread() {
            return;
        }
        let rect = ns_rect(frame);
        // SAFETY: `-[NSView setFrame:]` with an NSRect, on a view we own.
        unsafe {
            let _: () = msg_send![&*self.container, setFrame: rect];
        }
    }

    /// Show or hide without detaching the plug-in's view — a background tab
    /// keeps its editor attached (see [`super::HostRegionState::Hidden`]).
    pub fn set_hidden(&self, hidden: bool) {
        if !is_main_thread() {
            return;
        }
        // SAFETY: `-[NSView setHidden:]` with a BOOL, on a view we own.
        unsafe {
            let _: () = msg_send![&*self.container, setHidden: hidden];
        }
    }

    /// The parent's own coordinate system, which decides how a region becomes a
    /// frame (see [`super::region_to_frame`]).
    ///
    /// # Safety
    ///
    /// `parent_ns_view` must be a valid `NSView*` or null.
    pub unsafe fn parent_geometry(parent_ns_view: *mut AnyObject) -> Option<ParentGeometry> {
        if parent_ns_view.is_null() || !is_main_thread() {
            return None;
        }
        // SAFETY: `-[NSView bounds]` and `-[NSView isFlipped]` on a valid view.
        unsafe {
            let parent: &AnyObject = &*parent_ns_view;
            let bounds: NSRect = msg_send![parent, bounds];
            let is_flipped: bool = msg_send![parent, isFlipped];
            Some(ParentGeometry {
                height_points: bounds.size.height,
                is_flipped,
            })
        }
    }

    /// The window's backing scale factor, for the pixel↔point conversion.
    ///
    /// # Safety
    ///
    /// `parent_ns_view` must be a valid `NSView*` or null.
    pub unsafe fn backing_scale(parent_ns_view: *mut AnyObject) -> Option<f64> {
        if parent_ns_view.is_null() || !is_main_thread() {
            return None;
        }
        // SAFETY: `-[NSView window]` may be nil (an unmounted view), which is
        // why the result is checked before `-[NSWindow backingScaleFactor]`.
        unsafe {
            let parent: &AnyObject = &*parent_ns_view;
            let window: *mut AnyObject = msg_send![parent, window];
            if window.is_null() {
                return None;
            }
            let scale: f64 = msg_send![&*window, backingScaleFactor];
            (scale.is_finite() && scale > 0.0).then_some(scale)
        }
    }
}

impl Drop for MacHostRegion {
    /// Take the container out of the view hierarchy before releasing it.
    ///
    /// Only from the main thread. Off it, the container is leaked rather than
    /// removed: an `NSView` torn out of a hierarchy from a worker thread
    /// corrupts AppKit's state for the whole window, and one leaked view is a
    /// far smaller problem than that. The guard should never fire — this type
    /// is created and dropped by GPUI window code — and it is here for the
    /// caller that is added later without noticing.
    fn drop(&mut self) {
        if !is_main_thread() {
            eprintln!(
                "[MacHostRegion] dropped off the main thread; leaking the \
                 container rather than mutating the view hierarchy"
            );
            // Deliberately leak: `Retained` would release on the wrong thread.
            let leaked = self.container.clone();
            std::mem::forget(leaked);
            return;
        }
        // SAFETY: `-[NSView removeFromSuperview]` on a view we own. Safe when
        // the view has no superview.
        unsafe {
            let _: () = msg_send![&*self.container, removeFromSuperview];
        }
    }
}

fn ns_rect(frame: FramePoints) -> NSRect {
    NSRect {
        origin: NSPoint {
            x: frame.x,
            y: frame.y,
        },
        size: NSSize {
            width: frame.width,
            height: frame.height,
        },
    }
}
