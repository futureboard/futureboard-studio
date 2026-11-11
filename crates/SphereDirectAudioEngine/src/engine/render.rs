//! DSP / render kernel for the native audio engine.
//!
//! Split out of `engine.rs` (which owns device lifecycle, command dispatch, and
//! the public engine API) so the realtime render path can be read and modified
//! in isolation. This is a pure relocation of the free render/DSP functions that
//! previously lived inline in `engine.rs` — no behavior change.
//!
//! Realtime rules apply to everything reachable from
//! `render_project_block_interleaved`: no allocation, no locking, no blocking in
//! steady state. `use super::*;` pulls in the shared engine vocabulary
//! (`SharedState`, runtime types, consts, debug-flag helpers).
use super::*;

use crate::audio_graph::ChainJob;

use crate::dsp::simd;

use crate::monitor::{ListenMode, TapStage};

use SphereAudioProcessor::StretchBackend;

// ── Submodules ─────────────────────────────────────────────────────────────────

mod sample;
pub use self::sample::*;
mod clip;
pub(crate) use self::clip::*;
mod mix;
pub use self::mix::*;
mod block;
pub use self::block::*;
mod chains;
pub use self::chains::*;
mod bridge;
pub(crate) use self::bridge::*;
mod instruments;
use self::instruments::*;
mod capture;
use self::capture::*;

/// `FUTUREBOARD_FADER_DEBUG=1` — trace the gain [`apply_fader`] actually applies.
///
/// Shares its variable with the UI-side fader traces so one run covers the whole
/// chain: the pointer→norm mapping, the norm→linear conversion, the dispatch
/// into `update_track_param`, the `[DAUx] SetTrackVolume` drain, and this — the
/// measured pre/post ratio on the block that reaches the device. "The fader
/// moves and nothing gets quieter" is one of those five links, and reading the
/// code cannot tell you which.
/// Read this module's cached debug flag once, on a control thread (see
/// [`super::prime_audio_thread_resources`]).
pub(super) fn prime_debug_flags() {
    let _ = fader_debug_enabled();
}

fn fader_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_FADER_DEBUG").is_some())
}

use crate::runtime::{has_soloed_vsti_output_parent, solo_keeps_source_audible};

#[cfg(test)]
#[path = "control_room_tests.rs"]
mod control_room_tests;

#[cfg(test)]
#[path = "solfege_pitch_tests.rs"]
mod solfege_pitch_tests;

#[cfg(test)]
#[path = "parallel_render_tests.rs"]
mod parallel_render_tests;

#[cfg(test)]
mod jam_input_tests;

#[cfg(test)]
mod loopback_render_tests;

#[cfg(test)]
mod live_input_monitor_tests;

#[cfg(test)]
mod soundfont_instrument_tests;

#[cfg(test)]
mod bridge_bypass_tests;

#[cfg(test)]
mod fade_curve_tests;

#[cfg(test)]
mod warp_stretch_render_tests;

#[cfg(test)]
mod nonfinite_guard_tests;

#[cfg(test)]
mod spatial_mix_tests;

#[cfg(test)]
mod compensation_silence_tests;
