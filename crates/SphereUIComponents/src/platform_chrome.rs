//! Central platform chrome policy for Futureboard Native.
//!
//! All `cfg(target_os = …)` checks for titlebar / menubar / window controls live
//! here. UI code should call [`PlatformChromePolicy::current()`] instead of
//! scattering platform conditionals.

use gpui::{
    point, px, Pixels, Point, TitlebarOptions, WindowDecorations, WindowKind, WindowOptions,
};

/// Product name shared by native window chrome and OS-level window metadata.
pub const APP_WINDOW_TITLE: &str = "Futureboard Studio";

/// Add the product name to a tool or project window title without duplicating it.
pub fn branded_window_title(title: &str) -> String {
    if title == APP_WINDOW_TITLE || title.contains(APP_WINDOW_TITLE) {
        title.to_string()
    } else {
        format!("{title} — {APP_WINDOW_TITLE}")
    }
}

/// Shared titlebar height across platforms (matches GPUI chrome layout).
pub const TITLEBAR_HEIGHT_PX: f32 = 32.0;

// AppKit caption-button metrics, measured on macOS for a titled window with a
// transparent full-size-content titlebar: three 14pt buttons whose origins are
// 23pt apart, inset 9pt from the left and vertically centered in a 32pt band.
/// Left inset of the traffic-light group.
pub const MACOS_TRAFFIC_LIGHT_INSET_PX: f32 = 9.0;
/// Diameter of one traffic light.
pub const MACOS_TRAFFIC_LIGHT_DIAMETER_PX: f32 = 14.0;
/// Distance between two neighbouring traffic-light origins.
pub const MACOS_TRAFFIC_LIGHT_SPACING_PX: f32 = 23.0;
/// Right edge of the traffic-light group: close, minimize, zoom.
pub const MACOS_TRAFFIC_LIGHT_GROUP_RIGHT_PX: f32 = MACOS_TRAFFIC_LIGHT_INSET_PX
    + 2.0 * MACOS_TRAFFIC_LIGHT_SPACING_PX
    + MACOS_TRAFFIC_LIGHT_DIAMETER_PX;
/// Breathing room between the last traffic light and the drawn title, so the
/// title reads as a separate element instead of touching the zoom button.
pub const MACOS_TRAFFIC_LIGHT_TITLE_GAP_PX: f32 = 12.0;

/// macOS traffic-light reserved width in the custom titlebar row.
pub const MACOS_TRAFFIC_LIGHT_PADDING_PX: f32 =
    MACOS_TRAFFIC_LIGHT_GROUP_RIGHT_PX + MACOS_TRAFFIC_LIGHT_TITLE_GAP_PX;

/// Minimum left inset for external dialog titles (wizard, preferences) on Win/Linux.
pub const EXTERNAL_DIALOG_TITLE_PADDING_PX: f32 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformChromeKind {
    Windows,
    Linux,
    MacOS,
}

#[derive(Debug, Clone, Copy)]
pub struct PlatformChromePolicy {
    pub kind: PlatformChromeKind,
    pub show_in_window_menubar: bool,
    pub use_native_macos_menubar: bool,
    /// The drawn titlebar renders the caption controls itself.
    pub show_window_controls: bool,
    /// The window server draws the caption controls over the drawn titlebar, as
    /// macOS does with traffic lights. Distinct from `show_window_controls`:
    /// together they also describe a window that has no caption controls at all,
    /// where the drawn titlebar must supply its own close affordance.
    pub platform_caption_controls: bool,
    pub traffic_light_left_padding_px: f32,
    pub titlebar_height_px: f32,
}

impl PlatformChromePolicy {
    pub fn current() -> Self {
        platform_policy()
    }

    /// Chrome for external dialogs (wizard, preferences).
    pub fn external_dialog() -> Self {
        let main = Self::current();
        let traffic_light_left_padding_px = match main.kind {
            PlatformChromeKind::MacOS => MACOS_TRAFFIC_LIGHT_PADDING_PX,
            PlatformChromeKind::Windows | PlatformChromeKind::Linux => {
                EXTERNAL_DIALOG_TITLE_PADDING_PX
            }
        };
        Self {
            show_in_window_menubar: false,
            use_native_macos_menubar: false,
            traffic_light_left_padding_px,
            ..main
        }
    }

    /// Chrome for windows opened without any platform caption controls, so the
    /// drawn titlebar must not reserve macOS traffic-light space.
    pub fn chromeless_dialog() -> Self {
        Self {
            show_window_controls: false,
            platform_caption_controls: false,
            traffic_light_left_padding_px: EXTERNAL_DIALOG_TITLE_PADDING_PX,
            ..Self::external_dialog()
        }
    }

    /// Whether the drawn titlebar has to provide the only close affordance. True
    /// only when neither the titlebar nor the platform draws caption controls.
    pub fn needs_drawn_close_fallback(&self) -> bool {
        !self.show_window_controls && !self.platform_caption_controls
    }

    /// Whether a drawn titlebar may carry a leading product/dialog icon. macOS
    /// puts no icon in a window's title area, and the space left of the title
    /// already belongs to the traffic lights.
    pub fn show_titlebar_icon(&self) -> bool {
        self.kind != PlatformChromeKind::MacOS
    }

    /// Left padding for external dialog titlebars (traffic lights or minimum inset).
    pub fn external_titlebar_left_padding(&self) -> gpui::Pixels {
        self.traffic_light_left_padding()
    }

    pub fn titlebar_height(&self) -> gpui::Pixels {
        px(self.titlebar_height_px)
    }

    pub fn traffic_light_left_padding(&self) -> gpui::Pixels {
        px(self.traffic_light_left_padding_px)
    }

    /// `TitlebarOptions` for the main studio window.
    pub fn studio_titlebar_options() -> TitlebarOptions {
        let policy = Self::current();
        TitlebarOptions {
            title: Some(APP_WINDOW_TITLE.into()),
            // Windows: transparent titlebar + GPUI `WindowControlArea` hit-testing.
            // macOS: blend custom chrome with native traffic lights.
            // Linux: same client chrome path as Windows.
            appears_transparent: true,
            traffic_light_position: policy.native_traffic_light_position(),
        }
    }

    /// `TitlebarOptions` for wizard / settings dialogs.
    pub fn external_dialog_titlebar_options() -> TitlebarOptions {
        let policy = Self::external_dialog();
        TitlebarOptions {
            title: Some(APP_WINDOW_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: policy.native_traffic_light_position(),
        }
    }

    /// Window decorations for external dialogs.
    pub fn external_dialog_window_decorations() -> Option<WindowDecorations> {
        match Self::current().kind {
            PlatformChromeKind::MacOS => None,
            PlatformChromeKind::Windows | PlatformChromeKind::Linux => {
                Some(WindowDecorations::Client)
            }
        }
    }

    /// Whether a [`WindowKind::Dialog`] may be owned by the window that opened it.
    ///
    /// Windows hosts an owned dialog in a real Win32 dialog while GPUI keeps
    /// rendering the whole client surface, which is what Futureboard wants.
    /// AppKit instead presents an owned dialog as a sheet glued to the parent
    /// titlebar: it ignores the requested bounds, removes the window's standard
    /// buttons, and duplicates the dialog chrome GPUI already draws. Futureboard
    /// therefore uses an unparented dialog panel on macOS; GPUI keeps it at the
    /// native modal-panel level so floating utility windows cannot cover it.
    pub fn use_platform_owned_dialogs() -> bool {
        Self::current().kind != PlatformChromeKind::MacOS
    }

    /// Whether the OS may draw its own window frame (avoid duplicating GPUI WCO).
    pub fn use_client_window_decorations_for_studio() -> bool {
        matches!(
            Self::current().kind,
            PlatformChromeKind::Windows | PlatformChromeKind::Linux
        )
    }

    /// Where GPUI should place the macOS traffic lights inside the drawn titlebar.
    ///
    /// GPUI derives each button frame as
    /// `titlebar_height - position.y - button_height`, measured from the bottom of
    /// AppKit's titlebar band. That band is 32pt, the same height Futureboard
    /// draws, so centering in the drawn bar is a plain center of the diameter.
    /// Keeping the horizontal inset at AppKit's own value leaves the group exactly
    /// where every other macOS window puts it.
    fn native_traffic_light_position(&self) -> Option<Point<Pixels>> {
        if self.kind != PlatformChromeKind::MacOS {
            return None;
        }
        let centered_y = (TITLEBAR_HEIGHT_PX - MACOS_TRAFFIC_LIGHT_DIAMETER_PX) / 2.0;
        Some(point(px(MACOS_TRAFFIC_LIGHT_INSET_PX), px(centered_y)))
    }
}

#[cfg(target_os = "windows")]
fn platform_policy() -> PlatformChromePolicy {
    PlatformChromePolicy {
        kind: PlatformChromeKind::Windows,
        show_in_window_menubar: true,
        use_native_macos_menubar: false,
        show_window_controls: true,
        platform_caption_controls: false,
        traffic_light_left_padding_px: 0.0,
        titlebar_height_px: TITLEBAR_HEIGHT_PX,
    }
}

#[cfg(target_os = "linux")]
fn platform_policy() -> PlatformChromePolicy {
    PlatformChromePolicy {
        kind: PlatformChromeKind::Linux,
        show_in_window_menubar: true,
        use_native_macos_menubar: false,
        show_window_controls: true,
        platform_caption_controls: false,
        traffic_light_left_padding_px: 0.0,
        titlebar_height_px: TITLEBAR_HEIGHT_PX,
    }
}

#[cfg(target_os = "macos")]
fn platform_policy() -> PlatformChromePolicy {
    PlatformChromePolicy {
        kind: PlatformChromeKind::MacOS,
        show_in_window_menubar: false,
        use_native_macos_menubar: true,
        show_window_controls: false,
        // AppKit draws the traffic lights over the drawn titlebar.
        platform_caption_controls: true,
        traffic_light_left_padding_px: MACOS_TRAFFIC_LIGHT_PADDING_PX,
        titlebar_height_px: TITLEBAR_HEIGHT_PX,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn platform_policy() -> PlatformChromePolicy {
    PlatformChromePolicy {
        kind: PlatformChromeKind::Linux,
        show_in_window_menubar: true,
        use_native_macos_menubar: false,
        show_window_controls: true,
        platform_caption_controls: false,
        traffic_light_left_padding_px: 0.0,
        titlebar_height_px: TITLEBAR_HEIGHT_PX,
    }
}

/// Studio window options (main Futureboard window).
pub fn studio_window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(PlatformChromePolicy::studio_titlebar_options()),
        focus: true,
        // Open the studio window hidden. The OS otherwise shows an empty black
        // client area while StudioLayout's heavy first layout / workspace install
        // runs (black screen at init). The mount path reveals it after the first
        // frame paints via `window.activate_window()` (which applies the stored
        // initial placement and shows the window). The Welcome window opts back
        // into `show: true` in `welcome_window_options`.
        show: false,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        window_decorations: if PlatformChromePolicy::use_client_window_decorations_for_studio() {
            Some(WindowDecorations::Client)
        } else {
            None
        },
        ..Default::default()
    }
}

/// Wire macOS native menubar command dispatch to the studio layout entity.
pub fn register_studio_menu_dispatcher(
    studio: gpui::Entity<crate::layout::StudioLayout>,
    cx: &mut gpui::Context<crate::layout::StudioLayout>,
) {
    use std::sync::Arc;

    crate::native_macos_menu::set_command_dispatcher(Arc::new(move |command_id, app| {
        let owner_bounds = app
            .active_window()
            .and_then(|handle| handle.update(app, |_, window, _| window.bounds()).ok());
        let _ = studio.update(app, |this, cx| {
            this.dispatch_command_id_from_bounds(command_id, owner_bounds, cx);
            cx.notify();
        });
    }));
    crate::native_macos_menu::install_native_macos_menu(cx);
}

/// Partial options shared by GPUI-backed native dialogs. On Windows,
/// [`WindowKind::Dialog`] is hosted by a real Win32 dialog while GPUI continues
/// to render the complete client surface.
pub fn external_dialog_window_options_partial() -> WindowOptions {
    let platform_owned = PlatformChromePolicy::use_platform_owned_dialogs();
    WindowOptions {
        titlebar: Some(PlatformChromePolicy::external_dialog_titlebar_options()),
        focus: true,
        show: true,
        kind: WindowKind::Dialog,
        dialog_parenting: platform_owned,
        is_movable: true,
        is_resizable: false,
        is_minimizable: false,
        window_decorations: PlatformChromePolicy::external_dialog_window_decorations(),
        ..Default::default()
    }
}

/// Options for the pre-studio session transaction window (loading, switching,
/// closing a session). This window bridges a window handoff — Welcome → Studio
/// or Studio → Welcome — so it must outlive the surface it replaces and must not
/// offer a platform close affordance while the transaction owns its lifetime.
pub fn session_transaction_window_options() -> WindowOptions {
    let mut options = external_dialog_window_options_partial();
    options.is_resizable = false;
    options.is_minimizable = false;
    // Windows destroys owned dialogs together with their owner, and the owner
    // here is exactly the window being retired.
    options.dialog_parenting = false;

    if PlatformChromePolicy::current().kind == PlatformChromeKind::MacOS {
        // The unparented dialog panel remains visible during app deactivation.
        // Dropping `TitlebarOptions` also builds the style mask without
        // `NSWindowStyleMaskClosable`, so no traffic light can close a window
        // whose lifetime belongs to the running transaction.
        options.titlebar = None;
    }

    options
}

/// Top-level external tool window. Unlike [`external_dialog_window_options_partial`],
/// this is an independent application window: it is not modal/owned by the
/// Studio HWND and receives normal taskbar, minimize, maximize, and resize
/// behavior from the platform.
pub fn external_window_options_partial() -> WindowOptions {
    WindowOptions {
        titlebar: Some(PlatformChromePolicy::external_dialog_titlebar_options()),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        dialog_parenting: false,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        window_decorations: PlatformChromePolicy::external_dialog_window_decorations(),
        ..Default::default()
    }
}

/// Whether external tool windows (the Mixer, the MIDI Editor) take the
/// platform's own frame: its caption, caption buttons and a native menu bar
/// (see [`crate::native_win32_menu`]). Only on Windows; elsewhere they keep
/// the drawn chrome.
pub fn native_tool_window_frame() -> bool {
    PlatformChromePolicy::current().kind == PlatformChromeKind::Windows
}

/// [`external_window_options_partial`] with the platform's frame on Windows
/// ([`native_tool_window_frame`]): the OS draws the caption and its buttons,
/// so the window draws no titlebar of its own. `title` is the caption text.
pub fn native_tool_window_options_partial(title: impl Into<gpui::SharedString>) -> WindowOptions {
    let mut options = external_window_options_partial();
    if native_tool_window_frame() {
        options.titlebar = Some(TitlebarOptions {
            title: Some(title.into()),
            appears_transparent: false,
            traffic_light_position: None,
        });
        options.window_decorations = None;
    }
    options
}

#[cfg(test)]
mod chrome_policy_tests {
    use super::*;

    /// A window must offer exactly one close affordance: either the drawn
    /// titlebar renders caption controls, or the platform does, or neither does
    /// and the drawn titlebar has to supply the fallback.
    #[test]
    fn every_policy_has_exactly_one_source_of_caption_controls() {
        for policy in [
            PlatformChromePolicy::current(),
            PlatformChromePolicy::external_dialog(),
            PlatformChromePolicy::chromeless_dialog(),
        ] {
            assert!(
                !(policy.show_window_controls && policy.platform_caption_controls),
                "{policy:?} would draw caption controls next to the platform ones"
            );
            let sources = usize::from(policy.show_window_controls)
                + usize::from(policy.platform_caption_controls)
                + usize::from(policy.needs_drawn_close_fallback());
            assert_eq!(sources, 1, "{policy:?} must have one close affordance");
        }
    }

    #[test]
    fn a_window_without_caption_controls_keeps_its_own_close() {
        let chromeless = PlatformChromePolicy::chromeless_dialog();
        assert!(chromeless.needs_drawn_close_fallback());
        assert!(!chromeless.show_window_controls);
        assert!(!chromeless.platform_caption_controls);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_dialogs_leave_the_caption_to_the_traffic_lights() {
        let dialog = PlatformChromePolicy::external_dialog();
        assert!(dialog.platform_caption_controls);
        assert!(!dialog.show_window_controls);
        assert!(!dialog.needs_drawn_close_fallback());
        assert!(!dialog.show_titlebar_icon());
        assert_eq!(
            dialog.traffic_light_left_padding_px,
            MACOS_TRAFFIC_LIGHT_PADDING_PX
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_external_dialogs_use_unparented_modal_panels() {
        let options = external_dialog_window_options_partial();
        assert_eq!(options.kind, WindowKind::Dialog);
        assert!(!options.dialog_parenting);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_draw_their_own_caption_controls_and_icon() {
        let dialog = PlatformChromePolicy::external_dialog();
        assert!(dialog.show_window_controls);
        assert!(!dialog.platform_caption_controls);
        assert!(!dialog.needs_drawn_close_fallback());
        assert!(dialog.show_titlebar_icon());
    }

    /// The reserved inset has to clear the whole traffic-light group, not merely
    /// reach it: an inset equal to the group's right edge puts the title flush
    /// against the zoom button.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_drawn_title_clears_the_traffic_lights() {
        let dialog = PlatformChromePolicy::external_dialog();
        let position = dialog
            .native_traffic_light_position()
            .expect("macOS reports a traffic-light position");

        // GPUI lays the group out from `position.x` using AppKit's own spacing.
        let group_right = f32::from(position.x)
            + 2.0 * MACOS_TRAFFIC_LIGHT_SPACING_PX
            + MACOS_TRAFFIC_LIGHT_DIAMETER_PX;
        assert_eq!(group_right, MACOS_TRAFFIC_LIGHT_GROUP_RIGHT_PX);
        assert!(
            dialog.traffic_light_left_padding_px > group_right,
            "title starts at {} and would touch the zoom button ending at {group_right}",
            dialog.traffic_light_left_padding_px
        );
        assert_eq!(
            dialog.traffic_light_left_padding_px - group_right,
            MACOS_TRAFFIC_LIGHT_TITLE_GAP_PX
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn traffic_lights_sit_centered_in_the_drawn_titlebar() {
        let position = PlatformChromePolicy::current()
            .native_traffic_light_position()
            .expect("macOS reports a traffic-light position");

        // GPUI resolves the frame as `titlebar_height - position.y - diameter`
        // against AppKit's 32pt band, so equal gaps above and below the group mean
        // the resolved origin matches the requested offset.
        let origin_y = TITLEBAR_HEIGHT_PX - f32::from(position.y) - MACOS_TRAFFIC_LIGHT_DIAMETER_PX;
        assert_eq!(origin_y, f32::from(position.y));
        assert_eq!(
            2.0 * origin_y + MACOS_TRAFFIC_LIGHT_DIAMETER_PX,
            TITLEBAR_HEIGHT_PX
        );
    }
}
