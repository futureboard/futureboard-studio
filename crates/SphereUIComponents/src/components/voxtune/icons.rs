//! The Lucide icons the Voxtune editor draws, embedded from
//! `packages/shared/lucide`. Paths are `voxtune/<name>.svg`; an asset source
//! serves them through [`svg`] (Studio's `EmbeddedAssets`, the desktop app's,
//! the plug-in's), so the editor needs nothing registered elsewhere.
//!
//! The `include_str!` paths are relative to this file, which is where both
//! Studio and the plug-in (mounting it with `#[path]`) compile it from.

macro_rules! icons {
    ($($konst:ident = $name:literal;)+) => {
        $(pub const $konst: &str = concat!("voxtune/", $name, ".svg");)+

        /// The SVG for an icon path, if it is one of these.
        pub fn svg(path: &str) -> Option<&'static str> {
            match path {
                $(
                    $konst => Some(include_str!(concat!(
                        "../../../../../packages/shared/lucide/icons/",
                        $name,
                        ".svg"
                    ))),
                )+
                _ => None,
            }
        }

        /// Every icon path (asset listing).
        pub const ALL: &[&str] = &[$($konst),+];
    };
}

icons! {
    TOOL_PITCH = "mouse-pointer-2";
    TOOL_DRIFT = "activity";
    TOOL_TIME = "move-horizontal";
    TOOL_FORMANT = "sparkles";
    TOOL_LEVEL = "volume-2";
    TOOL_CUT = "scissors";
    PLAY = "play";
    PAUSE = "pause";
    TO_START = "skip-back";
    LOOP = "repeat";
    UNDO = "undo-2";
    REDO = "redo-2";
    HISTORY = "history";
    ZOOM_IN = "zoom-in";
    ZOOM_OUT = "zoom-out";
    FIT = "scan";
    CHEVRON_DOWN = "chevron-down";
    CHEVRON_UP = "chevron-up";
    CHEVRONS_UP = "chevrons-up";
    CHEVRONS_DOWN = "chevrons-down";
    EAR = "ear";
    NOTE = "music-2";
    EXPRESSION = "spline";
    TAKE = "audio-waveform";
    TUNE = "crosshair";
    TUNE_ALL = "wand-sparkles";
    JOIN = "combine";
    RESET = "rotate-ccw";
    MUTE = "volume-x";
    CENTER = "focus";
    FLATTEN = "minus";
    QUANTIZE = "grid-3x3";
    LEVEL = "gauge";
    ACCENT = "zap";
    MAGNET = "magnet";
    VOCAL = "mic-vocal";
    OPEN = "folder-open";
    CHECK = "check";
    NONE = "ban";
    SLIDERS = "sliders-horizontal";
    BACK = "arrow-left";
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_icon_is_an_svg() {
        for path in super::ALL {
            let svg = super::svg(path).expect("listed");
            assert!(svg.contains("<svg"), "{path}");
        }
        assert!(super::svg("icons/play.svg").is_none());
    }
}
