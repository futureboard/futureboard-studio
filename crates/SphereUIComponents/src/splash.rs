//! Standalone boot splash window.
//!
//! Shown immediately at process launch, before Welcome or Studio windows exist.
//! Displays the splash artwork at logical 402×525: the @1x copy on a 100%
//! display, the @2x art above that, and the boot status under the artwork's
//! text column.

use gpui::{
    div, img, px, rgb, App, AppContext, Context, IntoElement, ObjectFit, ParentElement, Render,
    SharedString, Styled, StyledImage, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, WindowOptions,
};

use crate::edition::{self, EditionInfo, LicenseDisplayState};
use crate::embedded_assets::{
    splash_image_available, splash_image_for_scale, SPLASH_CE_IMAGE_PATH, SPLASH_IMAGE_PATH,
    SPLASH_PROFESSIONAL_IMAGE_PATH,
};
use crate::theme::{self, Colors};
use crate::window_position::centered_window_bounds;

/// Splash window size in logical pixels: the artwork is 804×1050 @2x and
/// 402×525 @1x.
pub const SPLASH_WIDTH: f32 = 402.0;
pub const SPLASH_HEIGHT: f32 = 525.0;
/// The artwork's left text margin, which the status line lines up with.
const ART_MARGIN_X: f32 = 26.0;
const STATUS_BOTTOM: f32 = 24.0;
/// Status text drawn over the artwork's own flat background, so it is
/// matched to the art rather than to the theme.
const STATUS_TEXT: u32 = 0x8d9099;

pub struct SplashWindow {
    image_path: &'static str,
    image_available: bool,
    status: SharedString,
}

impl SplashWindow {
    pub fn new() -> Self {
        let image_path = splash_image_path_for_edition(edition::current_edition_info().as_ref());
        let (image_path, image_available) = if splash_image_available(image_path) {
            (image_path, true)
        } else {
            (SPLASH_IMAGE_PATH, splash_image_available(SPLASH_IMAGE_PATH))
        };
        if !image_available {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                eprintln!("[splash] missing splash assets; using fallback panel");
            });
        }
        Self {
            image_path,
            image_available,
            status: SharedString::default(),
        }
    }

    /// Update the boot status line shown at the bottom of the splash.
    pub fn set_status(&mut self, status: impl Into<SharedString>) {
        self.status = status.into();
    }
}

/// Select the branded splash from the same verified license snapshot used by
/// Settings. A compiled Professional feature alone never grants Professional branding:
/// missing, invalid, or expired licenses all remain Community Edition.
fn splash_image_path_for_edition(info: Option<&EditionInfo>) -> &'static str {
    if info.is_some_and(|info| {
        info.license
            .as_ref()
            .is_some_and(|license| license.state == LicenseDisplayState::Active)
    }) {
        SPLASH_PROFESSIONAL_IMAGE_PATH
    } else {
        SPLASH_CE_IMAGE_PATH
    }
}

impl Render for SplashWindow {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(gpui::transparent_black())
            .font(theme::ui_font());

        if self.image_available {
            root = root.child(
                img(SharedString::from(splash_image_for_scale(
                    self.image_path,
                    window.scale_factor(),
                )))
                .w(px(SPLASH_WIDTH))
                .h(px(SPLASH_HEIGHT))
                .object_fit(ObjectFit::Contain),
            );
        } else {
            root = root.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(Colors::surface_base())
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child("Futureboard Studio"),
                    ),
            );
        }

        if !self.status.is_empty() {
            // The artwork leaves its lower part empty: the boot status sits
            // there, on the same left edge as the title above it.
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(STATUS_BOTTOM))
                    .left(px(ART_MARGIN_X))
                    .right(px(ART_MARGIN_X))
                    .truncate()
                    .text_size(px(crate::theme::typography::UI_XS))
                    .text_color(rgb(STATUS_TEXT))
                    .child(self.status.clone()),
            );
        }

        root
    }
}

/// Borderless centered splash shell. `PopUp` uses `WS_EX_TOOLWINDOW` on Windows
/// so the splash does not claim a separate taskbar button.
pub fn splash_window_options(cx: &mut App) -> WindowOptions {
    let bounds = centered_window_bounds(None, gpui::size(px(SPLASH_WIDTH), px(SPLASH_HEIGHT)), cx);
    WindowOptions {
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_background: WindowBackgroundAppearance::Transparent,
        window_decorations: None,
        ..Default::default()
    }
}

pub struct SplashWindowHandle {
    window: WindowHandle<SplashWindow>,
}

impl SplashWindowHandle {
    pub fn open(cx: &mut App) -> Result<Self, String> {
        let options = splash_window_options(cx);
        let handle = cx
            .open_window(options, |_window, cx| cx.new(|_| SplashWindow::new()))
            .map_err(|e| e.to_string())?;
        crate::boot::log("splash window shown");
        Ok(Self { window: handle })
    }

    /// Update the splash boot status line (e.g. "Opening Studio…").
    pub fn set_status(&self, cx: &mut App, status: impl Into<SharedString>) {
        let status = status.into();
        let _ = self.window.update(cx, |splash, _window, cx| {
            splash.set_status(status);
            cx.notify();
        });
    }

    pub fn close(self, cx: &mut App) {
        let _ = self.window.update(cx, |_splash, window, _cx| {
            window.remove_window();
        });
        crate::boot::log("splash window closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edition::LicenseDisplay;

    fn edition_with_license(state: LicenseDisplayState) -> EditionInfo {
        EditionInfo {
            edition: "Professional",
            app_version: "test".to_string(),
            license: Some(LicenseDisplay {
                state,
                licensee: None,
                entitlements: Vec::new(),
                expires_at: None,
            }),
            audio_engine: None,
        }
    }

    #[test]
    fn community_or_unlicensed_build_uses_ce_splash() {
        assert_eq!(splash_image_path_for_edition(None), SPLASH_CE_IMAGE_PATH);

        let unlicensed = EditionInfo {
            edition: "Professional",
            app_version: "test".to_string(),
            license: None,
            audio_engine: None,
        };
        assert_eq!(
            splash_image_path_for_edition(Some(&unlicensed)),
            SPLASH_CE_IMAGE_PATH
        );
    }

    #[test]
    fn a_100_percent_display_gets_the_1x_artwork() {
        for path in [
            SPLASH_IMAGE_PATH,
            SPLASH_CE_IMAGE_PATH,
            SPLASH_PROFESSIONAL_IMAGE_PATH,
        ] {
            let one_x = splash_image_for_scale(path, 1.0);
            assert_ne!(one_x, path);
            assert!(one_x.ends_with("_1x.png"));
            assert!(splash_image_available(one_x));
            assert_eq!(splash_image_for_scale(path, 1.25), path);
            assert_eq!(splash_image_for_scale(path, 2.0), path);
        }
    }

    #[test]
    fn only_an_active_license_uses_professional_splash() {
        let active = edition_with_license(LicenseDisplayState::Active);
        assert_eq!(
            splash_image_path_for_edition(Some(&active)),
            SPLASH_PROFESSIONAL_IMAGE_PATH
        );

        let expired = edition_with_license(LicenseDisplayState::Expired);
        assert_eq!(
            splash_image_path_for_edition(Some(&expired)),
            SPLASH_CE_IMAGE_PATH
        );
    }
}
