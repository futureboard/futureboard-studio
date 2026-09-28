use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use gpui::{Font, FontWeight, Rgba};
use serde::Deserialize;
use serde_json::Value;

/// Native UI font selected independently on each supported desktop platform.
#[cfg(target_os = "macos")]
pub const SYSTEM_UI_FONT_FAMILY: &str = ".AppleSystemUIFont";
#[cfg(target_os = "windows")]
pub const SYSTEM_UI_FONT_FAMILY: &str = "Segoe UI";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const SYSTEM_UI_FONT_FAMILY: &str = "Noto Sans";

/// Compatibility alias used by existing UI call sites. It intentionally points
/// to the platform font rather than a bundled typeface.
pub const FONT_FAMILY: &str = SYSTEM_UI_FONT_FAMILY;

/// Primary Thai system family, retained only for the plugin-editor shell chrome
/// renderer ([`crate::components::plugin_shell_text`]), whose `sphere_graphic_engine`
/// `FontConfig` supports a single fallback family rather than a composite chain.
///
/// General UI text does **not** use this: it goes through the composite
/// [`crate::fonts`] system, which never selects a font by language.
#[cfg(target_os = "macos")]
pub const SYSTEM_THAI_UI_FONT_FAMILY: &str = "Thonburi";
#[cfg(target_os = "windows")]
pub const SYSTEM_THAI_UI_FONT_FAMILY: &str = "Leelawadee UI";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const SYSTEM_THAI_UI_FONT_FAMILY: &str = "Noto Sans Thai";

/// Readability adjustment for the native application. This scales text only;
/// controls and window geometry retain their compact DAW layout.
pub const UI_TEXT_SCALE: f32 = 1.08;

/// Bundled cross-surface theme template. Installed to AppData on first run and
/// used as the fallback when no user theme exists or a user theme omits tokens.
pub const DEFAULT_THEME_JSON: &str = include_str!("../../../packages/shared/themes/Default.json");
pub const LIGHT_THEME_JSON: &str = include_str!("../../../packages/shared/themes/Light.json");
pub const TEMPLATE_THEME_JSON: &str = include_str!("../../../packages/shared/themes/Template.json");

/// Composite system-UI font at the default (regular) weight.
///
/// One descriptor for all normal UI text. The platform's native text system
/// (DirectWrite / CoreText / Fontconfig) shapes and per-cluster fallback-resolves
/// it against the coverage-ordered chain attached by [`crate::fonts`]. No
/// language detection, no per-character font switching.
pub fn ui_font() -> Font {
    crate::fonts::ui_font(FontWeight::NORMAL)
}

/// Composite system-UI font at an explicit weight (regular, medium, semibold, …).
pub fn ui_font_weight(weight: FontWeight) -> Font {
    crate::fonts::ui_font(weight)
}

/// Composite display font (large text) at the given weight.
pub fn display_font(weight: FontWeight) -> Font {
    crate::fonts::display_font(weight)
}

/// Compact DAW typography tokens (logical px — GPUI/DWrite scale for DPI).
pub mod typography {
    /// Small metadata labels (dB scale, channel index).
    pub const UI_XS: f32 = 11.0;
    /// Default UI body / toolbar / track header label.
    pub const UI_SM: f32 = 12.0;
    /// Section headers, dialog titles, emphasized labels.
    pub const UI_MD: f32 = 13.0;
    /// Semibold section / panel titles.
    pub const UI_TITLE: f32 = 13.0;
    /// Dense captions used only where 11 px cannot fit without increasing a
    /// compact control's height (meter legends, narrow strip metadata).
    pub const DENSE_CAPTION: f32 = 9.5;
    /// Dense interactive labels in fixed-height mixer/inspector controls.
    pub const DENSE_LABEL: f32 = 10.5;
    /// Native plugin editor wrapper titlebar (Pro-C 3, etc.).
    pub const PLUGIN_TITLE: f32 = 12.0;
    /// Default line-height ratio for single-line chrome text.
    pub const LINE_HEIGHT: f32 = 1.3;
}

/// Recommended text sizes. Kept here so individual components don't drift.
pub mod text {
    use super::typography::*;

    /// Caps-style sublabels — INSERTS / SENDS / TRACK.
    pub const CAPS: f32 = UI_XS;
    /// Small meta (CH 01, dB scale).
    pub const META: f32 = UI_XS;
    /// Standard UI label (track name, button label).
    pub const UI: f32 = UI_SM;
    /// Inspector / title text.
    pub const TITLE: f32 = UI_MD;
}

/// Corner radius scale (logical px).
///
/// Six names, nothing between them. Before this module the crate carried ~360
/// ad-hoc radius calls spread over nine different values (`rounded_sm` 4,
/// `rounded_md` 6, `rounded_lg` 8, plus hand-written 1/2/3/4/5/6/10/18/999 px),
/// which is why controls of the same height in the same toolbar did not read as
/// one system. Pick a token by *what the thing is*, never by how big it looks.
pub mod radius {
    /// Square. Grid cells, full-bleed lanes, the ruler, meter fills, table rows,
    /// the chrome row, the status bar, the inner edges of a segmented control,
    /// and every quad whose short side is under [`MIN_SIDE`].
    pub const NONE: f32 = 0.0;
    /// Borderless identity chips, clip bodies 10–15 px tall, MIDI notes,
    /// fader/slider thumbs, caret and selection highlights. Never pair with a
    /// 1 px stroke: this low, a border visibly varies in weight around the arc.
    pub const MICRO: f32 = 3.0;
    /// Controls in the 16–20 px band (mixer M/S/R/I, insert chips, dense
    /// toolbar buttons). [`CONTROL`] on a 16 px control would eat nearly 40% of
    /// its height and read as a lozenge.
    pub const CONTROL_SM: f32 = 4.0;
    /// The workhorse: interactive controls 24–32 px tall. Buttons, inputs,
    /// select triggers, transport, dock tabs, clips.
    ///
    /// The scale follows the ratio real systems use — radius ≈ 0.2–0.25 × the
    /// control's height — rather than one flat value across the whole band,
    /// which is why there are two control tiers instead of one.
    pub const CONTROL: f32 = 6.0;
    /// Containing surfaces that sit visibly above another plane: menus,
    /// popovers, tooltips, cards, inspector sections, mixer strip frames.
    pub const SURFACE: f32 = 10.0;
    /// Window-level surfaces only — modals and client-decorated utility windows,
    /// matched to the host window radius so a modal never reads sharper than the
    /// frame behind it.
    pub const DIALOG: f32 = 14.0;
    /// Fully round. NON-INTERACTIVE identity objects ≤ 20 px (status chips,
    /// badges, avatars, scrollbar thumbs, knob bodies, rail caps) and toggle
    /// switch tracks. Never on transport, mute/solo/arm, clips, notes or rows.
    pub const PILL: f32 = 9999.0;

    /// Below this short side a radius exceeds a quarter of the shape and the
    /// corners merge into a lozenge.
    pub const MIN_SIDE: f32 = 10.0;

    /// Radius for an element nested inside a rounded container.
    ///
    /// `inner = outer - padding`, floored at 0. Concentric corners only look
    /// right when the gap between the two arcs is constant; using the parent's
    /// radius on a child makes the child's corner look too tight.
    pub const fn inner(outer: f32, pad: f32) -> f32 {
        let r = outer - pad;
        if r > 0.0 {
            r
        } else {
            0.0
        }
    }

    /// Radius for a **content** quad whose size is data-driven and unbounded —
    /// clips at any zoom, notes at any row height, meter segments.
    ///
    /// Fixed-geometry chrome (rails, thumbs, switch tracks) does *not* go
    /// through this; it asks for [`PILL`] or [`MICRO`] directly, because its
    /// dimensions are chosen by the design rather than by the data.
    ///
    /// `Window::paint_quad` does not clamp corner radii the way the `div` path
    /// does, so batched painters must call this explicitly.
    pub fn clamped(r: f32, width: f32, height: f32) -> f32 {
        let short = if width < height { width } else { height };
        if short < MIN_SIDE {
            return NONE;
        }
        let ceiling = short * 0.25;
        if r > ceiling {
            ceiling
        } else {
            r
        }
    }
}

/// Spacing scale (logical px). Nine steps, 0–32; nothing above 24 appears in
/// Studio working chrome.
pub mod space {
    pub const NONE: f32 = 0.0;
    /// Tightest legal gap — adjacent micro toggles, transport icons, menu items.
    pub const HAIR: f32 = 2.0;
    /// Menu/popover outer padding, segmented-control inset, row backplate inset.
    /// Chosen so `radius::inner(SURFACE, TIGHT)` lands exactly on `CONTROL`.
    pub const TIGHT: f32 = 4.0;
    /// Icon-to-label gap, stacked dense rows, padding of a 20 px control.
    pub const SNUG: f32 = 6.0;
    /// The default: panel and card padding, menu row padding, button padding at
    /// 20–24 px, gap between toolbar control groups.
    pub const BASE: f32 = 8.0;
    /// Dialog body padding, section gaps inside a panel, 32 px button padding.
    pub const LOOSE: f32 = 12.0;
    /// Gap between major sections, dialog footer padding.
    pub const SECTION: f32 = 16.0;
    /// Gap between unrelated blocks on a settings or welcome surface.
    pub const BLOCK: f32 = 24.0;
    /// Outer margin of a full-window empty state. The ceiling of the scale.
    pub const PAGE: f32 = 32.0;
}

/// Control height ladder (logical px).
///
/// Visual heights are deliberately *smaller* than the old ones while hit targets
/// get *larger*: the growth goes into transparent padding via [`hit_target`], so
/// the app reads tighter and clicks easier at the same time.
pub mod size {
    /// Inline latching toggles inside a mixer strip (M/S/R/I), track-header
    /// pills. Visual only — inflate the hit box.
    pub const MICRO: f32 = 16.0;
    /// Insert/send chips, automation lane buttons, timeline tool buttons,
    /// status-bar chips, ruler buttons.
    pub const DENSE: f32 = 20.0;
    /// The dense-chrome default: toolbar and transport buttons, panel toggles,
    /// dock tabs, list/tree/menu rows, inspector control rows.
    pub const DEFAULT: f32 = 24.0;
    /// Text inputs, select/combo triggers, key-recorder and color-picker
    /// triggers, segmented-control outer, secondary dialog buttons.
    pub const COMFORTABLE: f32 = 28.0;
    /// Primary dialog actions, command-palette search, welcome CTAs, the app
    /// chrome row.
    pub const PROMINENT: f32 = 32.0;
    /// Browser tree rows. Must stay exact — `gpui::uniform_list` virtualization
    /// derives its window from this.
    pub const ROW_DENSE: f32 = 22.0;
    /// Menu items, picker rows, settings rows, command-palette rows.
    pub const ROW: f32 = 24.0;
    /// Minimum arrangement track row. Below this a clip drops to `radius::MICRO`
    /// and the header hides its control row.
    pub const TRACK_ROW_MIN: f32 = 40.0;
    /// Default arrangement track row.
    pub const TRACK_ROW: f32 = 64.0;
    /// At or above this row height the track header shows fader/pan/meter.
    /// Kept strictly below [`TRACK_ROW`] so the default row *does* show them.
    pub const TRACK_CONTROLS_MIN: f32 = 56.0;
    /// Mixer channel strip width.
    pub const STRIP_WIDTH: f32 = 80.0;

    /// Minimum comfortable hit target. A 16 px or 20 px control keeps its
    /// visual size and grows its clickable area with transparent padding.
    pub const HIT_MIN: f32 = 24.0;

    /// Transparent padding (per side) needed to lift `visual` to a comfortable
    /// hit target. Returns 0 when the control is already large enough.
    pub fn hit_target(visual: f32) -> f32 {
        let pad = (HIT_MIN - visual) * 0.5;
        if pad > 0.0 {
            pad
        } else {
            0.0
        }
    }
}

/// State-layer alphas.
///
/// Material 3's shipped opacities, reduced ~25% because M3 is tuned for 40 px
/// touch targets and Studio runs 20–24 px controls. Composite these over the
/// element's *rest* fill with [`Colors::composite`] — GPUI gives a div exactly
/// one background, so `.hover(|s| s.bg(..))` replaces the fill rather than
/// layering over it.
pub mod state {
    pub const HOVER: f32 = 0.06;
    pub const PRESSED: f32 = 0.10;
    pub const SELECTED: f32 = 0.10;
    pub const SELECTED_HOVER: f32 = 0.14;
    pub const DRAGGED: f32 = 0.16;
    /// Wash behind a latched DAW toggle (mute/solo/arm/monitor/automation).
    pub const ARMED_WASH: f32 = 0.18;
    /// Border alpha of that same latched toggle — state is always carried on
    /// two channels, never hue alone.
    pub const ARMED_BORDER: f32 = 0.55;
    pub const DISABLED_CONTENT: f32 = 0.38;
    /// Keyboard focus ring spread, in px.
    pub const FOCUS_RING_PX: f32 = 2.0;
}

/// Motion durations (ms) and the one easing worth having.
///
/// A pro tool confirms cause and effect; it does not animate idle space.
pub mod motion {
    /// State-layer crossfades on hover/press. Below ~90 ms a transition reads as
    /// instant, above ~140 ms a dense toolbar starts to feel syrupy.
    pub const MICRO_MS: u64 = 110;
    /// Popover and menu entry.
    pub const FAST_MS: u64 = 160;
    /// Panel expand/collapse, dialog entry.
    pub const SLOW_MS: u64 = 240;
}

/// Elevation levels.
///
/// Depth is carried by **value** first, a **hairline** second, and **shadow**
/// only for genuinely floating layers. On a 23%-lightness panel a black shadow
/// has almost no dynamic range left, which is why the 25 hand-rolled
/// `BoxShadow` literals this replaces read as smudges rather than lift.
pub mod elevation {
    use gpui::{point, px, BoxShadow};

    #[derive(Debug, Clone, Copy)]
    pub struct ShadowSpec {
        pub offset_y: f32,
        pub blur: f32,
        pub spread: f32,
        pub alpha: f32,
    }

    /// Menus, context menus, dropdowns, select/combo popovers, tooltips,
    /// the color-picker popover, the background-task panel.
    ///
    /// One value replacing the six different (offset, blur) pairs in the crate.
    pub const OVERLAY: ShadowSpec = ShadowSpec {
        offset_y: 4.0,
        blur: 12.0,
        spread: 0.0,
        alpha: 0.45,
    };

    /// Clip drag ghosts, track drag previews, slot and browser drags.
    pub const DRAG: ShadowSpec = ShadowSpec {
        offset_y: 8.0,
        blur: 20.0,
        spread: 0.0,
        alpha: 0.50,
    };

    /// Build the GPUI shadow list for a level, so no component writes a
    /// `BoxShadow` literal again.
    pub fn shadow(spec: ShadowSpec) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: gpui::hsla(0.0, 0.0, 0.0, spec.alpha),
            offset: point(px(0.0), px(spec.offset_y)),
            blur_radius: px(spec.blur),
            spread_radius: px(spec.spread),
            inset: false,
        }]
    }

    /// Keyboard focus ring.
    ///
    /// A ring, not a border recolor: swapping a 1 px border color is
    /// indistinguishable from hover. Drawn as a zero-blur spread shadow so it
    /// sits outside the control without affecting layout — `.focus_visible`
    /// cannot change an element's radius, so the ring simply follows it.
    pub fn focus_ring(color: gpui::Rgba) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: gpui::Hsla::from(color),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(0.0),
            spread_radius: px(super::state::FOCUS_RING_PX),
            inset: false,
        }]
    }
}

pub mod menu {
    pub const PANEL_MIN_WIDTH: f32 = 210.0;
    pub const PANEL_MAX_WIDTH: f32 = 340.0;
    pub const PANEL_PAD: f32 = 3.0;
    pub const ROW_HEIGHT: f32 = 20.0;
    pub const ROW_PAD_X: f32 = 8.0;
    pub const CHECK_SLOT_W: f32 = 18.0;
    pub const ICON_SIZE: f32 = 11.0;
    pub const CHEVRON_SIZE: f32 = 11.0;
    pub const LABEL_TEXT_SIZE: f32 = crate::theme::typography::UI_XS;
    pub const META_TEXT_SIZE: f32 = crate::theme::typography::UI_XS;
    pub const HEADER_TEXT_SIZE: f32 = crate::theme::typography::UI_XS;
    pub const HEADER_HEIGHT: f32 = 21.0;
    pub const SEPARATOR_MARGIN_Y: f32 = 2.0;
    pub const ITEM_GAP: f32 = 1.0;

    /// Approximate rendered width (px) of a menu label at `LABEL_TEXT_SIZE`.
    ///
    /// GPUI does not auto-size these absolutely-positioned popovers from their
    /// text contents, so panel width is estimated up front. A flat
    /// `chars * 6.1` figure is calibrated for Latin/Inter and badly
    /// underestimates scripts whose glyphs are wider (CJK) or whose character
    /// count is inflated by zero-advance combining marks (Thai/Lao), which made
    /// localized labels truncate with an ellipsis. Estimate per character by
    /// script instead so the panel reserves the room the text actually needs.
    pub fn estimate_label_width(text: &str) -> f32 {
        text.chars().map(char_advance).sum()
    }

    fn char_advance(ch: char) -> f32 {
        let c = ch as u32;
        // Thai and Lao above/below vowels and tone marks stack on the base
        // glyph with no horizontal advance.
        if is_south_east_asian_combining_mark(c) {
            return 0.0;
        }
        match c {
            // Hangul Jamo, CJK (radicals through unified ideographs), Hangul
            // syllables, CJK compatibility, and fullwidth forms render at
            // roughly the em width.
            0x1100..=0x11FF
            | 0x2E80..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6 => LABEL_TEXT_SIZE,
            // Thai and Lao base consonants/vowels are visibly wider than Latin.
            0x0E00..=0x0EFF => LABEL_TEXT_SIZE * 0.72,
            // Latin and everything else: preserve the tuned Inter figure.
            _ => 6.1,
        }
    }

    fn is_south_east_asian_combining_mark(c: u32) -> bool {
        matches!(c,
            // Thai
            0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E
            // Lao
            | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECD)
    }
}

#[derive(Debug, Clone)]
pub struct LoadedTheme {
    pub id: String,
    pub name: String,
    pub path: Option<PathBuf>,
    colors: HashMap<String, Rgba>,
    track_colors: Vec<Rgba>,
}

#[derive(Debug, Clone)]
pub struct ThemeLoadReport {
    pub active_id: String,
    pub active_name: String,
    pub active_path: Option<PathBuf>,
    pub discovered: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeManifest {
    id: Option<String>,
    name: Option<String>,
    tokens: Option<Value>,
    track_colors: Option<Vec<String>>,
}

static ACTIVE_THEME: OnceLock<RwLock<LoadedTheme>> = OnceLock::new();
static LAST_THEME_REPORT: OnceLock<RwLock<Option<ThemeLoadReport>>> = OnceLock::new();

fn active_theme_store() -> &'static RwLock<LoadedTheme> {
    ACTIVE_THEME.get_or_init(|| RwLock::new(load_default_theme()))
}

fn report_store() -> &'static RwLock<Option<ThemeLoadReport>> {
    LAST_THEME_REPORT.get_or_init(|| RwLock::new(None))
}

pub fn active_theme_summary() -> (String, String, Option<PathBuf>) {
    let theme = active_theme_store()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    (theme.id.clone(), theme.name.clone(), theme.path.clone())
}

/// Themes available to the UI, including the bundled fallback. This is a
/// control-path query used by Preferences and the command palette, never by a
/// render hot path.
pub fn available_theme_summaries() -> Vec<(String, String)> {
    let paths = crate::paths::FutureboardPaths::resolve();
    let _ = fs::create_dir_all(&paths.themes);
    install_builtin_theme_templates(&paths.themes);

    let default = load_default_theme();
    let mut themes = vec![(default.id.clone(), default.name.clone())];
    let mut discovered = discover_theme_files(&paths.themes);
    discovered.sort();
    for path in discovered {
        if let Ok(theme) = load_theme_file(&path, &default) {
            if theme.id != "publisher.theme-id" && !themes.iter().any(|(id, _)| id == &theme.id) {
                themes.push((theme.id, theme.name));
            }
        }
    }
    themes
}

/// Activate one installed theme by its stable id. The active store is shared
/// by all token lookups, so the next GPUI render picks up the new colors.
pub fn activate_theme_by_id(id: &str) -> bool {
    let paths = crate::paths::FutureboardPaths::resolve();
    let _ = fs::create_dir_all(&paths.themes);
    install_builtin_theme_templates(&paths.themes);
    let default = load_default_theme();
    // Pre-selector builds persisted display labels rather than ids. Keep those
    // settings files valid by resolving the old dark presets to the bundled
    // fallback instead of leaving the process on an unrelated auto-selected
    // theme.
    let id = match id {
        "Fleet Dark" | "Ableton Dark" => default.id.as_str(),
        other => other,
    };
    let selected = if id == default.id {
        Some(default.clone())
    } else {
        let mut files = discover_theme_files(&paths.themes);
        files.sort();
        files.into_iter().find_map(|path| {
            load_theme_file(&path, &default)
                .ok()
                .filter(|theme| theme.id == id)
        })
    };
    let Some(theme) = selected else {
        return false;
    };
    *active_theme_store()
        .write()
        .unwrap_or_else(|e| e.into_inner()) = theme;
    true
}

pub fn last_theme_load_report() -> Option<ThemeLoadReport> {
    report_store()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Initialize the Native GPUI theme system from:
/// `%APPDATA%/Futureboard Studio/Extensions/Themes/**/theme.json` on Windows
/// and the equivalent config path on other platforms.
///
/// Selection order:
/// 1. `FUTUREBOARD_THEME_ID=<theme id>` when set.
/// 2. First discovered non-default, non-template user theme, sorted by path.
/// 3. Bundled `futureboard.default`.
pub fn initialize_theme_system() -> ThemeLoadReport {
    let paths = crate::paths::FutureboardPaths::resolve();
    let _ = fs::create_dir_all(&paths.themes);
    install_builtin_theme_templates(&paths.themes);

    let default = load_default_theme();
    let mut discovered = discover_theme_files(&paths.themes);
    discovered.sort();

    let requested_id = std::env::var("FUTUREBOARD_THEME_ID").ok();
    let mut loaded = Vec::new();
    let mut errors = Vec::new();

    for path in &discovered {
        match load_theme_file(path, &default) {
            Ok(theme) => loaded.push(theme),
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }

    let chosen = requested_id
        .as_deref()
        .and_then(|id| loaded.iter().find(|theme| theme.id == id).cloned())
        .or_else(|| {
            loaded
                .iter()
                .find(|theme| theme.id != default.id && theme.id != "publisher.theme-id")
                .cloned()
        })
        .unwrap_or(default);

    let report = ThemeLoadReport {
        active_id: chosen.id.clone(),
        active_name: chosen.name.clone(),
        active_path: chosen.path.clone(),
        discovered: loaded.len(),
        errors,
    };

    *active_theme_store()
        .write()
        .unwrap_or_else(|e| e.into_inner()) = chosen;
    *report_store().write().unwrap_or_else(|e| e.into_inner()) = Some(report.clone());

    eprintln!(
        "[theme] active={} name={} discovered={} path={}",
        report.active_id,
        report.active_name,
        report.discovered,
        report
            .active_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<bundled>".to_string())
    );
    for error in &report.errors {
        eprintln!("[theme] failed to load {error}");
    }

    report
}

fn install_builtin_theme_templates(themes_dir: &Path) {
    let default_path = themes_dir.join("Default").join("theme.json");
    if !default_path.exists() {
        if let Some(parent) = default_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&default_path, DEFAULT_THEME_JSON);
    }

    let light_path = themes_dir.join("Light").join("theme.json");
    if !light_path.exists() {
        if let Some(parent) = light_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&light_path, LIGHT_THEME_JSON);
    }

    // The authoring template sits directly in the themes root (not inside its
    // own folder) so `discover_theme_files` below picks it up like any other
    // flat custom theme; it never auto-activates because its id
    // ("publisher.theme-id") is filtered out at the call sites that list and
    // select themes.
    let template_path = themes_dir.join("Template.json");
    if !template_path.exists() {
        let _ = fs::write(&template_path, TEMPLATE_THEME_JSON);
    }
}

/// Finds every theme manifest under the themes root, recursively.
///
/// Supports both the packaged layout (`<Name>/theme.json`) and a custom theme
/// dropped by a user as a flat file directly in the themes root
/// (`{themes_dir}/*.json`, e.g. `MyTheme.json`) — any `.json` file counts as a
/// candidate manifest and is parsed by [`load_theme_file`], which already
/// discards anything that fails to parse or resolve a usable `id`/`tokens`.
fn discover_theme_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("json"))
                .unwrap_or(false)
            {
                files.push(path);
            }
        }
    }

    files
}

fn load_default_theme() -> LoadedTheme {
    load_theme_from_str(DEFAULT_THEME_JSON, None, None).unwrap_or_else(|error| {
        eprintln!("[theme] bundled Default.json is invalid: {error}");
        LoadedTheme {
            id: "futureboard.default".to_string(),
            name: "Futureboard Default".to_string(),
            path: None,
            colors: HashMap::new(),
            track_colors: DEFAULT_TRACK_COLOR_VALUES
                .iter()
                .map(|c| rgba_from_u32(*c))
                .collect(),
        }
    })
}

fn load_theme_file(path: &Path, base: &LoadedTheme) -> Result<LoadedTheme, String> {
    let json = fs::read_to_string(path).map_err(|e| e.to_string())?;
    load_theme_from_str(&json, Some(path.to_path_buf()), Some(base))
}

fn load_theme_from_str(
    json: &str,
    path: Option<PathBuf>,
    base: Option<&LoadedTheme>,
) -> Result<LoadedTheme, String> {
    let manifest: ThemeManifest = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let mut colors = base.map(|theme| theme.colors.clone()).unwrap_or_default();

    if let Some(tokens) = manifest.tokens.as_ref() {
        flatten_theme_tokens(tokens, "", &mut colors)?;
    }

    let track_colors = manifest
        .track_colors
        .as_ref()
        .and_then(|colors| {
            let parsed: Vec<Rgba> = colors
                .iter()
                .filter_map(|color| parse_theme_color(color).ok())
                .collect();
            if parsed.is_empty() {
                None
            } else {
                Some(parsed)
            }
        })
        .or_else(|| base.map(|theme| theme.track_colors.clone()))
        .unwrap_or_else(|| {
            DEFAULT_TRACK_COLOR_VALUES
                .iter()
                .map(|c| rgba_from_u32(*c))
                .collect()
        });

    Ok(LoadedTheme {
        id: manifest
            .id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| "futureboard.unnamed".to_string()),
        name: manifest
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Unnamed Theme".to_string()),
        path,
        colors,
        track_colors,
    })
}

fn flatten_theme_tokens(
    value: &Value,
    prefix: &str,
    out: &mut HashMap<String, Rgba>,
) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let next = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_theme_tokens(value, &next, out)?;
            }
        }
        Value::String(color) => {
            let parsed = parse_theme_color(color)
                .map_err(|e| format!("invalid color token {prefix}={color:?}: {e}"))?;
            out.insert(prefix.to_string(), parsed);
        }
        _ => {}
    }
    Ok(())
}

fn parse_theme_color(input: &str) -> Result<Rgba, crate::color::ColorParseError> {
    crate::color::parse_hex_color(input)
}

fn rgba_from_u32(value: u32) -> Rgba {
    Rgba {
        r: ((value >> 16) & 0xFF) as f32 / 255.0,
        g: ((value >> 8) & 0xFF) as f32 / 255.0,
        b: (value & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

macro_rules! theme_color {
    ($name:ident, $key:literal, $fallback:literal) => {
        pub fn $name() -> Rgba {
            Self::resolve($key, $fallback)
        }
    };
}

pub struct Colors;

const DEFAULT_TRACK_COLOR_VALUES: [u32; 12] = [
    0x4FC9D8, 0x4FD39A, 0x8FD165, 0xE3C15E, 0xEC9A5C, 0xF0776F, 0xE87BAF, 0xA98BF5, 0x7FA8FF,
    0x5FBEE8, 0xC0A177, 0x4FD2BC,
];

impl Colors {
    fn resolve(key: &str, fallback: &str) -> Rgba {
        let theme = active_theme_store()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        theme
            .colors
            .get(key)
            .copied()
            .unwrap_or_else(|| parse_theme_color(fallback).unwrap_or_else(|_| Rgba::default()))
    }

    // Backgrounds
    theme_color!(surface_base, "surface.base", "#1A1D24");
    theme_color!(surface_panel, "surface.panel", "#22252E");
    theme_color!(surface_panel_alt, "surface.panelAlt", "#14161C");
    theme_color!(surface_panel_raised, "surface.panelRaised", "#2A2E38");
    theme_color!(surface_canvas, "surface.canvas", "#0E1015");
    theme_color!(surface_raised, "surface.raised", "#2A2E38");
    theme_color!(surface_input, "surface.input", "#0E1015");
    theme_color!(surface_window, "surface.window", "#16181F");
    theme_color!(surface_titlebar, "surface.titlebar", "#14161C");
    theme_color!(surface_sidebar, "surface.sidebar", "#14161C");
    theme_color!(surface_card, "surface.card", "#22252E");
    theme_color!(surface_card_hover, "surface.cardHover", "#383D48");
    theme_color!(surface_card_selected, "surface.cardSelected", "#4A4F5C");
    theme_color!(surface_code, "surface.code", "#16181F");
    theme_color!(surface_badge, "surface.badge", "#2A2E38");
    theme_color!(surface_hover, "surface.hover", "#383D48");
    theme_color!(surface_active, "surface.active", "#4A4F5C");
    theme_color!(surface_control_hover, "surface.controlHover", "#31353F");
    theme_color!(surface_overlay, "surface.overlay", "#05070BA6");

    // Borders
    theme_color!(border_subtle, "border.subtle", "#FFFFFF14");
    theme_color!(border_normal, "border.normal", "#FFFFFF1F");
    theme_color!(border_default, "border.default", "#FFFFFF1F");
    theme_color!(border_strong, "border.strong", "#FFFFFF33");
    theme_color!(border_focus, "border.focus", "#2FC9D6E0");
    theme_color!(border_accent, "border.accent", "#2FC9D68C");
    theme_color!(divider, "border.divider", "#FFFFFF0F");

    // Text
    theme_color!(text_primary, "text.primary", "#EBEDF2");
    theme_color!(text_secondary, "text.secondary", "#B9BDC6");
    theme_color!(text_muted, "text.muted", "#8F949F");
    theme_color!(text_faint, "text.faint", "#6F7480");
    theme_color!(text_dim, "text.dim", "#828792");
    theme_color!(text_disabled, "text.disabled", "#5B606B");
    theme_color!(text_inverse, "text.inverse", "#0E1015");

    // Accent
    theme_color!(accent_primary, "accent.primary", "#2FC9D6");
    theme_color!(accent_primary_hover, "accent.primaryHover", "#54D8E2");
    theme_color!(accent_hover, "accent.hover", "#54D8E2");
    theme_color!(accent_active, "accent.active", "#2FC9D62E");
    theme_color!(accent_focus, "accent.focus", "#2FC9D6E0");
    theme_color!(accent_soft, "accent.soft", "#2FC9D629");
    theme_color!(accent_muted, "accent.muted", "#2FC9D61A");
    theme_color!(accent_pressed, "accent.pressed", "#1AA8B5");
    theme_color!(on_accent, "accent.onAccent", "#06181B");

    // Status / Alert Accents
    theme_color!(status_error, "status.error", "#F2645F");
    theme_color!(status_warning, "status.warning", "#E8B75C");
    theme_color!(semantic_warning, "semantic.warning", "#E8B75C");
    theme_color!(status_success, "status.success", "#43D18A");
    theme_color!(accent_success, "accent.success", "#43D18A");
    theme_color!(accent_warning, "accent.warning", "#E8B75C");
    theme_color!(accent_danger, "accent.danger", "#F2645F");
    theme_color!(accent_purple, "accent.purple", "#A78BFA");

    // Workspace tab / focused panel tokens.
    theme_color!(tab_text, "tab.text", "#B9BDC6");
    theme_color!(tab_text_muted, "tab.text_muted", "#8F949F");
    theme_color!(tab_text_active, "tab.text_active", "#EBEDF2");
    theme_color!(tab_indicator_active, "tab.indicator_active", "#2FC9D6");
    theme_color!(tab_bg_active, "tab.backgroundActive", "#2FC9D61A");
    theme_color!(tab_bg_hover, "tab.backgroundHover", "#383D48");
    theme_color!(panel_border_focused, "panel.border_focused", "#2FC9D68C");
    theme_color!(panel_header_active, "panel.header_active", "#2FC9D6");

    // DAW-specific
    theme_color!(meter_bg, "meter.background", "#0000002E");
    theme_color!(meter_low, "meter.low", "#43D18A");
    theme_color!(meter_mid, "meter.mid", "#E8B75C");
    theme_color!(meter_high, "meter.high", "#F2645F");
    theme_color!(fader_rail, "fader.rail", "#00000029");
    theme_color!(fader_thumb, "fader.thumb", "#C8CCD4");
    theme_color!(fader_tick, "fader.tick", "#FFFFFF14");
    theme_color!(fader_scale_text, "fader.scaleText", "#8F949F");
    theme_color!(knob_bg, "knob.background", "#14161C");
    theme_color!(knob_ring, "knob.ring", "#2FC9D6");
    theme_color!(slot_bg, "slot.background", "#22252E");
    theme_color!(slot_border, "slot.border", "#FFFFFF12");
    theme_color!(statusbar_bg, "statusbar.background", "#14161C");
    theme_color!(statusbar_text, "statusbar.text", "#8F949F");
    theme_color!(mixer_bg, "mixer.background", "#0E1015");
    theme_color!(master_strip_bg, "mixer.masterStripBackground", "#191B22");
    theme_color!(timeline_grid_major, "timeline.gridMajor", "#FFFFFF1C");
    theme_color!(timeline_grid_minor, "timeline.gridMinor", "#FFFFFF0D");
    theme_color!(timeline_grid_bar, "timeline.gridBar", "#FFFFFF3D");
    theme_color!(timeline_playhead, "timeline.playhead", "#FF6A5A");
    theme_color!(timeline_background, "timeline.background", "#16181F");
    theme_color!(
        timeline_content_background,
        "timeline.contentBackground",
        "#16181F"
    );
    theme_color!(
        timeline_region_background,
        "timeline.regionBackground",
        "#FFFFFF0A"
    );
    theme_color!(
        timeline_region_background_alt,
        "timeline.regionBackgroundAlt",
        "#FFFFFF05"
    );
    theme_color!(
        timeline_lane_background,
        "timeline.laneBackground",
        "#FFFFFF07"
    );
    theme_color!(
        timeline_lane_alt_background,
        "timeline.laneAltBackground",
        "#00000016"
    );
    theme_color!(
        timeline_selected_lane_background,
        "timeline.selectedLaneBackground",
        "#2FC9D614"
    );
    theme_color!(
        timeline_empty_body_background,
        "timeline.emptyBodyBackground",
        "#00000024"
    );
    theme_color!(
        timeline_ruler_background,
        "timeline.rulerBackground",
        "#22252E"
    );
    theme_color!(timeline_ruler_tick, "timeline.rulerTick", "#FFFFFF45");
    theme_color!(timeline_ruler_text, "timeline.rulerText", "#D3D7DE");
    theme_color!(timeline_selection, "timeline.selection", "#2FC9D62E");

    // Track colors (fallbacks)
    theme_color!(track_audio, "track.audio", "#38C7B4");
    theme_color!(track_midi, "track.midi", "#E8B75C");
    theme_color!(track_instrument, "track.instrument", "#5FD98C");
    theme_color!(track_bus, "track.bus", "#7FA8FF");
    theme_color!(track_return, "track.return", "#43D18A");
    theme_color!(track_master, "track.master", "#EBEDF2");
    // Subdued overlays for track row states — graphite-leaning so the selected
    // track reads as elevated without flooding the header with accent hue.
    theme_color!(track_selected_overlay, "track.selectedOverlay", "#FFFFFF0F");
    theme_color!(track_muted_overlay, "track.mutedOverlay", "#0E10158C");

    // Surface selection states (used by rows/lanes that shouldn't get the full
    // accent treatment — sublanes, list selections).
    theme_color!(surface_selected, "surface.selected", "#4A4F5C");
    theme_color!(surface_selected_soft, "surface.selectedSoft", "#414652");
    theme_color!(surface_pressed, "surface.pressed", "#383D48");
    theme_color!(surface_muted, "surface.muted", "#191B22");

    // Extra named accents kept distinct from the purple primary.
    theme_color!(accent_cyan, "accent.cyan", "#2FC9D6");
    theme_color!(accent_green, "accent.green", "#5FD98C");

    // Neutral state layers. Composited over an element's rest fill via
    // [`Colors::composite`] rather than replacing it, so one set of alphas
    // behaves correctly on every surface in the ramp.
    theme_color!(state_hover, "state.hover", "#FFFFFF0F");
    theme_color!(state_pressed, "state.pressed", "#FFFFFF1A");
    theme_color!(state_selected, "state.selected", "#FFFFFF1A");
    theme_color!(state_selected_hover, "state.selectedHover", "#FFFFFF24");
    theme_color!(state_dragged, "state.dragged", "#FFFFFF29");
    theme_color!(state_armed, "state.armed", "#2FC9D62E");
    // Pressed goes *darker* than rest on a dark theme, which reads as physical
    // depression with no bevel.
    theme_color!(state_recessed, "state.recessed", "#0000001F");
    theme_color!(state_scrim, "state.scrim", "#05070BA6");
    theme_color!(state_focus_ring, "state.focusRing", "#2FC9D6E0");

    // Latched DAW track states.
    //
    // Each gets its own hue and `accent.primary` is deliberately absent: on a
    // selected, focused, playing, armed track the accent already marks four
    // things, so reusing it here would make "is anything soloed?" unanswerable
    // at a glance across a 40-track arrangement. Five well-separated hues,
    // each carried on fill *and* border *and* glyph.
    theme_color!(state_mute, "state.mute", "#6F9BFF");
    theme_color!(state_solo, "state.solo", "#E8B75C");
    theme_color!(state_arm, "state.arm", "#F2645F");
    theme_color!(state_monitor, "state.monitor", "#43D18A");
    theme_color!(state_automation, "state.automation", "#A78BFA");

    // Automation sublane tokens — quiet graphite lanes with a purple curve so the
    // envelope is the only saturated element in the section.
    theme_color!(automation_curve, "automation.curve", "#A78BFA");
    theme_color!(automation_curve_hover, "automation.curveHover", "#C4B2FD");
    // Left header/label tint (opaque — sits over the header column, not the grid).
    theme_color!(automation_lane_bg, "automation.laneBg", "#14161C");
    theme_color!(
        automation_lane_bg_selected,
        "automation.laneBgSelected",
        "#1A1D24"
    );
    theme_color!(
        automation_lane_header_bg,
        "automation.laneHeaderBg",
        "#191B22"
    );
    // Right-side lane body. TRANSLUCENT overlays (8-digit RGBA) so the timeline
    // grid drawn behind the rows stays visible — never an opaque dark block.
    // Selected ≈ rgba(124,92,255,0.05) over the timeline canvas.
    theme_color!(automation_canvas_bg, "automation.canvasBg", "#0E101524");
    theme_color!(
        automation_canvas_bg_selected,
        "automation.canvasBgSelected",
        "#A78BFA14"
    );
    // Faint value/center guides drawn behind the curve.
    theme_color!(
        automation_value_region_bg,
        "automation.valueRegionBg",
        "#A78BFA0F"
    );
    theme_color!(automation_center_line, "automation.centerLine", "#A78BFA3D");
    theme_color!(automation_center_band, "automation.centerBand", "#A78BFA0A");
    theme_color!(automation_separator, "automation.separator", "#FFFFFF0D");
    theme_color!(
        automation_separator_strong,
        "automation.separatorStrong",
        "#FFFFFF1F"
    );
    theme_color!(automation_rail, "automation.rail", "#6B5CA8");
    theme_color!(automation_rail_active, "automation.railActive", "#A78BFA");
    theme_color!(automation_point, "automation.point", "#D6C9FE");

    // Compact button surface tokens shared by chrome controls.
    theme_color!(button_bg, "button.bg", "#22252E");
    theme_color!(button_bg_hover, "button.bgHover", "#31353F");
    theme_color!(button_bg_pressed, "button.bgPressed", "#1A1D24");
    theme_color!(button_bg_active, "button.bgActive", "#1E4650");
    theme_color!(button_border, "button.border", "#FFFFFF12");
    theme_color!(button_border_hover, "button.borderHover", "#FFFFFF29");
    theme_color!(button_text, "button.text", "#EBEDF2");
    theme_color!(button_text_muted, "button.textMuted", "#8F949F");

    // Surfaces
    theme_color!(bottom_panel_bg, "surface.bottomPanel", "#22252E");
    theme_color!(
        bottom_panel_header_bg,
        "surface.bottomPanelHeader",
        "#14161C"
    );
    theme_color!(mixer_strip_bg, "surface.mixerStrip", "#22252E");
    theme_color!(mixer_strip_bg_alt, "surface.mixerStripAlt", "#1E212A");
    theme_color!(
        mixer_strip_selected_bg,
        "surface.mixerStripSelected",
        "#4A4F5C"
    );
    theme_color!(
        master_strip_header_bg,
        "surface.masterStripHeader",
        "#16181F"
    );

    // Borders
    theme_color!(panel_border, "border.panel", "#FFFFFF0D");
    theme_color!(strip_border, "border.strip", "#FFFFFF12");
    theme_color!(strip_border_subtle, "border.stripSubtle", "#FFFFFF0A");
    theme_color!(master_strip_border, "border.masterStrip", "#FFFFFF1F");

    // Slots
    theme_color!(slot_bg_hover, "slot.backgroundHover", "#31353F");
    theme_color!(slot_empty_text, "slot.emptyText", "#6F7480");

    // Fader
    theme_color!(fader_groove, "fader.groove", "#0E1015");
    theme_color!(fader_thumb_border, "fader.thumbBorder", "#FFFFFF33");

    // Meters
    theme_color!(meter_rail, "meter.rail", "#00000024");
    theme_color!(meter_peak, "meter.peak", "#EBEDF2");
    // Latched clip cap. Deliberately a different, hotter red than
    // `meter_high` — a clip indicator painted in the same hex as the meter's
    // own top band is invisible exactly when it matters.
    theme_color!(meter_clip, "meter.clip", "#FF2D20");

    // Status
    theme_color!(statusbar_text_muted, "statusbar.textMuted", "#6F7480");
    theme_color!(statusbar_accent, "statusbar.accent", "#2FC9D6");
    theme_color!(statusbar_warning, "statusbar.warning", "#E8B75C");

    /// Track-tinted audio clip body overlay. The arrangement grid is deliberately
    /// painted behind clips, so these alphas are part of the timeline layering
    /// contract rather than arbitrary component-local styling.
    pub fn timeline_audio_clip_fill(track_color: Rgba, selected: bool) -> Rgba {
        Self::with_alpha(track_color, if selected { 0.32 } else { 0.16 })
    }

    /// Audio clip outline, kept substantially stronger than the translucent body.
    pub fn timeline_audio_clip_border(track_color: Rgba, selected: bool) -> Rgba {
        Self::with_alpha(track_color, if selected { 0.95 } else { 0.62 })
    }

    /// Waveforms remain close to full strength even though their clip body is
    /// translucent, preserving transient readability against the visible grid.
    pub fn timeline_audio_clip_waveform(track_color: Rgba) -> Rgba {
        Self::with_alpha(track_color, 0.92)
    }

    // Helper to dynamically adjust alpha channel
    pub fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
        Rgba {
            r: color.r,
            g: color.g,
            b: color.b,
            a: alpha,
        }
    }

    /// Source-over composite of a translucent `layer` onto `base` (opaque, or
    /// itself a tint over the window).
    ///
    /// This is what makes the state-layer model implementable at all. A GPUI
    /// div has exactly one `background`, so `.hover(|s| s.bg(state_hover()))`
    /// *replaces* the rest fill with a 6%-white wash over whatever happens to
    /// be behind the element — not a 6% lift of the control itself. Every
    /// hover/pressed/selected rule therefore resolves its color up front:
    ///
    /// ```ignore
    /// let rest = Colors::button_bg();
    /// let hover = Colors::composite(rest, Colors::state_hover());
    /// div().bg(rest).hover(move |s| s.bg(hover))
    /// ```
    ///
    /// Control-path only: this is a few float ops, but it belongs in the style
    /// closure's captured value, not inside a per-frame paint loop.
    pub fn composite(base: Rgba, layer: Rgba) -> Rgba {
        // Porter-Duff "over". On an opaque base this is the plain blend; on a
        // see-through one (a theme whose surfaces are tints over the window)
        // the base's colour only counts as far as it is there — otherwise a
        // faint white strip under a dark recess layer came out light grey.
        let la = layer.a.clamp(0.0, 1.0);
        let ba = base.a.clamp(0.0, 1.0);
        // The result is never more see-through than the base: a state layer
        // must never punch a hole through the control it is lifting.
        let a = la + ba * (1.0 - la);
        if a <= f32::EPSILON {
            return Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            };
        }
        let k = ba * (1.0 - la);
        Rgba {
            r: (layer.r * la + base.r * k) / a,
            g: (layer.g * la + base.g * k) / a,
            b: (layer.b * la + base.b * k) / a,
            a,
        }
    }

    /// Rest fill lifted by a neutral state layer at an explicit alpha from
    /// [`crate::theme::state`].
    pub fn lift(base: Rgba, alpha: f32) -> Rgba {
        Self::composite(base, Self::with_alpha(Self::state_hover(), alpha))
    }

    /// Foreground that stays readable on an arbitrary fill.
    ///
    /// Needed wherever the *content* picks the background — a mixer name plate
    /// filled with the track colour, a swatch, a coloured badge. The palette
    /// spans pale yellows and deep blues, so a fixed light or dark text colour
    /// is unreadable on half of it.
    ///
    /// Uses relative luminance (Rec. 709 coefficients, the same weighting
    /// WCAG's contrast formula is built on) rather than a naive average, so a
    /// saturated green reads as light and a saturated blue as dark, which is
    /// how the eye sees them. The 0.55 threshold sits where both `text_inverse`
    /// and `text_primary` clear 4.5:1 against the default palette.
    pub fn on_color(fill: Rgba) -> Rgba {
        let luminance = 0.2126 * fill.r + 0.7152 * fill.g + 0.0722 * fill.b;
        if luminance > 0.55 {
            Self::text_inverse()
        } else {
            Self::text_primary()
        }
    }

    /// Fill and border for a latched DAW toggle (mute, solo, arm, monitor,
    /// automation-write). Returns `(fill, border)`; the glyph itself is painted
    /// at the full `semantic` color, so the state reads on three channels.
    pub fn latched(base: Rgba, semantic: Rgba) -> (Rgba, Rgba) {
        (
            Self::composite(
                base,
                Self::with_alpha(semantic, crate::theme::state::ARMED_WASH),
            ),
            Self::with_alpha(semantic, crate::theme::state::ARMED_BORDER),
        )
    }

    pub const TRACK_COLORS: [u32; 12] = DEFAULT_TRACK_COLOR_VALUES;

    pub fn track_color_for_index(index: usize) -> Rgba {
        let theme = active_theme_store()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        theme
            .track_colors
            .get(index % theme.track_colors.len())
            .copied()
            .unwrap_or_else(|| rgba_from_u32(Self::TRACK_COLORS[index % Self::TRACK_COLORS.len()]))
    }
}

#[cfg(test)]
mod font_stack_tests {
    use super::*;

    /// All normal UI text resolves to one composite descriptor carrying a
    /// coverage-ordered fallback chain — no language input, no per-language font.
    #[test]
    fn ui_font_is_a_single_composite_with_a_fallback_chain() {
        let font = ui_font();
        let fallbacks = font
            .fallbacks
            .expect("composite UI font must attach a fallback chain");
        assert!(
            !fallbacks.fallback_list().is_empty(),
            "composite UI font needs a non-empty fallback chain"
        );
    }

    /// Weight selection is a descriptor axis, not a family swap: the family is
    /// identical across weights.
    #[test]
    fn weight_variants_share_the_same_anchor_family() {
        assert_eq!(
            ui_font_weight(FontWeight::NORMAL).family,
            ui_font_weight(FontWeight::BOLD).family
        );
        assert_eq!(ui_font_weight(FontWeight::BOLD).weight, FontWeight::BOLD);
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;

    fn rgba(r: f32, g: f32, b: f32, a: f32) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// Over the window colour, then onto black: what reaches the screen.
    fn on_black(c: Rgba) -> f32 {
        c.r * c.a
    }

    #[test]
    fn an_opaque_base_blends_as_before() {
        let c = Colors::composite(rgba(0.2, 0.2, 0.2, 1.0), rgba(0.0, 0.0, 0.0, 0.5));
        assert!((c.r - 0.1).abs() < 1.0e-6 && (c.a - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn a_dark_layer_on_a_faint_tint_stays_dark() {
        // A theme's strip: white at 3% over the window. A 12% black recess on
        // it must come out darker than the strip, not light grey.
        let strip = rgba(1.0, 1.0, 1.0, 0.03);
        let well = Colors::composite(strip, rgba(0.0, 0.0, 0.0, 0.12));
        assert!(on_black(well) < on_black(strip), "{well:?}");
        assert!(well.a >= strip.a);
    }
}

#[cfg(test)]
mod theme_discovery_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn scratch_dir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "futureboard_theme_discovery_test_{}_{id}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// A user-authored theme dropped as a flat `<Name>.json` directly in the
    /// themes root (no `theme.json` wrapper folder) must be discovered, since
    /// that is the documented custom-theme drop path (`{appdata}/Extensions/
    /// Themes/*.json`).
    #[test]
    fn discovers_flat_custom_theme_json_in_root() {
        let dir = scratch_dir();
        let custom_path = dir.join("MyCustom.json");
        fs::write(
            &custom_path,
            r##"{"id":"user.mycustom","name":"My Custom","tokens":{"surface":{"base":"#123456"}}}"##,
        )
        .unwrap();

        let found = discover_theme_files(&dir);
        assert!(
            found.contains(&custom_path),
            "flat custom theme not discovered: {found:?}"
        );

        let default = load_default_theme();
        let loaded = load_theme_file(&custom_path, &default).expect("valid theme json");
        assert_eq!(loaded.id, "user.mycustom");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Packaged themes still install and are discovered via their
    /// `<Name>/theme.json` layout alongside flat custom themes.
    #[test]
    fn discovers_nested_theme_json_alongside_flat_custom_json() {
        let dir = scratch_dir();
        install_builtin_theme_templates(&dir);
        fs::write(
            dir.join("MyCustom.json"),
            r#"{"id":"user.mycustom2","name":"My Custom 2","tokens":{}}"#,
        )
        .unwrap();

        let found = discover_theme_files(&dir);
        assert!(found.iter().any(|p| p.ends_with("Default/theme.json")));
        assert!(found.iter().any(|p| p.ends_with("Light/theme.json")));
        assert!(found.iter().any(|p| p.ends_with("MyCustom.json")));

        let _ = fs::remove_dir_all(&dir);
    }
}
