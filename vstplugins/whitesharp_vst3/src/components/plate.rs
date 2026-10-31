//! Font helpers used by Studio's shared WhiteSharp styling module.
//!
//! The VST3 editor mounts only the Studio components it uses, so this module
//! provides the font helpers that `white_sharp_style` shares with the plate
//! editors without pulling in their editor implementation.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{App, Font, FontFallbacks, FontWeight};

static FACES: [&[u8]; 8] = [
    include_bytes!("../../../../crates/SphereUIComponents/assets/fonts/outfit/Outfit-Light.ttf"),
    include_bytes!("../../../../crates/SphereUIComponents/assets/fonts/outfit/Outfit-Regular.ttf"),
    include_bytes!("../../../../crates/SphereUIComponents/assets/fonts/outfit/Outfit-Medium.ttf"),
    include_bytes!("../../../../crates/SphereUIComponents/assets/fonts/outfit/Outfit-SemiBold.ttf"),
    include_bytes!("../../../../crates/SphereUIComponents/assets/fonts/outfit/Outfit-Bold.ttf"),
    include_bytes!(
        "../../../../crates/SphereUIComponents/assets/fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf"
    ),
    include_bytes!(
        "../../../../crates/SphereUIComponents/assets/fonts/ibm-plex-mono/IBMPlexMono-Medium.ttf"
    ),
    include_bytes!(
        "../../../../crates/SphereUIComponents/assets/fonts/ibm-plex-mono/IBMPlexMono-SemiBold.ttf"
    ),
];
static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Register WhiteSharp's embedded typefaces once in the plug-in process.
pub fn ensure_fonts(cx: &App) {
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    let faces = FACES.map(std::borrow::Cow::Borrowed).to_vec();
    if let Err(error) = cx.text_system().add_fonts(faces) {
        eprintln!("[plate] Outfit / IBM Plex Mono could not be registered: {error}");
        REGISTERED.store(false, Ordering::Release);
    }
}

/// Outfit with Studio's UI font chain as a fallback for unsupported glyphs.
pub fn outfit(weight: FontWeight) -> Font {
    let mut font = crate::theme::ui_font_weight(weight);
    let mut chain = vec![font.family.to_string()];
    if let Some(fallbacks) = font.fallbacks.as_ref() {
        chain.extend(fallbacks.fallback_list().iter().cloned());
    }
    font.family = "Outfit".into();
    font.fallbacks = Some(FontFallbacks::from_fonts(chain));
    font
}
