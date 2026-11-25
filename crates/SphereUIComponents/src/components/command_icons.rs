//! The Futureboard command icon set: one glyph per kind of command, shared by
//! every context menu.
//!
//! A context menu row names a command id (`clip:delete`, `track:freeze`,
//! `marker:add-here`); [`command_icon`] reads the glyph from the id, so every
//! menu built from [`ContextMenuEntry`](super::context_menu::ContextMenuEntry)
//! gets its icons without each call site choosing one. The verb decides first
//! (copy, delete, rename, split, freeze…), then the area the command acts on
//! (a marker, a region, the tempo track). Rows that pick one value from a list
//! (a grid size, a key, an output, a preset) have none: a glyph repeated on
//! every option is noise.
//!
//! The glyphs are Lucide's, from `packages/shared/lucide`, embedded under
//! `icons/menu/`. A drawn menu paints them with `svg()`; a native one gets
//! them rasterised by [`install_menu_icon_rasterizer`].

use crate::native_popup_menu::MenuIcon;
use crate::theme::Colors;

/// One glyph of the set: its asset path and its SVG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandIcon {
    pub path: &'static str,
    pub svg: &'static str,
}

macro_rules! icon_set {
    ($($name:literal),+ $(,)?) => {
        /// Every glyph the set uses, by Lucide name.
        const ICONS: &[(&str, CommandIcon)] = &[
            $((
                $name,
                CommandIcon {
                    path: concat!("icons/menu/", $name, ".svg"),
                    svg: include_str!(concat!(
                        "../../../../packages/shared/lucide/icons/",
                        $name,
                        ".svg"
                    )),
                },
            )),+
        ];
    };
}

icon_set![
    "copy",
    "scissors",
    "clipboard-paste",
    "undo-2",
    "copy-plus",
    "trash-2",
    "folder-minus",
    "folder-plus",
    "folder-open",
    "eraser",
    "pencil",
    "split",
    "combine",
    "layers",
    "snowflake",
    "audio-waveform",
    "palette",
    "volume-x",
    "headphones",
    "circle-dot",
    "play",
    "rewind",
    "fast-forward",
    "eye-off",
    "eye",
    "zoom-in",
    "zoom-out",
    "magnet",
    "grid-3x3",
    "repeat",
    "arrow-right-to-line",
    "hand",
    "spline",
    "move-vertical",
    "timer",
    "music",
    "rotate-ccw",
    "file-down",
    "file-up",
    "scan-search",
    "refresh-cw",
    "save",
    "power",
    "settings",
    "piano",
    "external-link",
    "list-checks",
    "route",
    "git-merge",
    "send",
    "sliders-horizontal",
    "keyboard-music",
    "wand-sparkles",
    "file-music",
    "type",
    "map-pin",
    "flag",
    "gauge",
    "plus",
    "arrow-left-right",
    "unlink",
    // Not a command glyph: the Keymap window's JSON view toggle.
    "braces",
];

fn by_name(name: &str) -> Option<CommandIcon> {
    ICONS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, icon)| *icon)
}

/// The SVG behind an `icons/menu/…` asset path, for the asset source.
pub fn svg(path: &str) -> Option<&'static str> {
    ICONS
        .iter()
        .find(|(_, icon)| icon.path == path)
        .map(|(_, icon)| icon.svg)
}

/// Commands that pick one value from a list: their rows carry no glyph.
const CHOICE_PREFIXES: &[&str] = &[
    "timeline:set-grid",
    "key:root",
    "key:scale",
    "preset:",
    "ara:",
    "assign:",
    "metronome:set-volume",
    "recording:set-count-in-bars",
    "ts:set:",
    "midi:articulation",
    "mixer:set-output",
    "mixer:pan-mode",
    "mixer:add-send-to",
    "sidechain",
    "track:height-small",
    "track:height-normal",
    "track:height-large",
    "track:height-huge",
];

/// The glyph for a command id, or `None` for a row that should not carry one.
pub fn command_icon(command: &str) -> Option<CommandIcon> {
    icon_name(command).and_then(by_name)
}

/// The Lucide name for a command id. See the module docs for the order.
fn icon_name(command: &str) -> Option<&'static str> {
    let c = command.to_ascii_lowercase();
    if c.is_empty() || c == "noop" || c == "none" {
        return None;
    }
    if CHOICE_PREFIXES.iter().any(|prefix| c.starts_with(prefix)) {
        return None;
    }
    let words: Vec<&str> = c
        .split(|ch: char| matches!(ch, ':' | '.' | '-' | '_' | '|' | '/'))
        .filter(|word| !word.is_empty())
        .collect();
    let has = |word: &str| words.contains(&word);
    let area = words.first().copied().unwrap_or("");

    // The verb, most specific first.
    let verb = if has("copy") {
        Some("copy")
    } else if has("cut") {
        Some("scissors")
    } else if has("paste") {
        Some("clipboard-paste")
    } else if has("undo") {
        Some("undo-2")
    } else if has("duplicate") {
        Some("copy-plus")
    } else if has("folder") && (has("remove") || has("ungroup")) || has("ungroup") {
        Some("folder-minus")
    } else if has("toggle") && has("folder") {
        Some("folder-open")
    } else if c == "group" || (has("group") && has("selected")) {
        Some("folder-plus")
    } else if has("delete") || (has("remove") && !has("folder")) {
        Some("trash-2")
    } else if has("clear") || has("erase") {
        Some("eraser")
    } else if has("rename") {
        Some("pencil")
    } else if has("split") || has("slice") {
        Some("split")
    } else if has("consolidate") {
        Some("combine")
    } else if has("flatten") || has("takes") {
        Some("layers")
    } else if has("freeze") || has("unfreeze") {
        Some("snowflake")
    } else if has("bounce") || has("normalize") {
        Some("audio-waveform")
    } else if has("reverse") {
        Some("arrow-left-right")
    } else if has("color") {
        Some("palette")
    } else if has("mute") {
        Some("volume-x")
    } else if has("solo") {
        Some("headphones")
    } else if has("record") {
        Some("circle-dot")
    } else if has("rewind") {
        Some("rewind")
    } else if has("fast") && has("forward") {
        Some("fast-forward")
    } else if has("play") || has("goto") {
        Some("play")
    } else if has("hide") {
        Some("eye-off")
    } else if (has("open") && has("track")) || has("show") {
        Some("eye")
    } else if has("zoom") && has("in") {
        Some("zoom-in")
    } else if has("zoom") && has("out") {
        Some("zoom-out")
    } else if has("snap") {
        Some("magnet")
    } else if has("quantize") {
        Some("grid-3x3")
    } else if has("loop") {
        Some("repeat")
    } else if has("move") && has("playhead") {
        Some("arrow-right-to-line")
    } else if has("tap") {
        Some("hand")
    } else if has("curve") || has("bend") {
        Some("spline")
    } else if has("fit") {
        Some("move-vertical")
    } else if has("timebase") && has("linear") {
        Some("timer")
    } else if has("timebase") && has("musical") {
        Some("music")
    } else if has("reset") {
        Some("rotate-ccw")
    } else if has("import") {
        Some("file-down")
    } else if has("export") {
        Some("file-up")
    } else if has("to") && has("midi") {
        Some("file-music")
    } else if has("reveal") || (has("open") && has("folder")) {
        Some("scan-search")
    } else if has("refresh") || has("scan") {
        Some("refresh-cw")
    } else if has("save") {
        Some("save")
    } else if has("quit") || has("bypass") || (has("re") && has("enable")) {
        Some("power")
    } else if has("settings") || has("preferences") || has("properties") {
        Some("settings")
    } else if has("editor") && area == "midi" {
        Some("piano")
    } else if has("editor") {
        Some("external-link")
    } else if has("select") && has("all") {
        Some("list-checks")
    } else if has("cycle") || has("target") {
        Some("route")
    } else if has("bus") {
        Some("git-merge")
    } else if has("send") {
        Some("send")
    } else if has("mixer") && has("toggle") {
        Some("sliders-horizontal")
    } else if has("keyboard") {
        Some("keyboard-music")
    } else if has("generator") {
        Some("wand-sparkles")
    } else if has("lyric") {
        Some("type")
    } else if has("disconnect") {
        Some("unlink")
    } else if has("edit") {
        Some("pencil")
    } else if has("load") || (has("open") && (area == "project" || area == "browser")) {
        Some("folder-open")
    } else if has("audio") && (has("add") || has("new")) {
        Some("audio-waveform")
    } else if has("midi") && (has("add") || has("new")) {
        Some("piano")
    } else {
        None
    };
    if verb.is_some() {
        return verb;
    }

    // The area, for what is left: adding to it, or acting on it.
    match area {
        "ruler" if has("tempo") => Some("gauge"),
        "ruler" if has("ts") => Some("music"),
        "ruler" if has("region") => Some("flag"),
        "ruler" if has("marker") => Some("map-pin"),
        "marker" => Some("map-pin"),
        "region" => Some("flag"),
        "tempo" => Some("gauge"),
        "ts" => Some("music"),
        "chords" => Some("piano"),
        "automation" => Some("spline"),
        "mixer" => Some("sliders-horizontal"),
        "songtext" | "song" => Some("type"),
        _ if has("add") || has("new") || has("create") => Some("plus"),
        _ => None,
    }
}

/// `icon` for a native menu row, in the menu's text colour.
pub fn native_menu_icon(icon: CommandIcon) -> MenuIcon {
    let tint = gpui::Rgba::from(Colors::text_secondary());
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    MenuIcon {
        svg: icon.svg,
        tint: [channel(tint.r), channel(tint.g), channel(tint.b)],
    }
}

/// Let native menus draw icons, with GPUI's own SVG renderer. Idempotent;
/// called whenever a native menu is about to open.
pub fn install_menu_icon_rasterizer(cx: &gpui::App) {
    if crate::native_popup_menu::has_icon_rasterizer() {
        return;
    }
    let renderer = cx.svg_renderer();
    crate::native_popup_menu::set_icon_rasterizer(move |svg, size| {
        let edge = gpui::DevicePixels(size as i32);
        renderer
            .render_alpha_mask(
                &gpui::RenderSvgParams {
                    path: gpui::SharedString::default(),
                    size: gpui::size(edge, edge),
                },
                Some(svg.as_bytes()),
            )
            .ok()
            .flatten()
            .map(|(_, mask)| mask)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(command: &str) -> Option<&'static str> {
        icon_name(command)
    }

    #[test]
    fn verbs_pick_their_glyph() {
        assert_eq!(name("edit:copy"), Some("copy"));
        assert_eq!(name("automation:cut"), Some("scissors"));
        assert_eq!(name("context.paste"), Some("clipboard-paste"));
        assert_eq!(name("clip:duplicate"), Some("copy-plus"));
        assert_eq!(name("clip:delete"), Some("trash-2"));
        assert_eq!(name("marker:delete"), Some("trash-2"));
        assert_eq!(name("tempo:clear"), Some("eraser"));
        assert_eq!(name("track:rename"), Some("pencil"));
        assert_eq!(name("clip:split-at-playhead"), Some("split"));
        assert_eq!(name("track:freeze"), Some("snowflake"));
        assert_eq!(name("track:mute"), Some("volume-x"));
        assert_eq!(name("track:solo"), Some("headphones"));
        assert_eq!(name("view:zoom-in"), Some("zoom-in"));
        assert_eq!(name("timeline:toggle-snap"), Some("magnet"));
        assert_eq!(name("region:set-loop"), Some("repeat"));
        assert_eq!(name("track:remove-from-folder"), Some("folder-minus"));
        assert_eq!(name("track:group-selected"), Some("folder-plus"));
        assert_eq!(name("tempo:hide-track"), Some("eye-off"));
        assert_eq!(name("midi:export-clip"), Some("file-up"));
        assert_eq!(name("browser:import"), Some("file-down"));
    }

    #[test]
    fn areas_fill_in_where_no_verb_matches() {
        assert_eq!(name("marker:add-here"), Some("map-pin"));
        assert_eq!(name("ruler:add-region"), Some("flag"));
        assert_eq!(name("tempo:create"), Some("gauge"));
        assert_eq!(name("ts:add-marker"), Some("music"));
        assert_eq!(name("track:add-audio"), Some("audio-waveform"));
    }

    #[test]
    fn choices_and_placeholders_have_no_glyph() {
        assert_eq!(name("timeline:set-grid:1/8"), None);
        assert_eq!(name("key:root:3"), None);
        assert_eq!(name("preset:2"), None);
        assert_eq!(name("noop"), None);
        assert_eq!(name(""), None);
    }

    #[test]
    fn every_glyph_is_an_svg_served_at_its_path() {
        for (_, icon) in ICONS {
            assert!(icon.svg.contains("<svg"), "{}", icon.path);
            assert_eq!(svg(icon.path), Some(icon.svg));
        }
    }
}
