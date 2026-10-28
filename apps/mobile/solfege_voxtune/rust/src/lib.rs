//! `canto_dsp` — the Android/Flutter shell around `sphere_voxtune`.
//!
//! The engine itself (analysis, notes, TD-PSOLA rendering, background
//! loading) lives in `crates/SphereVoxtune`, shared with the desktop app,
//! Futureboard Studio's tuner panel and the VST3/ARA plug-in. This crate adds
//! only what is specific to the phone:
//! - `ffi`: C-ABI surface for Flutter (`dart:ffi`)
//! - `output`: the AAudio output stream
//! - `recorder`: AAudio microphone capture with live pitch

// The engine's modules under their old paths, so `crate::engine::…` and the
// FFI keep resolving.
pub use sphere_voxtune::*;

#[cfg(target_os = "android")]
pub(crate) mod aaudio;
pub mod ffi;
pub mod output;
pub mod recorder;