//! BurnLimit's editor telemetry block: what [`Dsp::telemetry`](crate::Dsp::telemetry)
//! fills, published through the host's 64-slot pad-level channel.
//!
//! Every slot is a plain `f32`. Levels are linear peaks (0…1+, the same
//! 400 ms fall as the meter frame); loudness is in LUFS / LU; a loudness or
//! true-peak slot with nothing measured yet reads [`NO_READING`].

pub use crate::loudness::NO_READING;

/// Slots in the block.
pub const SLOTS: usize = 64;

/// Input peak, left — linear, after the input gain.
pub const IN_L: usize = 0;
/// Input peak, right — linear, after the input gain.
pub const IN_R: usize = 1;
/// Output peak, left — linear, as heard.
pub const OUT_L: usize = 2;
/// Output peak, right — linear, as heard.
pub const OUT_R: usize = 3;
/// Gain reduction now, positive dB (0 while bypassed).
pub const GR_DB: usize = 4;
/// Momentary loudness (400 ms), LUFS.
pub const MOMENTARY_LUFS: usize = 5;
/// Short-term loudness (3 s), LUFS.
pub const SHORT_LUFS: usize = 6;
/// Gated integrated loudness since the last reset, LUFS.
pub const INTEGRATED_LUFS: usize = 7;
/// Loudness range (EBU Tech 3342), LU; 0 until 3 s have been measured.
pub const LRA_LU: usize = 8;
/// Held true-peak maximum of the output since the last reset, dBTP.
pub const TRUE_PEAK_DBTP: usize = 9;
/// Held gain-reduction maximum since the last reset, positive dB.
pub const GR_MAX_DB: usize = 10;
/// 1.0 once integrated loudness has a gated block, else 0.0.
pub const VALID: usize = 11;
/// The latency the insert reports, host samples.
pub const LATENCY_SAMPLES: usize = 12;
