//! Studio's composite UI font, without its diagnostics window.

#[path = "../../../../crates/SphereUIComponents/src/fonts/chains.rs"]
mod chains;
#[path = "../../../../crates/SphereUIComponents/src/fonts/manager.rs"]
mod manager;

pub use manager::{CompositeRole, FontManager};

use gpui::{Font, FontWeight};

pub fn ui_font(weight: FontWeight) -> Font {
    FontManager::global().ui_font(weight)
}

pub fn display_font(weight: FontWeight) -> Font {
    FontManager::global().display_font(weight)
}
