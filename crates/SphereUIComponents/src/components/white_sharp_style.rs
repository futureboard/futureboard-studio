//! WhiteSharp's own look: the faceplate's palette, its pitch-class colours,
//! and the Outfit face it is set in, embedded and registered once.
//!
//! The plate is drawn at a design size of [`PLATE_W`] × [`PLATE_H`] logical
//! pixels and scaled to the window by [`plate_scale`]; every size below is
//! at the design size.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{hsla, px, rgb, rgba, App, Font, FontFallbacks, FontWeight, Hsla, Pixels, Rgba};

pub const PLATE_W: f32 = 1024.0;
pub const PLATE_H: f32 = 640.0;

/// The faceplate's fields, top to bottom, and the ring around the dial.
pub const PLATE: u32 = 0x141414;
pub const BAND: u32 = 0x2b2b2b;
pub const FLOOR: u32 = 0x212121;
/// Text and marks.
pub const INK: u32 = 0xe6e6e6;
pub const DIM: u32 = 0x8d8d8d;
pub const MUTE: u32 = 0x6a6a6a;

/// The band's pitch window, in semitones from the tonic it is anchored on.
pub const BAND_MIN_REL: f32 = -1.6;
pub const BAND_MAX_REL: f32 = 9.2;

/// Where the band sits, as shares of the plate's height.
pub const BAND_TOP: f32 = 0.357_812_5;
pub const BAND_HEIGHT: f32 = 0.284_375;
/// The ring's diameter, as a share of the plate's design width.
pub const ORB: f32 = 0.342_773_44;
/// The dial's face inside the ring, as a share of the ring.
pub const ORB_HOLE: f32 = 0.92;

pub fn color(hex: u32) -> Rgba {
    rgb(hex)
}

/// `hex` (0xRRGGBB) at `alpha`.
pub fn color_a(hex: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((hex << 8) | a)
}

/// Each pitch class's hue, C on red and once round the wheel: a note keeps
/// its colour in every octave.
const HUES: [f32; 12] = [
    6.0, 26.0, 46.0, 72.0, 98.0, 136.0, 164.0, 188.0, 214.0, 246.0, 276.0, 316.0,
];

pub fn pitch_hue(class: i32) -> f32 {
    HUES[class.rem_euclid(12) as usize]
}

/// Pitch class `class`'s ink at `saturation` and `light` (0–100) — the
/// faceplate's `hsl(hue s% l% / a)`.
pub fn pitch_hsl(class: i32, saturation: f32, light: f32, alpha: f32) -> Hsla {
    hsla(
        pitch_hue(class) / 360.0,
        saturation / 100.0,
        light / 100.0,
        alpha.clamp(0.0, 1.0),
    )
}

/// The ink most pitch marks use: 64 % saturation.
pub fn pitch_ink(class: i32, alpha: f32, light: f32) -> Hsla {
    pitch_hsl(class, 64.0, light, alpha)
}

/// [`pitch_ink`], or its grey when the plug-in is bypassed.
pub fn pitch_ink_or_grey(class: i32, alpha: f32, light: f32, grey: bool) -> Hsla {
    if grey {
        hsla(0.0, 0.0, light / 100.0, alpha.clamp(0.0, 1.0))
    } else {
        pitch_ink(class, alpha, light)
    }
}

/// The keys as the selector names them.
pub const KEY_LABELS: [&str; 12] = [
    "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
];
const SHARPS: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];
const FLATS: [&str; 12] = [
    "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
];

/// Whether notes in `key` are spelled with flats.
pub fn uses_flats(key: u8) -> bool {
    matches!(key % 12, 3 | 5 | 8 | 10)
}

/// MIDI note `midi`'s name, spelled for `key`.
pub fn spell(midi: f32, key: u8) -> &'static str {
    let class = (midi.round() as i32).rem_euclid(12) as usize;
    if uses_flats(key) {
        FLATS[class]
    } else {
        SHARPS[class]
    }
}

/// The scale's name on the selector and under the note.
pub fn scale_short(scale: whitesharp::Scale) -> &'static str {
    use whitesharp::Scale::*;
    match scale {
        Chromatic => "Chromatic",
        Major => "Major",
        Minor => "Minor",
        HarmonicMinor => "Harmonic",
        MelodicMinor => "Melodic",
        MajorPentatonic => "Pentatonic",
        MinorPentatonic => "Min Pent",
        Blues => "Blues",
        Dorian => "Dorian",
        Phrygian => "Phrygian",
        Lydian => "Lydian",
        Mixolydian => "Mixolydian",
    }
}

/// Upper-case `text` with a hair space between its letters: the plate's
/// tracked captions (GPUI text has no letter spacing of its own).
pub fn tracked(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 4);
    for (i, ch) in text.chars().flat_map(char::to_uppercase).enumerate() {
        if i > 0 {
            out.push('\u{200A}');
        }
        out.push(ch);
    }
    out
}

/// A font size at the design size `size`, scaled by `scale`, undoing
/// Studio's global text scale so the plate's type keeps the mockup's sizes.
pub fn type_px(size: f32, scale: f32) -> Pixels {
    px(size * scale / crate::theme::UI_TEXT_SCALE)
}

const OUTFIT_FAMILY: &str = "Outfit";
static OUTFIT_LIGHT: &[u8] = include_bytes!("../../assets/fonts/outfit/Outfit-Light.ttf");
static OUTFIT_REGULAR: &[u8] = include_bytes!("../../assets/fonts/outfit/Outfit-Regular.ttf");
static OUTFIT_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/outfit/Outfit-Medium.ttf");
static OUTFIT_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Adds the embedded Outfit faces (300, 400, 500) to the text system, once
/// per process.
pub fn ensure_fonts(cx: &App) {
    if OUTFIT_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    let faces = [OUTFIT_LIGHT, OUTFIT_REGULAR, OUTFIT_MEDIUM]
        .map(std::borrow::Cow::Borrowed)
        .to_vec();
    if let Err(error) = cx.text_system().add_fonts(faces) {
        eprintln!("[whitesharp] Outfit could not be registered: {error}");
        OUTFIT_REGISTERED.store(false, Ordering::Release);
    }
}

/// Outfit at `weight`, falling back to Studio's UI chain for anything it
/// lacks (the hair space, non-Latin text).
pub fn outfit(weight: FontWeight) -> Font {
    let mut font = crate::theme::ui_font_weight(weight);
    let mut chain = vec![font.family.to_string()];
    if let Some(fallbacks) = font.fallbacks.as_ref() {
        chain.extend(fallbacks.fallback_list().iter().cloned());
    }
    font.family = OUTFIT_FAMILY.into();
    font.fallbacks = Some(FontFallbacks::from_fonts(chain));
    font
}

/// The plate's scale for a content area of `width` × `height`: the design
/// size fitted inside it.
pub fn plate_scale(width: f32, height: f32) -> f32 {
    (width / PLATE_W).min(height / PLATE_H).clamp(0.5, 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_spelled_for_their_key() {
        assert_eq!(spell(61.0, 0), "C#");
        assert_eq!(spell(61.0, 5), "Db");
        assert_eq!(spell(70.4, 10), "Bb");
        assert_eq!(spell(-1.0, 0), "B");
    }

    #[test]
    fn captions_are_tracked_upper_case() {
        assert_eq!(tracked("Mix"), "M\u{200A}I\u{200A}X");
        assert_eq!(tracked(""), "");
    }

    #[test]
    fn the_plate_fits_its_area() {
        assert_eq!(plate_scale(1024.0, 640.0), 1.0);
        assert_eq!(plate_scale(2048.0, 640.0), 1.0);
        assert_eq!(plate_scale(512.0, 640.0), 0.5);
    }
}
