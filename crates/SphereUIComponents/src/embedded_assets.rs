use crate::assets;
use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// Asset path for the boot splash image, resolvable via `gpui::img(...)`.
pub const SPLASH_IMAGE_PATH: &str = "images/splash.png";
/// Community Edition boot splash image.
pub const SPLASH_CE_IMAGE_PATH: &str = "images/Splash_CE.png";
/// Licensed Professional Edition boot splash image.
pub const SPLASH_PROFESSIONAL_IMAGE_PATH: &str = "images/Splash_Professional.png";
/// Futureboard application icon/logo from packages/assets, resolvable via `gpui::img(...)`.
pub const APP_LOGO_PATH: &str = "images/app.png";
/// Futureboard horizontal wordmark from packages/assets, embedded for app chrome.
pub const LOGO_TEXT_PATH: &str = "images/logo-text.png";

/// Splash PNG, embedded at compile time so it ships inside the binary (no
/// runtime file dependency on the source tree / install layout).
static SPLASH_PNG: &[u8] = include_bytes!("../../../packages/shared/images/splash.png");
static SPLASH_CE_PNG: &[u8] = include_bytes!("../../../packages/shared/images/Splash_CE.png");
static SPLASH_PROFESSIONAL_PNG: &[u8] =
    include_bytes!("../../../packages/shared/images/Splash_Professional.png");
static APP_LOGO_PNG: &[u8] = include_bytes!("../../../packages/assets/app.png");
// UI-sized 2x derivative (398x36) avoids asking the renderer to minify the
// 3487x315 source at runtime, which produced visibly jagged text at 100% DPI.
static LOGO_TEXT_PNG: &[u8] = include_bytes!("../../../packages/assets/LogoText.UI@2x.png");

pub fn splash_image_available(path: &str) -> bool {
    match path {
        SPLASH_IMAGE_PATH => !SPLASH_PNG.is_empty(),
        SPLASH_CE_IMAGE_PATH => !SPLASH_CE_PNG.is_empty(),
        SPLASH_PROFESSIONAL_IMAGE_PATH => !SPLASH_PROFESSIONAL_PNG.is_empty(),
        _ => false,
    }
}

pub struct EmbeddedAssets;

impl EmbeddedAssets {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmbeddedAssets {
    fn default() -> Self {
        Self::new()
    }
}

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == SPLASH_IMAGE_PATH {
            return Ok(Some(Cow::Borrowed(SPLASH_PNG)));
        }
        if path == SPLASH_CE_IMAGE_PATH {
            return Ok(Some(Cow::Borrowed(SPLASH_CE_PNG)));
        }
        if path == SPLASH_PROFESSIONAL_IMAGE_PATH {
            return Ok(Some(Cow::Borrowed(SPLASH_PROFESSIONAL_PNG)));
        }
        if path == APP_LOGO_PATH {
            return Ok(Some(Cow::Borrowed(APP_LOGO_PNG)));
        }
        if path == LOGO_TEXT_PATH {
            return Ok(Some(Cow::Borrowed(LOGO_TEXT_PNG)));
        }
        if let Some(svg) = assets::audio_repair_icon(path) {
            return Ok(Some(Cow::Borrowed(svg.as_bytes())));
        }
        let bytes = match path {
            assets::ICON_PLAY_PATH => Some(assets::icons::PLAY.as_bytes()),
            assets::ICON_LAYERS_PATH => Some(assets::icons::LAYERS.as_bytes()),
            assets::ICON_PALETTE_PATH => Some(assets::icons::PALETTE.as_bytes()),
            assets::ICON_MOVE_HORIZONTAL_PATH => Some(assets::icons::MOVE_HORIZONTAL.as_bytes()),
            assets::ICON_TYPE_PATH => Some(assets::icons::TYPE.as_bytes()),
            assets::ICON_LIST_MUSIC_PATH => Some(assets::icons::LIST_MUSIC.as_bytes()),
            assets::ICON_GAUGE_PATH => Some(assets::icons::GAUGE.as_bytes()),
            assets::ICON_PAUSE_PATH => Some(assets::icons::PAUSE.as_bytes()),
            assets::ICON_SQUARE_PATH => Some(assets::icons::SQUARE.as_bytes()),
            assets::ICON_CIRCLE_PATH => Some(assets::icons::CIRCLE.as_bytes()),
            assets::ICON_SKIP_BACK_PATH => Some(assets::icons::SKIP_BACK.as_bytes()),
            assets::ICON_REPEAT_PATH => Some(assets::icons::REPEAT.as_bytes()),
            assets::ICON_REPEAT2_PATH => Some(assets::icons::REPEAT2.as_bytes()),
            assets::ICON_TIMER_PATH => Some(assets::icons::TIMER.as_bytes()),
            assets::ICON_METRONOME_PATH => Some(assets::icons::METRONOME.as_bytes()),
            assets::ICON_SAVE_PATH => Some(assets::icons::SAVE.as_bytes()),
            assets::ICON_FOLDER_PATH => Some(assets::icons::FOLDER.as_bytes()),
            assets::ICON_FOLDER_OPEN_PATH => Some(assets::icons::FOLDER_OPEN.as_bytes()),
            assets::ICON_SHARE_PATH => Some(assets::icons::SHARE.as_bytes()),
            assets::ICON_PANEL_BOTTOM_PATH => Some(assets::icons::PANEL_BOTTOM.as_bytes()),
            assets::ICON_PANEL_RIGHT_PATH => Some(assets::icons::PANEL_RIGHT.as_bytes()),
            assets::ICON_BUG_PATH => Some(assets::icons::BUG.as_bytes()),
            assets::ICON_MAXIMIZE_PATH => Some(assets::icons::GENERIC_MAXIMIZE.as_bytes()),
            assets::ICON_MINIMIZE_PATH => Some(assets::icons::GENERIC_MINIMIZE.as_bytes()),
            assets::ICON_RESTORE_PATH => Some(assets::icons::GENERIC_RESTORE.as_bytes()),
            assets::ICON_X_PATH => Some(assets::icons::GENERIC_CLOSE.as_bytes()),
            assets::ICON_MINUS_PATH => Some(assets::icons::MINUS.as_bytes()),
            assets::ICON_MENU_PATH => Some(assets::icons::MENU.as_bytes()),
            assets::ICON_SEARCH_PATH => Some(assets::icons::SEARCH.as_bytes()),
            assets::ICON_SCAN_SEARCH_PATH => Some(assets::icons::SCAN_SEARCH.as_bytes()),
            assets::ICON_LOCK_PATH => Some(assets::icons::LOCK.as_bytes()),
            assets::ICON_LOCK_OPEN_PATH => Some(assets::icons::LOCK_OPEN.as_bytes()),
            assets::ICON_DICES_PATH => Some(assets::icons::DICES.as_bytes()),
            assets::ICON_REFRESH_CW_PATH => Some(assets::icons::REFRESH_CW.as_bytes()),
            assets::ICON_ARROW_LEFT_RIGHT_PATH => Some(assets::icons::ARROW_LEFT_RIGHT.as_bytes()),
            assets::ICON_POWER_PATH => Some(assets::icons::POWER.as_bytes()),
            assets::ICON_TRASH_PATH => Some(assets::icons::TRASH.as_bytes()),
            assets::ICON_CHEVRON_LEFT_PATH => Some(assets::icons::CHEVRON_LEFT.as_bytes()),
            assets::ICON_CLOSE_SMALL_PATH => Some(assets::icons::X.as_bytes()),

            // New ones
            assets::ICON_MOUSE_POINTER_PATH => Some(assets::icons::MOUSE_POINTER.as_bytes()),
            assets::ICON_PENCIL_PATH => Some(assets::icons::PENCIL.as_bytes()),
            assets::ICON_SCISSORS_PATH => Some(assets::icons::SCISSORS.as_bytes()),
            assets::ICON_LINK_PATH => Some(assets::icons::LINK.as_bytes()),
            assets::ICON_VOLUME_X_PATH => Some(assets::icons::VOLUME_X.as_bytes()),
            assets::ICON_CLOCK_PATH => Some(assets::icons::CLOCK.as_bytes()),
            assets::ICON_AUTOMATION_PATH => Some(assets::icons::AUTOMATION.as_bytes()),
            assets::ICON_USER_PATH => Some(assets::icons::USER.as_bytes()),
            assets::ICON_LOG_OUT_PATH => Some(assets::icons::LOG_OUT.as_bytes()),
            assets::ICON_SLIDERS_HORIZONTAL_PATH => {
                Some(assets::icons::SLIDERS_HORIZONTAL.as_bytes())
            }
            assets::ICON_SPARKLES_PATH => Some(assets::icons::SPARKLES.as_bytes()),
            assets::ICON_PLUS_PATH => Some(assets::icons::PLUS.as_bytes()),
            assets::ICON_PLUG_PATH => Some(assets::icons::PLUG.as_bytes()),
            assets::ICON_ROUTE_PATH => Some(assets::icons::ROUTE.as_bytes()),
            assets::ICON_MIC_PATH => Some(assets::icons::MIC.as_bytes()),
            assets::ICON_FILM_PATH => Some(assets::icons::FILM.as_bytes()),
            assets::ICON_CPU_PATH => Some(assets::icons::CPU.as_bytes()),
            assets::ICON_MEMORY_STICK_PATH => Some(assets::icons::MEMORY_STICK.as_bytes()),
            assets::ICON_AUDIO_LINES_PATH => Some(assets::icons::AUDIO_LINES.as_bytes()),
            assets::ICON_HARD_DRIVE_PATH => Some(assets::icons::HARD_DRIVE.as_bytes()),
            assets::ICON_MUSIC_PATH => Some(assets::icons::MUSIC.as_bytes()),
            assets::ICON_KEYBOARD_PATH => Some(assets::icons::KEYBOARD.as_bytes()),
            assets::ICON_GIT_MERGE_PATH => Some(assets::icons::GIT_MERGE.as_bytes()),
            assets::ICON_GIT_FORK_PATH => Some(assets::icons::GIT_FORK.as_bytes()),
            assets::ICON_CORNER_DOWN_LEFT_PATH => Some(assets::icons::CORNER_DOWN_LEFT.as_bytes()),
            assets::ICON_VOLUME_2_PATH => Some(assets::icons::VOLUME_2.as_bytes()),
            assets::ICON_CIRCLE_DOT_PATH => Some(assets::icons::CIRCLE_DOT.as_bytes()),
            assets::ICON_MAGNET_PATH => Some(assets::icons::MAGNET.as_bytes()),
            assets::ICON_GRIP_VERTICAL_PATH => Some(assets::icons::GRIP_VERTICAL.as_bytes()),
            assets::ICON_FILE_PATH => Some(assets::icons::FILE.as_bytes()),
            assets::ICON_CHEVRON_RIGHT_PATH => Some(assets::icons::CHEVRON_RIGHT.as_bytes()),
            assets::ICON_CHEVRON_DOWN_PATH => Some(assets::icons::CHEVRON_DOWN.as_bytes()),
            assets::ICON_CHECK_PATH => Some(assets::icons::CHECK.as_bytes()),
            assets::ICON_STAR_PATH => Some(assets::icons::STAR.as_bytes()),
            assets::ICON_NEWSPAPER_PATH => Some(assets::icons::NEWSPAPER.as_bytes()),
            assets::ICON_PLAYHEAD_HANDLE_PATH => Some(assets::icons::PLAYHEAD_HANDLE.as_bytes()),
            assets::ICON_PLUGIN_CLAP_PATH => Some(assets::icons::PLUGIN_CLAP.as_bytes()),
            assets::ICON_PLUGIN_VST3_PATH => Some(assets::icons::PLUGIN_VST3.as_bytes()),
            assets::TIMELINE_SCROLL_PATH => Some(assets::icons::TIMELINE_SCROLL.as_bytes()),

            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let all_paths = [
            APP_LOGO_PATH,
            LOGO_TEXT_PATH,
            SPLASH_IMAGE_PATH,
            SPLASH_CE_IMAGE_PATH,
            SPLASH_PROFESSIONAL_IMAGE_PATH,
            assets::ICON_PLAY_PATH,
            assets::ICON_PAUSE_PATH,
            assets::ICON_SQUARE_PATH,
            assets::ICON_CIRCLE_PATH,
            assets::ICON_SKIP_BACK_PATH,
            assets::ICON_REPEAT_PATH,
            assets::ICON_REPEAT2_PATH,
            assets::ICON_TIMER_PATH,
            assets::ICON_METRONOME_PATH,
            assets::ICON_SAVE_PATH,
            assets::ICON_FOLDER_PATH,
            assets::ICON_FOLDER_OPEN_PATH,
            assets::ICON_SHARE_PATH,
            assets::ICON_PANEL_BOTTOM_PATH,
            assets::ICON_PANEL_RIGHT_PATH,
            assets::ICON_BUG_PATH,
            assets::ICON_MAXIMIZE_PATH,
            assets::ICON_MINIMIZE_PATH,
            assets::ICON_RESTORE_PATH,
            assets::ICON_X_PATH,
            assets::ICON_MINUS_PATH,
            assets::ICON_MENU_PATH,
            assets::ICON_SEARCH_PATH,
            assets::ICON_SCAN_SEARCH_PATH,
            assets::ICON_LOCK_PATH,
            assets::ICON_LOCK_OPEN_PATH,
            assets::ICON_DICES_PATH,
            assets::ICON_REFRESH_CW_PATH,
            assets::ICON_ARROW_LEFT_RIGHT_PATH,
            assets::ICON_MOUSE_POINTER_PATH,
            assets::ICON_PENCIL_PATH,
            assets::ICON_SCISSORS_PATH,
            assets::ICON_LINK_PATH,
            assets::ICON_VOLUME_X_PATH,
            assets::ICON_CLOCK_PATH,
            assets::ICON_AUTOMATION_PATH,
            assets::ICON_SLIDERS_HORIZONTAL_PATH,
            assets::ICON_SPARKLES_PATH,
            assets::ICON_PLUS_PATH,
            assets::ICON_PLUG_PATH,
            assets::ICON_ROUTE_PATH,
            assets::ICON_MIC_PATH,
            assets::ICON_FILM_PATH,
            assets::ICON_CPU_PATH,
            assets::ICON_MEMORY_STICK_PATH,
            assets::ICON_AUDIO_LINES_PATH,
            assets::ICON_HARD_DRIVE_PATH,
            assets::ICON_MUSIC_PATH,
            assets::ICON_KEYBOARD_PATH,
            assets::ICON_GIT_MERGE_PATH,
            assets::ICON_GIT_FORK_PATH,
            assets::ICON_CORNER_DOWN_LEFT_PATH,
            assets::ICON_VOLUME_2_PATH,
            assets::ICON_CIRCLE_DOT_PATH,
            assets::ICON_MAGNET_PATH,
            assets::ICON_GRIP_VERTICAL_PATH,
            assets::ICON_FILE_PATH,
            assets::ICON_CHEVRON_RIGHT_PATH,
            assets::ICON_CHEVRON_DOWN_PATH,
            assets::ICON_CHECK_PATH,
            assets::ICON_STAR_PATH,
            assets::ICON_NEWSPAPER_PATH,
            assets::ICON_PLAYHEAD_HANDLE_PATH,
            assets::ICON_PLUGIN_CLAP_PATH,
            assets::ICON_PLUGIN_VST3_PATH,
            assets::TIMELINE_SCROLL_PATH,
        ];
        let mut list = Vec::new();
        for p in all_paths
            .into_iter()
            .chain(assets::AUDIO_REPAIR_ICONS.iter().map(|(p, _)| *p))
        {
            if p.starts_with(path) {
                list.push(SharedString::from(p));
            }
        }
        Ok(list)
    }
}
