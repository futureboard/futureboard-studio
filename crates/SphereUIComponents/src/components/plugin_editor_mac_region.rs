//! The AppKit container view that a plug-in's `NSView` is attached inside, and
//! the platform-neutral geometry and lifecycle that drive it.
//!
//! # Shape of this file
//!
//! Two halves, deliberately separated:
//!
//! * everything above `mod appkit` is plain data — lifecycle states, a rect, a
//!   scale factor, and the arithmetic that turns a GPUI region into an AppKit
//!   frame. It compiles and is tested on every target, because that arithmetic
//!   is where the bugs actually live: Retina scaling, AppKit's bottom-left
//!   origin, and a container that must not be positioned against the wrong
//!   coordinate system;
//! * `mod appkit` is `#[cfg(target_os = "macos")]` and is the only place that
//!   touches Objective-C. Nothing it uses leaks out — no `NSView`, no
//!   `Retained<_>`, no raw window handle variant crosses back over.
//!
//! # Why a container view at all
//!
//! `IPlugView::attached` wants a parent it can own and resize under. Handing it
//! the GPUI window's own view would put the plug-in's subviews in among the
//! ones GPUI draws with, and every layout pass would fight it. A container of
//! our own, positioned to the editor shell's plug-in region, keeps the two
//! hierarchies apart: GPUI owns everything outside it, the plug-in owns
//! everything inside it, and the only thing crossing the line is a frame.

/// Where the plug-in region sits, in the units GPUI reports it: physical
/// pixels, top-left origin, relative to the window's content area.
///
/// The same convention as the Windows content child's rect, so the editor
/// shell hands both backends the same numbers and neither has to know which is
/// listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionPx {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl RegionPx {
    pub fn is_valid(self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// An AppKit frame: points, and an origin already resolved against the parent's
/// coordinate system.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramePoints {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl FramePoints {
    fn approx_eq(self, other: Self) -> bool {
        const EPS: f64 = 1.0e-6;
        (self.x - other.x).abs() < EPS
            && (self.y - other.y).abs() < EPS
            && (self.width - other.width).abs() < EPS
            && (self.height - other.height).abs() < EPS
    }
}

/// Convert a GPUI region into the frame the container view should take.
///
/// Three things have to be right at once, and getting any one wrong puts the
/// editor somewhere plausible-looking but wrong:
///
/// * **scale.** GPUI measures in physical pixels; AppKit lays out in points. On
///   a Retina display those differ by the backing scale factor, so a region
///   used unscaled lands at half size on the screen it was measured for.
/// * **origin.** AppKit's default coordinate system starts at the bottom-left
///   and counts upward, while the region counts down from the top. A container
///   positioned without the flip sits mirrored about the window's middle —
///   correct at the exact centre, and further off the further from it, which is
///   the kind of wrong that survives a quick look.
/// * **whose flip.** A view can opt into top-left coordinates by answering
///   `isFlipped`, and GPUI's may. The frame is expressed in the *parent's*
///   system, so the parent's answer is the one that decides, not ours.
///
/// `parent_height_points` is the parent's own height, and is only consulted
/// when the parent is not flipped.
pub fn region_to_frame(
    region: RegionPx,
    scale: f64,
    parent_height_points: f64,
    parent_is_flipped: bool,
) -> FramePoints {
    // A scale of zero would divide the whole layout away; a device that reports
    // one is broken, but silently producing infinities is worse than pinning.
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let x = f64::from(region.x) / scale;
    let top = f64::from(region.y) / scale;
    let width = f64::from(region.width) / scale;
    let height = f64::from(region.height) / scale;
    let y = if parent_is_flipped {
        top
    } else {
        parent_height_points - (top + height)
    };
    FramePoints {
        x,
        y,
        width,
        height,
    }
}

/// Size in points that a plug-in's reported size in points should occupy, and
/// the physical-pixel size the shell should reserve for it.
///
/// `IPlugView::getSize` answers in points on macOS — the plug-in never sees
/// backing pixels — so a plug-in that asks for 820x560 wants 820x560 points,
/// which is 1640x1120 physical on a 2x display. The shell reserves physical
/// pixels, so it needs the multiplication done for it.
pub fn plugin_points_to_region_px(width_points: f64, height_points: f64, scale: f64) -> RegionPx {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    RegionPx {
        x: 0,
        y: 0,
        width: (width_points * scale).round().max(0.0) as i32,
        height: (height_points * scale).round().max(0.0) as i32,
    }
}

/// Lifecycle of the container view.
///
/// Ordered, and only moving forward except through `Destroyed`. The editor
/// shell reads it to decide what to draw, and the backend reads it to decide
/// whether attaching is even possible yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostRegionState {
    /// Nothing exists yet.
    Created,
    /// The container view exists and is a subview of the GPUI window's view.
    Mounted,
    /// The container has a valid non-empty frame, so `IPlugView::attached` has
    /// something real to attach to. Attaching to a zero-sized view is how a
    /// plug-in ends up believing its editor is 0x0.
    Ready,
    /// A plug-in view is attached and the container is visible.
    Visible,
    /// Attached but hidden — a background tab. The view stays attached: tearing
    /// it down and rebuilding it on every tab switch loses whatever transient
    /// state the plug-in's UI was holding.
    Hidden,
    /// The container is gone. Terminal.
    Destroyed,
}

impl HostRegionState {
    /// Whether a plug-in view may be attached now.
    pub fn can_attach(self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether a plug-in view is currently attached.
    pub fn is_attached(self) -> bool {
        matches!(self, Self::Visible | Self::Hidden)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Mounted => "mounted",
            Self::Ready => "ready",
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Destroyed => "destroyed",
        }
    }
}

/// The platform-neutral half of the host region: what state it is in, what
/// geometry it was last given, and what it should do next.
///
/// The AppKit object itself lives in [`appkit`]; this is the part the editor
/// shell talks to and the part the tests can reach.
#[derive(Debug, Clone)]
pub struct HostRegionModel {
    state: HostRegionState,
    region: RegionPx,
    scale: f64,
    applied: Option<FramePoints>,
}

impl Default for HostRegionModel {
    fn default() -> Self {
        Self {
            state: HostRegionState::Created,
            region: RegionPx::default(),
            scale: 1.0,
            applied: None,
        }
    }
}

impl HostRegionModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> HostRegionState {
        self.state
    }

    pub fn region(&self) -> RegionPx {
        self.region
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// The container view has been made a subview of the GPUI window's view.
    pub fn mounted(&mut self) {
        if self.state == HostRegionState::Created {
            self.state = HostRegionState::Mounted;
        }
    }

    /// New geometry from a GPUI layout pass.
    ///
    /// Returns the frame to apply, or `None` when nothing moved. Returning
    /// `None` for an unchanged frame is what keeps a resize log from firing
    /// every frame and, more importantly, keeps `IPlugView::onSize` from being
    /// called on every layout pass with the size it already has — some editors
    /// rebuild their whole UI on `onSize` and stutter for it.
    pub fn set_geometry(
        &mut self,
        region: RegionPx,
        scale: f64,
        parent: ParentGeometry,
    ) -> Option<FramePoints> {
        self.region = region;
        self.scale = scale;
        if region.is_valid() && self.state == HostRegionState::Mounted {
            self.state = HostRegionState::Ready;
        }
        if !region.is_valid() {
            return None;
        }
        let frame = region_to_frame(region, scale, parent.height_points, parent.is_flipped);
        match self.applied {
            Some(previous) if previous.approx_eq(frame) => None,
            _ => {
                self.applied = Some(frame);
                Some(frame)
            }
        }
    }

    /// A plug-in view was attached.
    pub fn attached(&mut self) {
        if self.state == HostRegionState::Ready {
            self.state = HostRegionState::Visible;
        }
    }

    pub fn set_visible(&mut self, visible: bool) {
        match (self.state, visible) {
            (HostRegionState::Visible, false) => self.state = HostRegionState::Hidden,
            (HostRegionState::Hidden, true) => self.state = HostRegionState::Visible,
            _ => {}
        }
    }

    /// The plug-in view was detached but the container survives, so the editor
    /// can be reopened without rebuilding it.
    pub fn detached(&mut self) {
        if self.state.is_attached() {
            self.state = if self.region.is_valid() {
                HostRegionState::Ready
            } else {
                HostRegionState::Mounted
            };
        }
    }

    pub fn destroyed(&mut self) {
        self.state = HostRegionState::Destroyed;
        self.applied = None;
    }
}

/// What the container's parent view reports about its own coordinate system.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParentGeometry {
    pub height_points: f64,
    pub is_flipped: bool,
}

#[cfg(target_os = "macos")]
pub mod appkit;

#[cfg(target_os = "macos")]
mod docked {
    use super::appkit::MacHostRegion;
    use super::{region_to_frame, RegionPx};

    /// A plug-in's view parked inside a panel of the studio's own window.
    ///
    /// The counterpart of `ContentChildHwnd` on Windows, and the reason macOS
    /// can have one at all: this is only ever used for a plug-in hosted *in this
    /// process*. An ARA plug-in is — it is bound to a clip by the app itself,
    /// never behind the plug-in-host bridge — so its `NSView` can be a subview
    /// of the app's window like any other. The bridged editors cannot, which is
    /// what the host-owned window exists for, and why that story is unchanged.
    ///
    /// Geometry is the caller's: it measures the panel and hands over a
    /// physical-pixel rect, exactly as the Windows path does. The conversion to
    /// AppKit's points and bottom-left origin happens here, once, through
    /// [`region_to_frame`].
    pub struct DockedPluginSurface {
        region: MacHostRegion,
    }

    impl DockedPluginSurface {
        /// Mount a container inside `parent_ns_view` at `rect`.
        ///
        /// `parent_ns_view` is the studio window's own view, as
        /// `RawWindowHandle::AppKit` reports it. `None` off the main thread, on
        /// a null parent, or when the container could not be made — never a
        /// panic and never a half-mounted view.
        pub fn create(parent_ns_view: u64, rect: RegionPx) -> Option<Self> {
            let parent = parent_ns_view as *mut objc2::runtime::AnyObject;
            let frame = Self::frame_for(parent, rect)?;
            // SAFETY: `parent` is the pointer GPUI reported for this window's
            // view, used on the thread that owns it.
            let region = unsafe { MacHostRegion::mount(parent, frame) }?;
            Some(Self { region })
        }

        /// The container's `NSView*`, for `IPlugView::attached(…, "NSView")`.
        ///
        /// Borrowed, never transferred: the plug-in attaches to it and lets go
        /// of it, and this surface destroys it.
        pub fn handle(&self) -> u64 {
            self.region.view_ptr() as u64
        }

        /// Move or resize the container to a freshly measured panel rect.
        pub fn set_bounds(&self, parent_ns_view: u64, rect: RegionPx) {
            let parent = parent_ns_view as *mut objc2::runtime::AnyObject;
            if let Some(frame) = Self::frame_for(parent, rect) {
                self.region.set_frame(frame);
            }
        }

        /// The AppKit frame for a measured region, in the parent's own terms.
        ///
        /// Both the scale and the parent's coordinate system are read from the
        /// parent rather than assumed: a Retina display and a flipped superview
        /// each put the container somewhere plausible-looking but wrong if
        /// guessed, and the two compose.
        fn frame_for(
            parent: *mut objc2::runtime::AnyObject,
            rect: RegionPx,
        ) -> Option<super::FramePoints> {
            if !rect.is_valid() {
                return None;
            }
            // SAFETY: `parent` is a live `NSView*` or null; both are handled.
            let geometry = unsafe { MacHostRegion::parent_geometry(parent) }?;
            // SAFETY: same.
            let scale = unsafe { MacHostRegion::backing_scale(parent) }.unwrap_or(1.0);
            Some(region_to_frame(
                rect,
                scale,
                geometry.height_points,
                geometry.is_flipped,
            ))
        }
    }
}

#[cfg(target_os = "macos")]
pub use docked::DockedPluginSurface;

#[cfg(test)]
mod tests {
    use super::*;

    const FLIPPED: ParentGeometry = ParentGeometry {
        height_points: 800.0,
        is_flipped: true,
    };
    const BOTTOM_LEFT: ParentGeometry = ParentGeometry {
        height_points: 800.0,
        is_flipped: false,
    };

    fn region(x: i32, y: i32, w: i32, h: i32) -> RegionPx {
        RegionPx {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_flipped_parent_takes_the_region_as_it_comes() {
        let frame = region_to_frame(region(10, 20, 300, 200), 1.0, 800.0, true);
        assert_eq!(
            frame,
            FramePoints {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 200.0
            }
        );
    }

    #[test]
    fn a_bottom_left_parent_flips_the_origin_about_its_own_height() {
        // 20 points down from an 800-point top edge, for a 200-point box, is
        // 580 points up from the bottom. Getting this wrong mirrors the editor
        // about the window's middle — right at the centre, wrong everywhere
        // else, which is exactly the kind of error that survives a glance.
        let frame = region_to_frame(region(10, 20, 300, 200), 1.0, 800.0, false);
        assert_eq!(frame.y, 580.0);
        assert_eq!(frame.x, 10.0);
    }

    #[test]
    fn retina_divides_the_region_into_points() {
        // GPUI measures physical pixels; AppKit lays out in points. A 2x
        // display makes a 600x400 region a 300x200 view.
        let frame = region_to_frame(region(20, 40, 600, 400), 2.0, 400.0, true);
        assert_eq!(
            frame,
            FramePoints {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 200.0
            }
        );
    }

    #[test]
    fn retina_and_the_flip_compose() {
        // Both at once, which is the case on every modern Mac.
        let frame = region_to_frame(region(20, 40, 600, 400), 2.0, 400.0, false);
        assert_eq!(frame.x, 10.0);
        assert_eq!(frame.width, 300.0);
        // 400 - (20 + 200)
        assert_eq!(frame.y, 180.0);
    }

    #[test]
    fn a_broken_scale_factor_does_not_divide_the_layout_away() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let frame = region_to_frame(region(0, 0, 100, 100), bad, 100.0, true);
            assert!(frame.width.is_finite() && frame.width > 0.0, "scale {bad}");
            assert_eq!(frame.width, 100.0, "scale {bad} should pin to 1.0");
        }
    }

    #[test]
    fn a_plugins_point_size_becomes_the_physical_region_the_shell_reserves() {
        // `IPlugView::getSize` answers in points on macOS, so an 820x560 editor
        // needs 1640x1120 physical pixels reserved on a 2x display. Reserving
        // 820x560 physical would attach the plug-in into half the room it
        // asked for.
        assert_eq!(
            plugin_points_to_region_px(820.0, 560.0, 2.0),
            region(0, 0, 1640, 1120)
        );
        assert_eq!(
            plugin_points_to_region_px(820.0, 560.0, 1.0),
            region(0, 0, 820, 560)
        );
    }

    #[test]
    fn geometry_is_only_applied_when_it_moves() {
        let mut model = HostRegionModel::new();
        model.mounted();
        let first = model.set_geometry(region(0, 0, 400, 300), 1.0, FLIPPED);
        assert!(first.is_some(), "the first frame always applies");
        assert_eq!(
            model.set_geometry(region(0, 0, 400, 300), 1.0, FLIPPED),
            None,
            "an unchanged frame must not reach IPlugView::onSize — some editors \
             rebuild their whole UI on it"
        );
        assert!(model
            .set_geometry(region(0, 0, 401, 300), 1.0, FLIPPED)
            .is_some());
    }

    #[test]
    fn the_same_region_moves_when_the_display_scale_changes() {
        // Dragging the window to a monitor with a different scale changes the
        // frame even though the region did not: same pixels, different points.
        let mut model = HostRegionModel::new();
        model.mounted();
        model.set_geometry(region(0, 0, 400, 300), 1.0, FLIPPED);
        assert!(
            model
                .set_geometry(region(0, 0, 400, 300), 2.0, FLIPPED)
                .is_some(),
            "a scale change must re-apply the frame"
        );
    }

    #[test]
    fn the_region_is_not_ready_until_it_has_a_size() {
        let mut model = HostRegionModel::new();
        assert_eq!(model.state(), HostRegionState::Created);
        assert!(!model.state().can_attach());

        model.mounted();
        assert_eq!(model.state(), HostRegionState::Mounted);

        // A zero-sized region must not become Ready: attaching there is how a
        // plug-in ends up convinced its editor is 0x0.
        assert_eq!(model.set_geometry(region(0, 0, 0, 0), 2.0, FLIPPED), None);
        assert_eq!(model.state(), HostRegionState::Mounted);
        assert!(!model.state().can_attach());

        model.set_geometry(region(0, 0, 640, 480), 2.0, FLIPPED);
        assert_eq!(model.state(), HostRegionState::Ready);
        assert!(model.state().can_attach());
    }

    #[test]
    fn closing_the_editor_keeps_the_container_for_the_next_open() {
        // Detach is an editor-lifecycle event, not a DSP one. The container
        // survives so reopening does not rebuild it — and nothing here touches
        // the processor.
        let mut model = HostRegionModel::new();
        model.mounted();
        model.set_geometry(region(0, 0, 640, 480), 2.0, FLIPPED);
        model.attached();
        assert_eq!(model.state(), HostRegionState::Visible);
        assert!(model.state().is_attached());

        model.detached();
        assert_eq!(
            model.state(),
            HostRegionState::Ready,
            "the container stays mounted and sized, ready to attach again"
        );
        assert!(model.state().can_attach());

        model.attached();
        assert_eq!(model.state(), HostRegionState::Visible);
    }

    #[test]
    fn a_background_tab_hides_without_detaching() {
        // Tearing the view down on every tab switch loses whatever transient
        // state the plug-in's UI was holding.
        let mut model = HostRegionModel::new();
        model.mounted();
        model.set_geometry(region(0, 0, 640, 480), 1.0, BOTTOM_LEFT);
        model.attached();

        model.set_visible(false);
        assert_eq!(model.state(), HostRegionState::Hidden);
        assert!(model.state().is_attached(), "hidden is still attached");

        model.set_visible(true);
        assert_eq!(model.state(), HostRegionState::Visible);
    }

    #[test]
    fn destroyed_is_terminal() {
        let mut model = HostRegionModel::new();
        model.mounted();
        model.set_geometry(region(0, 0, 640, 480), 1.0, FLIPPED);
        model.attached();
        model.destroyed();
        assert_eq!(model.state(), HostRegionState::Destroyed);

        // Nothing walks it back out.
        model.mounted();
        model.attached();
        model.set_visible(true);
        model.detached();
        assert_eq!(model.state(), HostRegionState::Destroyed);
    }
}
