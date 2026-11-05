pub struct SvgIcon {
    pub name: &'static str,
    pub svg: &'static str,
}

pub mod icons {
    pub const PALETTE: &str = include_str!("../../../packages/shared/lucide/icons/palette.svg");
    pub const MOVE_HORIZONTAL: &str =
        include_str!("../../../packages/shared/lucide/icons/move-horizontal.svg");
    pub const TYPE: &str = include_str!("../../../packages/shared/lucide/icons/type.svg");
    pub const LIST_MUSIC: &str =
        include_str!("../../../packages/shared/lucide/icons/list-music.svg");
    pub const GAUGE: &str = include_str!("../../../packages/shared/lucide/icons/gauge.svg");
    pub const REPEAT2: &str = include_str!("../../../packages/shared/lucide/icons/repeat-2.svg");
    pub const TIMER: &str = include_str!("../../../packages/shared/lucide/icons/timer.svg");
    pub const SAVE: &str = include_str!("../../../packages/shared/lucide/icons/save.svg");
    pub const FOLDER: &str = include_str!("../../../packages/shared/lucide/icons/folder.svg");
    pub const FOLDER_OPEN: &str =
        include_str!("../../../packages/shared/lucide/icons/folder-open.svg");
    pub const ELLIPSIS: &str = include_str!("../../../packages/shared/lucide/icons/ellipsis.svg");
    pub const SHARE: &str = include_str!("../../../packages/shared/lucide/icons/share.svg");
    pub const PANEL_BOTTOM: &str =
        include_str!("../../../packages/shared/lucide/icons/panel-bottom.svg");
    pub const PANEL_RIGHT: &str =
        include_str!("../../../packages/shared/lucide/icons/panel-right.svg");
    pub const PANEL_LEFT: &str =
        include_str!("../../../packages/shared/lucide/icons/panel-left.svg");
    pub const BUG: &str = include_str!("../../../packages/shared/lucide/icons/bug.svg");
    pub const MINUS: &str = include_str!("../../../packages/shared/lucide/icons/minus.svg");
    pub const MENU: &str = include_str!("../../../packages/shared/lucide/icons/menu.svg");
    pub const SEARCH: &str = include_str!("../../../packages/shared/lucide/icons/search.svg");
    pub const SCAN_SEARCH: &str =
        include_str!("../../../packages/shared/lucide/icons/scan-search.svg");
    pub const LOCK: &str = include_str!("../../../packages/shared/lucide/icons/lock.svg");
    pub const LOCK_OPEN: &str = include_str!("../../../packages/shared/lucide/icons/lock-open.svg");
    pub const EYE: &str = include_str!("../../../packages/shared/lucide/icons/eye.svg");
    pub const EYE_OFF: &str = include_str!("../../../packages/shared/lucide/icons/eye-off.svg");
    pub const DICES: &str = include_str!("../../../packages/shared/lucide/icons/dices.svg");
    pub const REFRESH_CW: &str =
        include_str!("../../../packages/shared/lucide/icons/refresh-cw.svg");
    pub const ARROW_LEFT_RIGHT: &str =
        include_str!("../../../packages/shared/lucide/icons/arrow-left-right.svg");
    pub const X: &str = include_str!("../../../packages/shared/lucide/icons/x.svg");
    pub const TRASH: &str = include_str!("../../../packages/shared/lucide/icons/trash-2.svg");
    pub const CHEVRON_LEFT: &str =
        include_str!("../../../packages/shared/lucide/icons/chevron-left.svg");

    // App Icons
    pub const TIMELINE_SCROLL: &str =
        include_str!("../../../packages/shared/icons/timelinescroll.svg");

    // Window controls
    pub const GENERIC_MAXIMIZE: &str =
        include_str!("../../../packages/shared/icons/generic_maximize.svg");
    pub const GENERIC_MINIMIZE: &str =
        include_str!("../../../packages/shared/icons/generic_minimize.svg");
    pub const GENERIC_RESTORE: &str =
        include_str!("../../../packages/shared/icons/generic_restore.svg");
    pub const GENERIC_CLOSE: &str =
        include_str!("../../../packages/shared/icons/generic_close.svg");

    // Additional icons
    pub const UNDO: &str = include_str!("../../../packages/shared/lucide/icons/undo-2.svg");
    pub const REDO: &str = include_str!("../../../packages/shared/lucide/icons/redo-2.svg");
    pub const HISTORY: &str = include_str!("../../../packages/shared/lucide/icons/history.svg");
    pub const LINK: &str = include_str!("../../../packages/shared/lucide/icons/link.svg");
    pub const CLOCK: &str = include_str!("../../../packages/shared/lucide/icons/clock.svg");
    pub const USER: &str = include_str!("../../../packages/shared/lucide/icons/user.svg");
    pub const LOG_OUT: &str = include_str!("../../../packages/shared/lucide/icons/log-out.svg");
    pub const SPARKLES: &str = include_str!("../../../packages/shared/lucide/icons/sparkles.svg");
    pub const PLUS: &str = include_str!("../../../packages/shared/lucide/icons/plus.svg");
    pub const CPU: &str = include_str!("../../../packages/shared/lucide/icons/cpu.svg");
    pub const MEMORY_STICK: &str =
        include_str!("../../../packages/shared/lucide/icons/memory-stick.svg");
    pub const HARD_DRIVE: &str =
        include_str!("../../../packages/shared/lucide/icons/hard-drive.svg");
    pub const GRIP_VERTICAL: &str =
        include_str!("../../../packages/shared/lucide/icons/grip-vertical.svg");
    pub const FILE: &str = include_str!("../../../packages/shared/lucide/icons/file.svg");
    pub const CHEVRON_RIGHT: &str =
        include_str!("../../../packages/shared/lucide/icons/chevron-right.svg");
    pub const CHEVRON_DOWN: &str = r#"<svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" xmlns="http://www.w3.org/2000/svg"><path d="m6 9 6 6 6-6"/></svg>"#;
    pub const CHECK: &str = include_str!("../../../packages/shared/lucide/icons/check.svg");
    pub const STAR: &str = include_str!("../../../packages/shared/lucide/icons/star.svg");
    pub const NEWSPAPER: &str = include_str!("../../../packages/shared/lucide/icons/newspaper.svg");
    pub const PEN_LINE: &str = include_str!("../../../packages/shared/lucide/icons/pen-line.svg");
    pub const SCAN: &str = include_str!("../../../packages/shared/lucide/icons/scan.svg");
    pub const CHEVRON_UP: &str =
        include_str!("../../../packages/shared/lucide/icons/chevron-up.svg");
    pub const POP_OUT: &str =
        include_str!("../../../packages/shared/lucide/icons/square-arrow-out-up-right.svg");

    // Tabler outline

    // Playhead downward-pointing triangle
    pub const PLAYHEAD_HANDLE: &str = r#"<svg width="12" height="12" viewBox="0 0 12 12" fill="none" xmlns="http://www.w3.org/2000/svg"><polygon points="0,0 12,0 6,12" fill="currentColor"/></svg>"#;

    // Plug-in format brand marks
    pub const PLUGIN_CLAP: &str = include_str!("../../../packages/shared/icons/plugins/clap.svg");
    pub const PLUGIN_VST3: &str = include_str!("../../../packages/shared/icons/plugins/vst3.svg");
}

// SVG virtual path constants
pub const ICON_PLAY_PATH: &str = "icons/play.svg";
pub const ICON_PAUSE_PATH: &str = "icons/pause.svg";
pub const ICON_SQUARE_PATH: &str = "icons/square.svg";
pub const ICON_CIRCLE_PATH: &str = "icons/circle.svg";
pub const ICON_SKIP_BACK_PATH: &str = "icons/skip-back.svg";
pub const ICON_REPEAT_PATH: &str = "icons/repeat.svg";
pub const ICON_REPEAT2_PATH: &str = "icons/repeat-2.svg";
pub const ICON_TIMER_PATH: &str = "icons/timer.svg";
pub const ICON_METRONOME_PATH: &str = "icons/metronome.svg";
pub const ICON_SAVE_PATH: &str = "icons/save.svg";
pub const ICON_FOLDER_PATH: &str = "icons/folder.svg";
pub const ICON_FOLDER_OPEN_PATH: &str = "icons/folder-open.svg";
pub const ICON_ELLIPSIS_PATH: &str = "icons/ellipsis.svg";
pub const ICON_SHARE_PATH: &str = "icons/share.svg";
pub const ICON_PANEL_BOTTOM_PATH: &str = "icons/panel-bottom.svg";
pub const ICON_PANEL_RIGHT_PATH: &str = "icons/panel-right.svg";
pub const ICON_PANEL_LEFT_PATH: &str = "icons/panel-left.svg";
pub const ICON_BUG_PATH: &str = "icons/bug.svg";
pub const ICON_MAXIMIZE_PATH: &str = "icons/generic_maximize.svg";
pub const ICON_MINIMIZE_PATH: &str = "icons/generic_minimize.svg";
pub const ICON_RESTORE_PATH: &str = "icons/generic_restore.svg";
pub const ICON_X_PATH: &str = "icons/generic_close.svg";
pub const ICON_MINUS_PATH: &str = "icons/minus.svg";
pub const ICON_MENU_PATH: &str = "icons/menu.svg";
pub const ICON_SEARCH_PATH: &str = "icons/search.svg";
pub const ICON_SCAN_SEARCH_PATH: &str = "icons/scan-search.svg";
pub const ICON_LOCK_PATH: &str = "icons/lock.svg";
pub const ICON_LOCK_OPEN_PATH: &str = "icons/lock-open.svg";
pub const ICON_EYE_PATH: &str = "icons/eye.svg";
pub const ICON_EYE_OFF_PATH: &str = "icons/eye-off.svg";
pub const ICON_SNOWFLAKE_PATH: &str = "icons/snowflake.svg";
pub const ICON_DICES_PATH: &str = "icons/dices.svg";
pub const ICON_REFRESH_CW_PATH: &str = "icons/refresh-cw.svg";
pub const ICON_ARROW_LEFT_RIGHT_PATH: &str = "icons/arrow-left-right.svg";

// New path constants
pub const ICON_MOUSE_POINTER_PATH: &str = "icons/mouse-pointer.svg";
pub const ICON_PENCIL_PATH: &str = "icons/pencil.svg";
pub const ICON_SCISSORS_PATH: &str = "icons/scissors.svg";
pub const ICON_UNDO_PATH: &str = "icons/undo-2.svg";
pub const ICON_REDO_PATH: &str = "icons/redo-2.svg";
pub const ICON_HISTORY_PATH: &str = "icons/history.svg";
pub const ICON_ERASER_PATH: &str = "icons/eraser.svg";
pub const ICON_PEN_LINE_PATH: &str = "icons/pen-line.svg";
pub const ICON_ZOOM_IN_PATH: &str = "icons/zoom-in.svg";
pub const ICON_ZOOM_OUT_PATH: &str = "icons/zoom-out.svg";
pub const ICON_SCAN_PATH: &str = "icons/scan.svg";
pub const ICON_CHEVRON_UP_PATH: &str = "icons/chevron-up.svg";
pub const ICON_POP_OUT_PATH: &str = "icons/square-arrow-out-up-right.svg";
pub const ICON_LINK_PATH: &str = "icons/link.svg";
pub const ICON_VOLUME_X_PATH: &str = "icons/volume-x.svg";
pub const ICON_CLOCK_PATH: &str = "icons/clock.svg";
pub const ICON_AUTOMATION_PATH: &str = "icons/automation.svg";
pub const ICON_USER_PATH: &str = "icons/user.svg";
pub const ICON_LOG_OUT_PATH: &str = "icons/log-out.svg";
pub const ICON_SLIDERS_HORIZONTAL_PATH: &str = "icons/sliders-horizontal.svg";
pub const ICON_SPARKLES_PATH: &str = "icons/sparkles.svg";
pub const ICON_PLUS_PATH: &str = "icons/plus.svg";
pub const ICON_PLUG_PATH: &str = "icons/plug.svg";
pub const ICON_ROUTE_PATH: &str = "icons/route.svg";
pub const ICON_MIC_PATH: &str = "icons/mic.svg";
pub const ICON_FILM_PATH: &str = "icons/film.svg";
pub const ICON_CPU_PATH: &str = "icons/cpu.svg";
pub const ICON_MEMORY_STICK_PATH: &str = "icons/memory-stick.svg";
pub const ICON_AUDIO_LINES_PATH: &str = "icons/audio-lines.svg";
pub const ICON_HARD_DRIVE_PATH: &str = "icons/hard-drive.svg";
pub const ICON_MUSIC_PATH: &str = "icons/music.svg";
pub const ICON_KEYBOARD_PATH: &str = "icons/keyboard-music.svg";
pub const ICON_LAYERS_PATH: &str = "icons/layers.svg";
pub const ICON_PALETTE_PATH: &str = "icons/palette.svg";
pub const ICON_MOVE_HORIZONTAL_PATH: &str = "icons/move-horizontal.svg";
pub const ICON_TYPE_PATH: &str = "icons/type.svg";
pub const ICON_LIST_MUSIC_PATH: &str = "icons/list-music.svg";
pub const ICON_GAUGE_PATH: &str = "icons/gauge.svg";
pub const ICON_GIT_MERGE_PATH: &str = "icons/git-merge.svg";
pub const ICON_GIT_FORK_PATH: &str = "icons/git-fork.svg";
pub const ICON_CORNER_DOWN_LEFT_PATH: &str = "icons/corner-down-left.svg";
pub const ICON_VOLUME_2_PATH: &str = "icons/volume-2.svg";
pub const ICON_CIRCLE_DOT_PATH: &str = "icons/circle-dot.svg";
pub const ICON_MAGNET_PATH: &str = "icons/magnet.svg";
pub const ICON_GRIP_VERTICAL_PATH: &str = "icons/grip-vertical.svg";
pub const ICON_FILE_PATH: &str = "icons/file.svg";
pub const ICON_CHEVRON_RIGHT_PATH: &str = "icons/chevron-right.svg";
pub const ICON_CHEVRON_DOWN_PATH: &str = "icons/chevron-down.svg";
pub const ICON_CHECK_PATH: &str = "icons/check.svg";
pub const ICON_STAR_PATH: &str = "icons/star.svg";
pub const ICON_NEWSPAPER_PATH: &str = "icons/newspaper.svg";
pub const ICON_PLAYHEAD_HANDLE_PATH: &str = "icons/playhead_handle.svg";
pub const ICON_PLUGIN_CLAP_PATH: &str = "icons/plugins/clap.svg";
pub const ICON_PLUGIN_VST3_PATH: &str = "icons/plugins/vst3.svg";
pub const TIMELINE_SCROLL_PATH: &str = "icons/timelinescroll.svg";

// Plug-in editor chrome. `ICON_X_PATH` is the window-control close, drawn to
// match the titlebar buttons; a tab's own close is the lighter lucide glyph.
pub const ICON_POWER_PATH: &str = "icons/power.svg";
pub const ICON_TRASH_PATH: &str = "icons/trash-2.svg";
pub const ICON_CHEVRON_LEFT_PATH: &str = "icons/chevron-left.svg";
pub const ICON_CLOSE_SMALL_PATH: &str = "icons/x.svg";

// Audio Repair module glyphs. One stroke family drawn for the module
// navigator, not borrowed from a general-purpose icon set.
pub const ICON_REPAIR_NOISE_REDUCTION_PATH: &str = "icons/audio_repair/noise_reduction.svg";
pub const ICON_REPAIR_DE_CLICK_PATH: &str = "icons/audio_repair/de_click.svg";
pub const ICON_REPAIR_DE_HUM_PATH: &str = "icons/audio_repair/de_hum.svg";
pub const ICON_REPAIR_SPECTRAL_PATH: &str = "icons/audio_repair/spectral_repair.svg";
pub const ICON_REPAIR_DE_REVERB_PATH: &str = "icons/audio_repair/de_reverb.svg";
pub const ICON_REPAIR_DE_BLEED_PATH: &str = "icons/audio_repair/de_bleed.svg";
pub const ICON_REPAIR_DE_FEEDBACK_PATH: &str = "icons/audio_repair/de_feedback.svg";
pub const ICON_REPAIR_DRUM_SILENCER_PATH: &str = "icons/audio_repair/drum_silencer.svg";

/// The Futureboard icon set: original 16 px drawings for the DAW's own
/// vocabulary — transport, edit tools, track and signal types, mixer and
/// routing — replacing the general-purpose Lucide / Tabler glyphs that stood
/// in for them. Keyed by the same virtual paths, so every call site picks
/// them up unchanged. See `packages/shared/icons/futureboard/README.md`.
pub const FUTUREBOARD_ICONS: [(&str, &str); 32] = [
    (
        ICON_PLAY_PATH,
        include_str!("../../../packages/shared/icons/futureboard/play.svg"),
    ),
    (
        ICON_PAUSE_PATH,
        include_str!("../../../packages/shared/icons/futureboard/pause.svg"),
    ),
    (
        ICON_SQUARE_PATH,
        include_str!("../../../packages/shared/icons/futureboard/stop.svg"),
    ),
    (
        ICON_CIRCLE_PATH,
        include_str!("../../../packages/shared/icons/futureboard/record.svg"),
    ),
    (
        ICON_SKIP_BACK_PATH,
        include_str!("../../../packages/shared/icons/futureboard/skip-back.svg"),
    ),
    (
        ICON_REPEAT_PATH,
        include_str!("../../../packages/shared/icons/futureboard/cycle.svg"),
    ),
    (
        ICON_METRONOME_PATH,
        include_str!("../../../packages/shared/icons/futureboard/metronome.svg"),
    ),
    (
        ICON_MOUSE_POINTER_PATH,
        include_str!("../../../packages/shared/icons/futureboard/pointer.svg"),
    ),
    (
        ICON_PENCIL_PATH,
        include_str!("../../../packages/shared/icons/futureboard/pencil.svg"),
    ),
    (
        ICON_SCISSORS_PATH,
        include_str!("../../../packages/shared/icons/futureboard/scissors.svg"),
    ),
    (
        ICON_ERASER_PATH,
        include_str!("../../../packages/shared/icons/futureboard/eraser.svg"),
    ),
    (
        ICON_ZOOM_IN_PATH,
        include_str!("../../../packages/shared/icons/futureboard/zoom-in.svg"),
    ),
    (
        ICON_ZOOM_OUT_PATH,
        include_str!("../../../packages/shared/icons/futureboard/zoom-out.svg"),
    ),
    (
        ICON_MAGNET_PATH,
        include_str!("../../../packages/shared/icons/futureboard/snap.svg"),
    ),
    (
        ICON_MIC_PATH,
        include_str!("../../../packages/shared/icons/futureboard/mic.svg"),
    ),
    (
        ICON_KEYBOARD_PATH,
        include_str!("../../../packages/shared/icons/futureboard/instrument.svg"),
    ),
    (
        ICON_MUSIC_PATH,
        include_str!("../../../packages/shared/icons/futureboard/midi-note.svg"),
    ),
    (
        ICON_AUDIO_LINES_PATH,
        include_str!("../../../packages/shared/icons/futureboard/waveform.svg"),
    ),
    (
        ICON_VOLUME_2_PATH,
        include_str!("../../../packages/shared/icons/futureboard/speaker.svg"),
    ),
    (
        ICON_VOLUME_X_PATH,
        include_str!("../../../packages/shared/icons/futureboard/mute.svg"),
    ),
    (
        ICON_GIT_MERGE_PATH,
        include_str!("../../../packages/shared/icons/futureboard/bus.svg"),
    ),
    (
        ICON_GIT_FORK_PATH,
        include_str!("../../../packages/shared/icons/futureboard/group.svg"),
    ),
    (
        ICON_CORNER_DOWN_LEFT_PATH,
        include_str!("../../../packages/shared/icons/futureboard/return.svg"),
    ),
    (
        ICON_FILM_PATH,
        include_str!("../../../packages/shared/icons/futureboard/video.svg"),
    ),
    (
        ICON_SLIDERS_HORIZONTAL_PATH,
        include_str!("../../../packages/shared/icons/futureboard/mixer.svg"),
    ),
    (
        ICON_AUTOMATION_PATH,
        include_str!("../../../packages/shared/icons/futureboard/automation.svg"),
    ),
    (
        ICON_PLUG_PATH,
        include_str!("../../../packages/shared/icons/futureboard/insert.svg"),
    ),
    (
        ICON_ROUTE_PATH,
        include_str!("../../../packages/shared/icons/futureboard/routing.svg"),
    ),
    (
        ICON_SNOWFLAKE_PATH,
        include_str!("../../../packages/shared/icons/futureboard/freeze.svg"),
    ),
    (
        ICON_POWER_PATH,
        include_str!("../../../packages/shared/icons/futureboard/power.svg"),
    ),
    (
        ICON_CIRCLE_DOT_PATH,
        include_str!("../../../packages/shared/icons/futureboard/mono.svg"),
    ),
    (
        ICON_LAYERS_PATH,
        include_str!("../../../packages/shared/icons/futureboard/stack.svg"),
    ),
];

/// Resolve a Futureboard icon by its virtual asset path.
pub fn futureboard_icon(path: &str) -> Option<&'static str> {
    FUTUREBOARD_ICONS
        .iter()
        .find(|(candidate, _)| *candidate == path)
        .map(|(_, svg)| *svg)
}

/// The Audio Repair glyph family, registered once.
///
/// Adding a repair module means adding an SVG file and one row here; the
/// embedded asset source resolves the whole table, so no per-icon `match` arm
/// or `list()` entry has to be kept in sync. The files carry no final UI color
/// — GPUI rasterizes them as a coverage mask and tints from the theme.
pub const AUDIO_REPAIR_ICONS: [(&str, &str); 8] = [
    (
        ICON_REPAIR_NOISE_REDUCTION_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/noise_reduction.svg"),
    ),
    (
        ICON_REPAIR_DE_CLICK_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/de_click.svg"),
    ),
    (
        ICON_REPAIR_DE_HUM_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/de_hum.svg"),
    ),
    (
        ICON_REPAIR_SPECTRAL_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/spectral_repair.svg"),
    ),
    (
        ICON_REPAIR_DE_REVERB_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/de_reverb.svg"),
    ),
    (
        ICON_REPAIR_DE_BLEED_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/de_bleed.svg"),
    ),
    (
        ICON_REPAIR_DE_FEEDBACK_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/de_feedback.svg"),
    ),
    (
        ICON_REPAIR_DRUM_SILENCER_PATH,
        include_str!("../../../packages/shared/icons/audio_repair/drum_silencer.svg"),
    ),
];

/// WhiteSharp's wordmark, for its editor's faceplate.
pub const WHITESHARP_WORDMARK_PATH: &str = "plugins/whitesharp/wordmark.svg";
pub const WHITESHARP_WORDMARK: &str = include_str!("../assets/whitesharp/wordmark.svg");

/// Resolve an Audio Repair glyph by its virtual asset path.
pub fn audio_repair_icon(path: &str) -> Option<&'static str> {
    AUDIO_REPAIR_ICONS
        .iter()
        .find(|(candidate, _)| *candidate == path)
        .map(|(_, svg)| *svg)
}

#[cfg(target_os = "windows")]
fn log_startup_dpi() {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    let dpi = unsafe { GetDpiForSystem() };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    eprintln!("[UI] dpi_scale={scale:.3}");
}

#[cfg(not(target_os = "windows"))]
fn log_startup_dpi() {}

/// Configures the native system-font policy and process-wide text scale.
///
/// The function name is retained for call-site compatibility. No font blobs
/// are registered: each platform resolves its own UI and Thai system families.
pub fn register_fonts(_cx: &mut gpui::App) {
    log_startup_dpi();
    gpui::set_global_text_scale(crate::theme::UI_TEXT_SCALE);
    eprintln!("[Fonts] source=system");
    eprintln!("[UI] default_font={}", crate::theme::FONT_FAMILY);
    eprintln!("[UI] text_scale={:.2}", crate::theme::UI_TEXT_SCALE);
    eprintln!(
        "[UI] default_font_size={}",
        crate::theme::typography::UI_SM as u32
    );
}

#[cfg(test)]
mod futureboard_icon_tests {
    use super::*;
    use gpui::AssetSource;

    /// Every Futureboard icon is what the asset source hands out for its path,
    /// so no call site can still be drawing the glyph it replaced.
    #[test]
    fn every_futureboard_icon_is_served_for_its_path() {
        let source = crate::embedded_assets::EmbeddedAssets::new();
        for (path, svg) in FUTUREBOARD_ICONS {
            let served = source
                .load(path)
                .expect("load")
                .unwrap_or_else(|| panic!("{path} is not served"));
            assert_eq!(
                served.as_ref(),
                svg.as_bytes(),
                "{path} serves another glyph"
            );
        }
    }

    /// The grid and colour rules from the set's README: a 16 px canvas, and
    /// every paint `currentColor` so the theme tints it. The only literal
    /// colours allowed are the mask's black and white.
    #[test]
    fn futureboard_icons_follow_the_grid_and_tint_from_the_theme() {
        for (path, svg) in FUTUREBOARD_ICONS {
            assert!(
                svg.contains(r#"viewBox="0 0 16 16""#),
                "{path}: not on the 16 px grid"
            );
            assert!(
                svg.contains("currentColor"),
                "{path}: paints nothing tintable"
            );
            let stripped = svg.replace("#fff", "").replace("#000", "");
            assert!(
                !stripped.contains('#') || stripped.contains("url(#"),
                "{path}: literal colour"
            );
            for literal in stripped.split("url(#").skip(1) {
                assert!(literal.starts_with('k'), "{path}: unexpected reference");
            }
        }
        let mut paths: Vec<&str> = FUTUREBOARD_ICONS.iter().map(|(path, _)| *path).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(
            paths.len(),
            FUTUREBOARD_ICONS.len(),
            "two icons claim one path"
        );
    }
}
