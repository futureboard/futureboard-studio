//! The panel sizes its plate as the window less `TITLEBAR_HEIGHT` and the
//! 30 px identity strip under it. The plug-in draws neither, so this cancels
//! the strip and the plate gets the whole window.
pub const TITLEBAR_HEIGHT: f32 = -30.0;
