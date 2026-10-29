//! Equz8 — 8-band parametric EQ.
//!
//! Phase 1 (easy). Filter coefficients and runtime state use the MIT/Apache
//! [`biquad`] crate. No DirectAudioEngine dependency.
//!
//! Cut bands (high/low pass) expand into a Butterworth cascade of up to
//! [`MAX_SECTIONS`] biquads for 24 and 48 dB/oct; a 12 dB/oct cut is the
//! single section it always was, at the band's own Q.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, flush_denormal, linear_to_db, make_eq_coefficients, max_filter_frequency, mix,
    time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.equz8";
pub const BAND_COUNT: usize = 8;

/// Biquad sections one band can expand into: 48 dB/oct is an 8th-order
/// Butterworth cascade, four second-order sections.
pub const MAX_SECTIONS: usize = 4;

/// Soft span (dB) from threshold to full dynamic amount. Pro-Q-style: the
/// envelope does not hard-switch at the threshold, it ramps over this window.
/// The law of a band whose `ratio` is [`RATIO_LEGACY`].
const DYNAMIC_SPAN_DB: f32 = 24.0;

/// `ratio` value meaning "the fixed-span law projects saved before the ratio
/// control existed were mixed with": the dynamic amount ramps linearly from
/// nothing at the threshold to the full range [`DYNAMIC_SPAN_DB`] past it.
/// A missing `ratio` field deserializes to this, so those projects sound as
/// they always did.
pub const RATIO_LEGACY: f32 = 0.0;

/// The ratio a band the editor creates starts at.
pub const DEFAULT_RATIO: f32 = 2.0;

/// Output peak meters fall back over about this long.
const PEAK_FALL_SECONDS: f32 = 0.4;

/// Which side of the threshold engages a dynamic band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DynMode {
    /// Engage as the band's level rises past the threshold — a de-esser.
    Above,
    /// Engage as it falls below — upward tone shaping.
    Below,
}

impl DynMode {
    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Above => 0.0,
            Self::Below => 1.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        if value.round() as i32 == 1 {
            Self::Below
        } else {
            Self::Above
        }
    }
}

/// How far toward its full range a dynamic band is pulled, 0..1, by a
/// detector reading `level_db` — the steady state the band's attack and
/// release glide toward.
///
/// With a ratio (1..10) the gain moves like a compressor's: `Above`, by
/// `(level − threshold)·(1 − 1/ratio)` dB; `Below`, by
/// `(threshold − level)·(ratio − 1)·0.5` dB; either way never past the range.
/// With [`RATIO_LEGACY`] it is the fixed linear ramp over
/// [`DYNAMIC_SPAN_DB`].
pub fn dynamic_fraction(band: &BandParams, level_db: f32) -> f32 {
    let over = match band.dyn_mode {
        DynMode::Above => level_db - band.threshold_db,
        DynMode::Below => band.threshold_db - level_db,
    };
    if over <= 0.0 {
        return 0.0;
    }
    if band.ratio < 1.0 {
        return clamp(over / DYNAMIC_SPAN_DB, 0.0, 1.0);
    }
    let range = band.range_db.abs();
    if range <= 1.0e-6 {
        return 0.0;
    }
    let moved = match band.dyn_mode {
        DynMode::Above => over * (1.0 - 1.0 / band.ratio),
        DynMode::Below => over * (band.ratio - 1.0) * 0.5,
    };
    clamp(moved / range, 0.0, 1.0)
}

/// The gain offset, in dB, a dynamic band settles at for a detector reading
/// `level_db` (signed like its range). 0 for a band that is not dynamic.
pub fn dynamic_offset_db(band: &BandParams, level_db: f32) -> f32 {
    if !(band.active && band.dynamic && band.band_type.has_gain()) {
        return 0.0;
    }
    dynamic_fraction(band, level_db) * band.range_db
}

/// The live readings the editor polls through the host's level block.
pub mod telemetry {
    use super::BAND_COUNT;

    /// Slots in one block.
    pub const SLOTS: usize = 64;
    /// Output peak, left and right, linear, falling over ~400 ms.
    pub const OUT_L: usize = 0;
    pub const OUT_R: usize = 1;
    /// `DETECTOR_BASE + band`: the band's detector level in dBFS; −120 for a
    /// band that is off or not dynamic.
    pub const DETECTOR_BASE: usize = 2;
    /// `GAIN_BASE + band`: the band's current dynamic gain offset in dB; 0
    /// for a band that is not dynamic.
    pub const GAIN_BASE: usize = 2 + BAND_COUNT;
    /// The reading of a detector that is not listening.
    pub const SILENT_DB: f32 = -120.0;
}

/// Rebuild the peaking/shelf filter when applied gain moves by at least this
/// much — avoids per-sample coefficient work on a quiet envelope.
const DYNAMIC_GAIN_EPS_DB: f32 = 0.05;

/// Output and mix glide over this long, so a dragged knob — which arrives as
/// one new value per block — does not step the level at every block edge.
const GAIN_SMOOTHING_MS: f32 = 15.0;

/// Power on/off swaps dry for processed over about this long.
const POWER_FADE_MS: f32 = 10.0;

/// The processed share `power` asks for.
fn power_share(power: bool) -> f32 {
    if power { 1.0 } else { 0.0 }
}

/// A control value that glides to a new target instead of stepping: two
/// one-poles in series, so it leaves and lands with zero slope, and neither a
/// dragged knob (a new target every block) nor a toggle puts a corner in the
/// signal it scales. Once within [`Glide::SNAP`] of the target it lands on it
/// exactly, so a settled glide costs one compare and "fully off" is an exact
/// state the audio path can skip on.
#[derive(Debug, Clone, Copy)]
struct Glide {
    first: Smoothed,
    second: Smoothed,
    step: f32,
}

impl Glide {
    /// −100 dB of a unity gain: a landing this size is inaudible.
    const SNAP: f32 = 1.0e-5;

    /// Settled at `value`, gliding over about `ms` at `sample_rate`.
    fn new(value: f32, ms: f32, sample_rate: f32) -> Self {
        let mut glide = Self {
            first: Smoothed::at(value),
            second: Smoothed::at(value),
            step: 0.0,
        };
        glide.set_time(ms, sample_rate);
        glide
    }

    /// Half of `ms` per pole.
    fn set_time(&mut self, ms: f32, sample_rate: f32) {
        self.step = smoothing_step(ms * 0.5, sample_rate);
    }

    fn set(&mut self, target: f32) {
        self.first.target = target;
    }

    fn is_settled(&self) -> bool {
        self.second.value == self.first.target && self.first.value == self.first.target
    }

    /// Lands on the target now: construction, reset, project restore.
    fn settle(&mut self) {
        self.first.settle();
        self.second = Smoothed::at(self.first.target);
    }

    #[inline]
    fn next(&mut self) -> f32 {
        let target = self.first.target;
        if self.is_settled() {
            return target;
        }
        self.second.target = self.first.next(self.step);
        let value = self.second.next(self.step);
        if (value - target).abs() < Self::SNAP && (self.first.value - target).abs() < Self::SNAP {
            self.settle();
            return target;
        }
        value
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandType {
    HighPass,
    LowShelf,
    Bell,
    Notch,
    HighShelf,
    LowPass,
}

impl BandType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HighPass => "highpass",
            Self::LowShelf => "lowshelf",
            Self::Bell => "bell",
            Self::Notch => "notch",
            Self::HighShelf => "highshelf",
            Self::LowPass => "lowpass",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "highpass" | "hp" => Some(Self::HighPass),
            "lowshelf" | "ls" => Some(Self::LowShelf),
            "bell" | "peak" | "peaking" => Some(Self::Bell),
            "notch" => Some(Self::Notch),
            "highshelf" | "hs" => Some(Self::HighShelf),
            "lowpass" | "lp" => Some(Self::LowPass),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::HighPass => 0.0,
            Self::LowShelf => 1.0,
            Self::Bell => 2.0,
            Self::Notch => 3.0,
            Self::HighShelf => 4.0,
            Self::LowPass => 5.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            0 => Self::HighPass,
            1 => Self::LowShelf,
            3 => Self::Notch,
            4 => Self::HighShelf,
            5 => Self::LowPass,
            _ => Self::Bell,
        }
    }

    /// Dynamic gain only applies to shapes that have a gain stage.
    pub const fn has_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Bell | Self::HighShelf)
    }

    /// Cut shapes take a slope and expand into a cascade.
    pub const fn is_cut(self) -> bool {
        matches!(self, Self::HighPass | Self::LowPass)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BandParams {
    pub active: bool,
    pub band_type: BandType,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
    /// When true (and the shape has gain), the band's applied gain moves from
    /// `gain_db` toward `gain_db + range_db` as the band-local detector exceeds
    /// `threshold_db` — FabFilter Pro-Q style dynamic EQ.
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f32,
    #[serde(default)]
    pub range_db: f32,
    #[serde(default = "default_attack_ms")]
    pub attack_ms: f32,
    #[serde(default = "default_release_ms")]
    pub release_ms: f32,
    /// dB/oct for the cut shapes — 12, 24 or 48; ignored by the others.
    /// `serde(default)` 12: the single section every cut was before slopes.
    #[serde(default = "legacy_slope")]
    pub slope: f32,
    /// Which side of the threshold engages the dynamics. Projects saved
    /// before the mode existed load as `Above`, the only mode there was.
    #[serde(default = "default_dyn_mode")]
    pub dyn_mode: DynMode,
    /// Dynamic ratio 1..10, or [`RATIO_LEGACY`] for the fixed-span law
    /// (what a missing field loads as).
    #[serde(default)]
    pub ratio: f32,
}

fn legacy_slope() -> f32 {
    12.0
}

fn default_dyn_mode() -> DynMode {
    DynMode::Above
}

fn default_threshold_db() -> f32 {
    -24.0
}

fn default_attack_ms() -> f32 {
    10.0
}

fn default_release_ms() -> f32 {
    100.0
}

fn flat_band(band_type: BandType, freq: f32, q: f32) -> BandParams {
    BandParams {
        active: false,
        band_type,
        freq,
        gain_db: 0.0,
        q,
        dynamic: false,
        threshold_db: default_threshold_db(),
        range_db: 0.0,
        attack_ms: default_attack_ms(),
        release_ms: default_release_ms(),
        slope: legacy_slope(),
        dyn_mode: default_dyn_mode(),
        ratio: DEFAULT_RATIO,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub output_db: f32,
    pub mix: f32,
    pub bands: [BandParams; BAND_COUNT],
    /// Index of the band being auditioned in isolation, or [`ipc::SOLO_NONE`].
    ///
    /// `serde(default)` so projects saved before band solo existed still load —
    /// they deserialize with solo off, which is the pre-existing behavior.
    #[serde(default = "solo_none")]
    pub solo_band: i32,
}

fn solo_none() -> i32 {
    ipc::SOLO_NONE
}

/// Neutral insert state: every band off, gain flat. Types/freqs stay as a
/// useful starting layout when the user enables a slot — no curve on open.
pub fn default_params() -> Params {
    Params {
        power: true,
        output_db: 0.0,
        mix: 100.0,
        bands: [
            flat_band(BandType::HighPass, 50.0, 0.7),
            flat_band(BandType::LowShelf, 120.0, 0.8),
            flat_band(BandType::Bell, 250.0, 1.2),
            flat_band(BandType::Bell, 750.0, 1.4),
            flat_band(BandType::Bell, 1_500.0, 1.0),
            flat_band(BandType::Bell, 3_500.0, 1.1),
            flat_band(BandType::HighShelf, 8_000.0, 0.8),
            flat_band(BandType::LowPass, 16_000.0, 0.7),
        ],
        solo_band: ipc::SOLO_NONE,
    }
}

/// What `band` does to a sine at `freq_hz`, in dB, with its gain at
/// `gain_db` — built from the coefficients [`Dsp`] runs, so a drawn curve is
/// the curve that plays. 0 dB for a band that is switched off. Control/UI
/// side only.
pub fn band_response_at_gain_db(
    band: &BandParams,
    gain_db: f32,
    freq_hz: f32,
    sample_rate: f32,
) -> f32 {
    if !band.active {
        return 0.0;
    }
    (0..section_count(band))
        .map(|section| {
            section_coefficients(band, section, gain_db, sample_rate).map_or(0.0, |coefficients| {
                biquad_response_db(&coefficients, freq_hz, sample_rate)
            })
        })
        .sum()
}

/// Biquad coefficients for one EQ shape: the shared
/// [`make_eq_coefficients`], with the Q ceiling raised to [`ipc::Q_MAX`].
/// Below the shared builder's own ceiling of 12 it *is* the shared builder,
/// so bands saved before the range widened design the same filters.
fn design(
    kind: &str,
    freq_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate: f32,
) -> Option<Coefficients<f32>> {
    if q <= 12.0 {
        return make_eq_coefficients(kind, freq_hz, gain_db, q, sample_rate);
    }
    let fs = sample_rate.max(1.0);
    let f0 = clamp(freq_hz, 10.0, max_filter_frequency(fs));
    let filter_type = match kind {
        "bell" => Type::PeakingEQ(gain_db),
        "lowshelf" => Type::LowShelf(gain_db),
        "highshelf" => Type::HighShelf(gain_db),
        "lowpass" => Type::LowPass,
        "highpass" => Type::HighPass,
        "notch" => Type::Notch,
        "bandpass" => Type::BandPass,
        _ => return None,
    };
    Coefficients::<f32>::from_params(filter_type, fs.hz(), f0.hz(), q.min(ipc::Q_MAX)).ok()
}

/// Butterworth section Qs for a cascade of the given (even) filter order:
/// the k-th of `order/2` sections at `1 / (2 cos((2k+1)π / 2·order))`.
fn butterworth_q(order: usize, k: usize) -> f32 {
    let angle = (2.0 * k as f32 + 1.0) * std::f32::consts::PI / (2.0 * order as f32);
    1.0 / (2.0 * angle.cos())
}

/// How many biquad sections a band expands into: one, except a cut at 24
/// (two) or 48 dB/oct (four).
pub fn section_count(band: &BandParams) -> usize {
    if !band.band_type.is_cut() {
        return 1;
    }
    let order = (band.slope / 6.0).round().max(2.0) as usize;
    (order / 2).clamp(1, MAX_SECTIONS)
}

/// The coefficients of section `section` of `band` at gain `gain_db`. A
/// 12 dB/oct cut keeps the band's own Q, exactly as before slopes; a steeper
/// one is a Butterworth cascade.
fn section_coefficients(
    band: &BandParams,
    section: usize,
    gain_db: f32,
    sample_rate: f32,
) -> Option<Coefficients<f32>> {
    let count = section_count(band);
    let q = if band.band_type.is_cut() && count > 1 {
        butterworth_q(count * 2, section)
    } else {
        band.q
    };
    design(band.band_type.as_str(), band.freq, gain_db, q, sample_rate)
}

/// [`band_response_at_gain_db`] at the band's own gain.
pub fn band_response_db(band: &BandParams, freq_hz: f32, sample_rate: f32) -> f32 {
    band_response_at_gain_db(band, band.gain_db, freq_hz, sample_rate)
}

/// The whole EQ's wet response at `freq_hz`, in dB: every band in series,
/// then the output gain. What the insert plays at full mix with no band
/// moving dynamically.
pub fn response_db(params: &Params, freq_hz: f32, sample_rate: f32) -> f32 {
    params
        .bands
        .iter()
        .map(|band| band_response_db(band, freq_hz, sample_rate))
        .sum::<f32>()
        + params.output_db
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Equz8",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: &[
            ParamDescriptor {
                id: "power",
                name: "Power",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "mix",
                name: "Mix",
                default_value: 100.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "outputDb",
                name: "Output",
                default_value: 0.0,
                min: ipc::OUTPUT_MIN_DB,
                max: ipc::OUTPUT_MAX_DB,
                unit: "dB",
            },
        ],
    }
}

#[derive(Debug, Clone, Copy)]
struct DynamicBandState {
    envelope: f32,
    attack_coeff: f32,
    release_coeff: f32,
    last_applied_db: f32,
    /// The detector's last reading and the offset it produced, for telemetry.
    level_db: f32,
    offset_db: f32,
}

impl DynamicBandState {
    fn new() -> Self {
        Self {
            envelope: 0.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            last_applied_db: 0.0,
            level_db: telemetry::SILENT_DB,
            offset_db: 0.0,
        }
    }
}

type Cascade = [Option<DirectForm1<f32>>; MAX_SECTIONS];

const EMPTY_CASCADE: Cascade = [None; MAX_SECTIONS];

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    left: [Cascade; BAND_COUNT],
    right: [Cascade; BAND_COUNT],
    /// Sections each band actually runs.
    sections: [u8; BAND_COUNT],
    /// Output peaks, left and right, and their per-sample fall.
    out_peak: [f32; 2],
    peak_fall: f32,
    /// Band-local detector for dynamic EQ (bandpass around the band centre).
    detector: [Option<DirectForm1<f32>>; BAND_COUNT],
    dyn_state: [DynamicBandState; BAND_COUNT],
    /// Audition bandpass for the soloed band, per channel. `None` when no band
    /// is soloed — built on the control thread so the audio path only runs it.
    solo_left: Option<DirectForm1<f32>>,
    solo_right: Option<DirectForm1<f32>>,
    /// Linear output gain and wet share (0..1), smoothed per sample.
    output_gain: Glide,
    mix: Glide,
    /// Dry → processed share, following `params.power`.
    power: Glide,
    /// Fast-path flag: skip detector/envelope work when nothing needs it.
    any_dynamic: bool,
}

/// Q used to audition band shapes that have no meaningful centre width.
///
/// Shelves and pass filters are broad by construction, so reusing their own Q
/// would open an audition window either far too wide to isolate anything or so
/// narrow it sits on a slope. A moderate fixed Q gives a consistent "what lives
/// around this corner frequency" listen.
const SOLO_WIDE_Q: f32 = 0.9;

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let mut dsp = Self {
            sample_rate: sample_rate.max(1.0),
            params: default_params(),
            left: [EMPTY_CASCADE; BAND_COUNT],
            right: [EMPTY_CASCADE; BAND_COUNT],
            sections: [0; BAND_COUNT],
            out_peak: [0.0; 2],
            peak_fall: time_constant(sample_rate.max(1.0), PEAK_FALL_SECONDS),
            detector: [None, None, None, None, None, None, None, None],
            dyn_state: [DynamicBandState::new(); BAND_COUNT],
            solo_left: None,
            solo_right: None,
            output_gain: Glide::new(1.0, GAIN_SMOOTHING_MS, sample_rate.max(1.0)),
            mix: Glide::new(1.0, GAIN_SMOOTHING_MS, sample_rate.max(1.0)),
            power: Glide::new(1.0, POWER_FADE_MS, sample_rate.max(1.0)),
            any_dynamic: false,
        };
        dsp.rebuild();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter (project restore). Output, mix and power land
    /// on the restored values rather than gliding in from the old ones.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.rebuild();
        self.settle();
    }

    /// Put every smoother on its target.
    fn settle(&mut self) {
        self.output_gain.settle();
        self.mix.settle();
        self.power.settle();
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    ///
    /// The audio path never parses JSON or looks up string parameter ids.
    /// A future host bridge can drain its bounded parameter ring between
    /// blocks and call this method directly.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }

        match wire_index {
            ipc::POWER_INDEX => self.power.set(power_share(self.params.power)),
            ipc::MIX_INDEX => self.mix.set(self.params.mix / 100.0),
            ipc::OUTPUT_INDEX => self.output_gain.set(db_to_linear(self.params.output_db)),
            ipc::SOLO_INDEX => self.rebuild_solo(),
            _ => {
                if let Some((band, _)) = ipc::decode_band_wire(wire_index) {
                    self.rebuild_band(band);
                    if self.params.solo_band == band as i32 {
                        self.rebuild_solo();
                    }
                } else if let Some((band, field)) = ipc::decode_dyn_wire(wire_index) {
                    match field {
                        ipc::BAND_DYN_ENABLED => {
                            self.rebuild_band(band);
                            self.refresh_any_dynamic();
                        }
                        ipc::BAND_DYN_ATTACK | ipc::BAND_DYN_RELEASE => {
                            self.refresh_dyn_timing(band);
                        }
                        ipc::BAND_DYN_THRESHOLD | ipc::BAND_DYN_RANGE => {
                            // Envelope curve only — applied gain updates sample-by-sample.
                        }
                        _ => {}
                    }
                } else if let Some((band, field)) = ipc::decode_ext_wire(wire_index) {
                    // Slope reshapes the cascade; mode and ratio only the
                    // envelope law, which runs sample by sample.
                    if field == ipc::BAND_SLOPE {
                        self.rebuild_band(band);
                        if self.params.solo_band == band as i32 {
                            self.rebuild_solo();
                        }
                    }
                }
            }
        }
        true
    }

    /// The editor's live readings ([`telemetry`]). Realtime-safe: a copy of
    /// state the audio path keeps anyway.
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut block = [0.0; telemetry::SLOTS];
        block[telemetry::OUT_L] = self.out_peak[0];
        block[telemetry::OUT_R] = self.out_peak[1];
        for (index, band) in self.params.bands.iter().enumerate() {
            let listening = self.params.power
                && band.active
                && band.dynamic
                && band.band_type.has_gain()
                && band.range_db.abs() > 1.0e-6;
            let state = &self.dyn_state[index];
            block[telemetry::DETECTOR_BASE + index] = if listening {
                state.level_db
            } else {
                telemetry::SILENT_DB
            };
            block[telemetry::GAIN_BASE + index] = if listening { state.offset_db } else { 0.0 };
        }
        block
    }

    #[inline]
    fn track_peaks(&mut self, left: f32, right: f32) {
        let fall = self.peak_fall;
        let [l, r] = &mut self.out_peak;
        *l = flush_denormal(left.abs().max(*l * fall));
        *r = flush_denormal(right.abs().max(*r * fall));
    }

    fn rebuild(&mut self) {
        self.peak_fall = time_constant(self.sample_rate, PEAK_FALL_SECONDS);
        self.output_gain
            .set_time(GAIN_SMOOTHING_MS, self.sample_rate);
        self.mix.set_time(GAIN_SMOOTHING_MS, self.sample_rate);
        self.power.set_time(POWER_FADE_MS, self.sample_rate);
        self.output_gain.set(db_to_linear(self.params.output_db));
        self.mix.set(self.params.mix / 100.0);
        self.power.set(power_share(self.params.power));
        for i in 0..BAND_COUNT {
            self.rebuild_band(i);
        }
        self.rebuild_solo();
        self.refresh_any_dynamic();
    }

    fn refresh_any_dynamic(&mut self) {
        self.any_dynamic = self.params.bands.iter().any(|band| {
            band.active && band.dynamic && band.band_type.has_gain() && band.range_db.abs() > 1.0e-6
        });
    }

    fn refresh_dyn_timing(&mut self, index: usize) {
        let band = self.params.bands[index];
        let state = &mut self.dyn_state[index];
        state.attack_coeff = time_constant(self.sample_rate, band.attack_ms.max(0.1) * 0.001);
        state.release_coeff = time_constant(self.sample_rate, band.release_ms.max(1.0) * 0.001);
    }

    /// Build the audition bandpass for the soloed band, if any.
    fn rebuild_solo(&mut self) {
        let index = self.params.solo_band;
        let filter = if index >= 0 && (index as usize) < BAND_COUNT {
            let band = self.params.bands[index as usize];
            let q = match band.band_type {
                BandType::Bell | BandType::Notch => band.q,
                _ => SOLO_WIDE_Q,
            };
            design("bandpass", band.freq, 0.0, q, self.sample_rate).map(DirectForm1::<f32>::new)
        } else {
            None
        };
        self.solo_left = filter;
        self.solo_right = filter;
    }

    fn rebuild_band(&mut self, index: usize) {
        let band = self.params.bands[index];
        self.refresh_dyn_timing(index);
        if !band.active {
            self.left[index] = EMPTY_CASCADE;
            self.right[index] = EMPTY_CASCADE;
            self.sections[index] = 0;
            self.detector[index] = None;
            return;
        }
        let applied = band.gain_db;
        let count = section_count(&band);
        self.sections[index] = count as u8;
        for section in 0..MAX_SECTIONS {
            let filter = if section < count {
                section_coefficients(&band, section, applied, self.sample_rate)
                    .map(DirectForm1::<f32>::new)
            } else {
                None
            };
            self.left[index][section] = filter;
            self.right[index][section] = filter;
        }
        self.dyn_state[index].last_applied_db = applied;

        let wants_detector = band.dynamic && band.band_type.has_gain();
        self.detector[index] = if wants_detector {
            let q = match band.band_type {
                BandType::Bell => band.q,
                _ => SOLO_WIDE_Q,
            };
            design("bandpass", band.freq, 0.0, q, self.sample_rate).map(DirectForm1::<f32>::new)
        } else {
            None
        };
    }

    /// Retunes a dynamic band to a new applied gain. Only gain-bearing shapes
    /// reach this, and those are always one section.
    fn set_band_applied_gain(&mut self, index: usize, applied_db: f32) {
        let band = self.params.bands[index];
        let Some(coeffs) = design(
            band.band_type.as_str(),
            band.freq,
            applied_db,
            band.q,
            self.sample_rate,
        ) else {
            return;
        };
        // Retune in place so filter history survives — installing a fresh
        // DirectForm1 would zero the delay line and click on every gain step.
        if let Some(filter) = self.left[index][0].as_mut() {
            filter.update_coefficients(coeffs);
        }
        if let Some(filter) = self.right[index][0].as_mut() {
            filter.update_coefficients(coeffs);
        }
        self.dyn_state[index].last_applied_db = applied_db;
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        for cascade in self.left.iter_mut().chain(self.right.iter_mut()) {
            for filter in cascade.iter_mut().flatten() {
                filter.reset_state();
            }
        }
        for filter in self.detector.iter_mut().flatten() {
            filter.reset_state();
        }
        for state in &mut self.dyn_state {
            state.envelope = 0.0;
            state.level_db = telemetry::SILENT_DB;
            state.offset_db = 0.0;
        }
        self.out_peak = [0.0; 2];
        for filter in self.solo_left.iter_mut().chain(self.solo_right.iter_mut()) {
            filter.reset_state();
        }
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rebuild();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        // Fully off and settled: the chain is not run at all, as before.
        if self.power.is_settled() && !self.params.power {
            self.track_peaks(left, right);
            return (left, right);
        }
        let on = self.power.next();
        let gain = self.output_gain.next();
        let amount = self.mix.next();
        let (wet_l, wet_r) = self.process_on(left, right, gain, amount);
        let out = if on == 1.0 {
            (wet_l, wet_r)
        } else {
            // A step from dry, so a chain that leaves the signal untouched
            // stays bit-exact through the fade.
            (left + (wet_l - left) * on, right + (wet_r - right) * on)
        };
        self.track_peaks(out.0, out.1);
        out
    }
}

impl Dsp {
    /// The powered chain for one sample, at output `gain` and wet share
    /// `amount` (both already smoothed).
    #[inline]
    fn process_on(&mut self, left: f32, right: f32, gain: f32, amount: f32) -> (f32, f32) {
        // Band solo replaces the chain rather than adding to it, and
        // deliberately ignores Mix: auditioning a band means hearing that
        // frequency region alone, so blending the dry signal back in would
        // defeat the point. Output level still applies so the audition can be
        // matched in loudness.
        if let (Some(solo_l), Some(solo_r)) = (self.solo_left.as_mut(), self.solo_right.as_mut()) {
            return (solo_l.run(left) * gain, solo_r.run(right) * gain);
        }

        if self.any_dynamic {
            // Dynamic path: per band, update detector → envelope → applied gain
            // then run the EQ stage. Bands without dynamic still use their
            // prebuilt static filters.
            let mut wet_l = left;
            let mut wet_r = right;
            for index in 0..BAND_COUNT {
                let band = self.params.bands[index];
                if !band.active {
                    continue;
                }
                if band.dynamic && band.band_type.has_gain() && band.range_db.abs() > 1.0e-6 {
                    let detector_in = left.abs().max(right.abs());
                    let detected = match self.detector[index].as_mut() {
                        Some(filter) => filter.run(detector_in).abs(),
                        None => detector_in,
                    };
                    let (attack, release, last) = {
                        let state = &self.dyn_state[index];
                        (
                            state.attack_coeff,
                            state.release_coeff,
                            state.last_applied_db,
                        )
                    };
                    let envelope = {
                        let state = &mut self.dyn_state[index];
                        let coeff = if detected > state.envelope {
                            attack
                        } else {
                            release
                        };
                        state.envelope =
                            flush_denormal(coeff * state.envelope + (1.0 - coeff) * detected);
                        state.envelope
                    };
                    let env_db = linear_to_db(envelope.max(1.0e-12));
                    let offset = dynamic_fraction(&band, env_db) * band.range_db;
                    let state = &mut self.dyn_state[index];
                    state.level_db = env_db;
                    state.offset_db = offset;
                    let applied = band.gain_db + offset;
                    if (applied - last).abs() >= DYNAMIC_GAIN_EPS_DB {
                        self.set_band_applied_gain(index, applied);
                    }
                }
                self.run_band(index, &mut wet_l, &mut wet_r);
            }
            wet_l *= gain;
            wet_r *= gain;
            return (mix(left, wet_l, amount), mix(right, wet_r, amount));
        }

        let mut wet_l = left;
        let mut wet_r = right;
        for index in 0..BAND_COUNT {
            self.run_band(index, &mut wet_l, &mut wet_r);
        }
        wet_l *= gain;
        wet_r *= gain;
        (mix(left, wet_l, amount), mix(right, wet_r, amount))
    }

    /// One band's cascade over both channels; nothing for an empty slot.
    #[inline]
    fn run_band(&mut self, index: usize, left: &mut f32, right: &mut f32) {
        for section in 0..self.sections[index] as usize {
            if let Some(filter) = self.left[index][section].as_mut() {
                *left = filter.run(*left);
            }
            if let Some(filter) = self.right[index][section].as_mut() {
                *right = filter.run(*right);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_id() {
        assert_eq!(descriptor().id, PLUGIN_ID);
    }

    #[test]
    fn default_is_flat_inactive() {
        let params = default_params();
        assert!(params.bands.iter().all(|band| !band.active));
        assert!(params.bands.iter().all(|band| band.gain_db == 0.0));
        assert!(params.bands.iter().all(|band| !band.dynamic));
        assert!(params.bands.iter().all(|band| band.range_db == 0.0));
    }

    #[test]
    fn bypass_when_power_off() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.power = false;
        dsp.set_params(params);
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }

    #[test]
    fn processes_without_nan() {
        let mut dsp = Dsp::new(48_000.0);
        let (l, r) = dsp.process_stereo(0.5, -0.5);
        assert!(l.is_finite() && r.is_finite());
    }

    /// Measure output level at one frequency by running a sine through the DSP.
    fn level_at(dsp: &mut Dsp, freq_hz: f32) -> f32 {
        dsp.reset();
        let sr = 48_000.0f32;
        let step = std::f32::consts::TAU * freq_hz / sr;
        // Settle the filter state before measuring.
        for i in 0..4_000 {
            let _ = dsp.process_stereo((i as f32 * step).sin(), 0.0);
        }
        let mut peak = 0.0f32;
        for i in 4_000..8_000 {
            let (l, _) = dsp.process_stereo((i as f32 * step).sin(), 0.0);
            peak = peak.max(l.abs());
        }
        peak
    }

    #[test]
    fn solo_isolates_the_band_frequency_region() {
        let mut params = default_params();
        // A bell at 1 kHz, soloed. Everything far from it must drop away.
        params.bands[4].active = true;
        params.bands[4].band_type = BandType::Bell;
        params.bands[4].freq = 1_000.0;
        params.bands[4].q = 2.0;
        params.solo_band = 4;

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let in_band = level_at(&mut dsp, 1_000.0);
        let below = level_at(&mut dsp, 100.0);
        let above = level_at(&mut dsp, 10_000.0);

        assert!(
            in_band > 0.5,
            "the soloed region must pass through, got {in_band}"
        );
        assert!(
            below < in_band * 0.2,
            "content an octave-plus below must be rejected: {below} vs {in_band}"
        );
        assert!(
            above < in_band * 0.2,
            "content well above must be rejected: {above} vs {in_band}"
        );
    }

    #[test]
    fn solo_ignores_mix() {
        let mut params = default_params();
        params.bands[4].active = true;
        params.bands[4].band_type = BandType::Bell;
        params.bands[4].freq = 1_000.0;
        params.bands[4].q = 2.0;
        params.solo_band = 4;
        params.mix = 0.0;

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let below = level_at(&mut dsp, 100.0);
        assert!(
            below < 0.15,
            "solo must still reject out-of-band content at mix=0, got {below}"
        );
    }

    #[test]
    fn clearing_solo_restores_the_eq_chain() {
        let mut params = default_params();
        params.solo_band = 2;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        assert!(dsp.apply_wire_param(ipc::SOLO_INDEX, ipc::SOLO_NONE as f32));
        assert_eq!(dsp.params().solo_band, ipc::SOLO_NONE);
        let (l, r) = dsp.process_stereo(0.1, -0.1);
        assert!(l.is_finite() && r.is_finite());
    }

    /// Dragging the soloed band has to retune what you hear, or the audition
    /// window stays where the band used to be.
    #[test]
    fn editing_the_soloed_band_retunes_the_audition() {
        let mut params = default_params();
        params.bands[4].active = true;
        params.bands[4].band_type = BandType::Bell;
        params.bands[4].freq = 1_000.0;
        params.bands[4].q = 2.0;
        params.solo_band = 4;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let at_1k = level_at(&mut dsp, 1_000.0);
        assert!(dsp.apply_wire_param(ipc::band_wire_index(4, ipc::BAND_FREQ), 5_000.0));
        let at_5k = level_at(&mut dsp, 5_000.0);
        assert!(
            at_5k > at_1k * 0.5,
            "moving the soloed band to 5 kHz must move the audition with it"
        );
    }

    #[test]
    fn wire_gain_updates_params() {
        let mut dsp = Dsp::new(48_000.0);
        assert_eq!(dsp.params().solo_band, ipc::SOLO_NONE);
        assert!(dsp.apply_wire_param(ipc::band_wire_index(2, ipc::BAND_GAIN), 6.0));
        assert_eq!(dsp.params().bands[2].gain_db, 6.0);
    }

    #[test]
    fn dynamic_cut_reduces_hot_in_band_signal() {
        let mut params = default_params();
        params.bands[4].active = true;
        params.bands[4].band_type = BandType::Bell;
        params.bands[4].freq = 1_000.0;
        params.bands[4].gain_db = 0.0;
        params.bands[4].q = 2.0;
        params.bands[4].dynamic = true;
        params.bands[4].threshold_db = -40.0;
        params.bands[4].range_db = -12.0;
        params.bands[4].attack_ms = 0.1;
        params.bands[4].release_ms = 50.0;

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        // Hot sine at the band centre should be pulled down by the dynamic cut.
        let hot = level_at(&mut dsp, 1_000.0);
        assert!(
            hot < 0.85,
            "dynamic cut must reduce a hot in-band tone, got {hot}"
        );

        // Far from the band, the detector stays quiet so the cut should not apply.
        let far = level_at(&mut dsp, 80.0);
        assert!(
            far > hot,
            "out-of-band content must stay closer to unity than the cut band: far={far} hot={hot}"
        );
    }

    /// The curve the editor draws is the curve that plays: a sine through the
    /// running DSP lands where `response_db` says, at every band type.
    #[test]
    fn the_drawn_response_is_what_plays() {
        let mut params = default_params();
        for band in &mut params.bands {
            band.active = true;
        }
        params.bands[2].gain_db = 5.0;
        params.bands[4].gain_db = -7.0;
        params.bands[1].gain_db = 3.0;
        params.bands[6].gain_db = -4.0;
        params.output_db = -2.0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        // Off the sample rate's divisors, so the sampled peak sweeps the
        // whole cycle rather than landing on the same few phases.
        for hz in [41.0, 123.0, 257.0, 1_517.0, 4_111.0, 11_003.0] {
            let heard = linear_to_db(level_at(&mut dsp, hz));
            let drawn = response_db(&params, hz, 48_000.0);
            assert!(
                (heard - drawn).abs() < 0.25,
                "{hz} Hz: heard {heard} drawn {drawn}"
            );
        }
        // A switched-off band draws flat.
        assert_eq!(
            band_response_db(&default_params().bands[3], 750.0, 48_000.0),
            0.0
        );
    }

    #[test]
    fn legacy_state_without_dynamic_fields_loads() {
        let json = r#"{"version":1,"params":{"power":true,"outputDb":0.0,"mix":100.0,"bands":[{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0},{"active":false,"bandType":"bell","freq":1000.0,"gainDb":0.0,"q":1.0}]}}"#;
        let state = ipc::Equz8State::from_json(json).expect("legacy json");
        assert!(!state.params.bands[0].dynamic);
        assert_eq!(state.params.bands[0].threshold_db, -24.0);
        assert_eq!(state.params.bands[0].range_db, 0.0);
    }

    /// A project saved before slopes, modes and ratios plays exactly as it
    /// did: the new fields load as their legacy values, and a static chain
    /// is bit-identical to one built straight from the shared builder.
    #[test]
    fn legacy_state_plays_exactly_as_before() {
        let band = |kind: &str, freq: f32, gain: f32, q: f32| {
            format!(
                r#"{{"active":true,"bandType":"{kind}","freq":{freq},"gainDb":{gain},"q":{q}}}"#
            )
        };
        let bands = [
            band("highpass", 80.0, 0.0, 0.9),
            band("lowshelf", 120.0, 3.0, 0.8),
            band("bell", 900.0, -4.0, 2.0),
            band("bell", 2500.0, 2.5, 1.0),
            band("notch", 4000.0, 0.0, 6.0),
            band("highshelf", 9000.0, -2.0, 0.7),
            band("lowpass", 15000.0, 0.0, 0.7),
            band("bell", 300.0, 1.0, 1.4),
        ];
        let json = format!(
            r#"{{"version":1,"params":{{"power":true,"outputDb":-1.5,"mix":100.0,"bands":[{}]}}}}"#,
            bands.join(",")
        );
        let state = ipc::Equz8State::from_json(&json).expect("legacy json");
        for band in &state.params.bands {
            assert_eq!(band.slope, 12.0);
            assert_eq!(band.dyn_mode, DynMode::Above);
            assert_eq!(band.ratio, RATIO_LEGACY);
        }
        let mut reference: Vec<DirectForm1<f32>> = state
            .params
            .bands
            .iter()
            .map(|band| {
                builtin_dsp_core::make_eq_biquad(
                    band.band_type.as_str(),
                    band.freq,
                    band.gain_db,
                    band.q,
                    48_000.0,
                )
                .expect("filter")
            })
            .collect();
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(state.params);
        let gain = db_to_linear(-1.5);
        for n in 0..4_000 {
            let x = ((n as f32) * 0.037).sin() * 0.5 + ((n as f32) * 0.31).sin() * 0.2;
            let mut expected = x;
            for filter in &mut reference {
                expected = filter.run(expected);
            }
            let (l, _) = dsp.process_stereo(x, 0.0);
            assert_eq!(l, expected * gain, "sample {n}");
        }
    }

    /// The legacy law is the fixed 24 dB ramp; a ratio replaces it with a
    /// compressor's.
    #[test]
    fn the_ratio_law_replaces_the_legacy_ramp() {
        let mut band = default_params().bands[2];
        band.active = true;
        band.dynamic = true;
        band.threshold_db = -30.0;
        band.range_db = -12.0;
        band.ratio = RATIO_LEGACY;
        assert!((dynamic_fraction(&band, -18.0) - 0.5).abs() < 1.0e-6);
        band.ratio = 4.0;
        // 8 dB over at 4:1 moves 6 dB: half of a 12 dB range.
        assert!((dynamic_offset_db(&band, -22.0) + 6.0).abs() < 1.0e-5);
        // Never past the range.
        assert!((dynamic_offset_db(&band, 0.0) + 12.0).abs() < 1.0e-5);
        assert_eq!(dynamic_offset_db(&band, -40.0), 0.0);
        band.dyn_mode = DynMode::Below;
        // 4 dB under at 4:1 moves 4·3·0.5 = 6 dB.
        assert!((dynamic_offset_db(&band, -34.0) + 6.0).abs() < 1.0e-5);
        assert_eq!(dynamic_offset_db(&band, -20.0), 0.0);
    }

    #[test]
    fn cut_slopes_steepen_the_cascade() {
        let mut band = default_params().bands[0];
        band.active = true;
        band.freq = 1_000.0;
        band.q = std::f32::consts::FRAC_1_SQRT_2;
        let at = |band: &BandParams, hz: f32| band_response_db(band, hz, 48_000.0);
        // 12 dB/oct: one section at the band's own Q, −3 dB at the corner.
        assert_eq!(section_count(&band), 1);
        assert!((at(&band, 1_000.0) + 3.0).abs() < 0.1);
        band.slope = 24.0;
        assert_eq!(section_count(&band), 2);
        assert!((at(&band, 1_000.0) + 3.0).abs() < 0.1);
        assert!(
            (at(&band, 250.0) + 48.0).abs() < 2.0,
            "{}",
            at(&band, 250.0)
        );
        band.slope = 48.0;
        assert_eq!(section_count(&band), MAX_SECTIONS);
        assert!(
            (at(&band, 500.0) + 48.0).abs() < 2.0,
            "{}",
            at(&band, 500.0)
        );

        // And what plays is what is drawn.
        let mut params = default_params();
        params.bands[0] = band;
        params.bands[7].active = true;
        params.bands[7].slope = 24.0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        for hz in [613.0, 1_013.0, 3_011.0, 15_013.0] {
            let heard = linear_to_db(level_at(&mut dsp, hz));
            let drawn = response_db(&params, hz, 48_000.0);
            assert!((heard - drawn).abs() < 0.3, "{hz}: {heard} vs {drawn}");
        }
    }

    #[test]
    fn a_wide_q_designs_past_the_shared_ceiling() {
        let mut band = default_params().bands[3];
        band.active = true;
        band.gain_db = 12.0;
        band.q = ipc::Q_MAX;
        // Half a semitone off the centre a Q-18 bell is well down from 12 dB;
        // the shared builder's Q-12 one would still be higher.
        let narrow = band_response_db(&band, 750.0 * 1.03, 48_000.0);
        band.q = 12.0;
        let capped = band_response_db(&band, 750.0 * 1.03, 48_000.0);
        assert!(narrow < capped - 0.5, "{narrow} vs {capped}");
    }

    #[test]
    fn telemetry_reports_peaks_and_dynamics() {
        let mut params = default_params();
        params.bands[4] = BandParams {
            active: true,
            band_type: BandType::Bell,
            freq: 1_000.0,
            q: 2.0,
            dynamic: true,
            threshold_db: -40.0,
            range_db: -12.0,
            attack_ms: 0.1,
            ratio: 4.0,
            ..params.bands[4]
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let _ = level_at(&mut dsp, 1_000.0);
        let block = dsp.telemetry();
        assert!(block[telemetry::OUT_L] > 0.1, "{}", block[telemetry::OUT_L]);
        assert_eq!(block[telemetry::OUT_R], 0.0, "silent right");
        assert!(block[telemetry::DETECTOR_BASE + 4] > -40.0);
        assert!(block[telemetry::GAIN_BASE + 4] < -1.0);
        assert_eq!(block[telemetry::DETECTOR_BASE], telemetry::SILENT_DB);
        assert_eq!(block[telemetry::GAIN_BASE], 0.0);
    }

    #[test]
    fn mix_zero_is_dry() {
        let mut dsp = shaping();
        assert!(dsp.apply_wire_param(ipc::MIX_INDEX, 0.0));
        for _ in 0..4_800 {
            let _ = dsp.process_stereo(0.1, 0.1);
        }
        let (l, r) = dsp.process_stereo(0.3, -0.2);
        assert!((l - 0.3).abs() < 1.0e-6 && (r + 0.2).abs() < 1.0e-6);
    }

    const BLOCK: usize = 128;
    /// Blocks played before measuring, so filters and smoothers are at rest.
    const WARM: usize = 50;

    /// The measure of LiveStageEngine's `param_sweep`: a smooth two-tone
    /// signal goes through `block` one 128-frame block at a time, and this is
    /// the worst ratio of the kink (second difference) where a block meets
    /// the last one to the biggest kink inside the block. About 1 is smooth;
    /// a value that steps once per block reads many times that.
    fn edge_kink(blocks: usize, mut block: impl FnMut(usize, &mut [f32], &mut [f32])) -> f32 {
        let mut left = [0.0f32; BLOCK];
        let mut right = [0.0f32; BLOCK];
        let mut n = 0usize;
        let mut last = [(0.0f32, 0.0f32); 2];
        let mut worst = 0.0f32;
        for index in 0..WARM + blocks {
            for i in 0..BLOCK {
                let t = n as f64 / 48_000.0;
                let tone = (std::f64::consts::TAU * 110.0 * t).sin() * 0.3
                    + (std::f64::consts::TAU * 330.0 * t).sin() * 0.1;
                (left[i], right[i]) = (tone as f32, (tone * 0.8) as f32);
                n += 1;
            }
            block(index.saturating_sub(WARM), &mut left, &mut right);
            let mut edge = 0.0f32;
            let mut inside = 1.0e-7f32;
            for i in 0..BLOCK {
                let d2 = |a: f32, b: f32, c: f32| (c - 2.0 * b + a).abs();
                let kink =
                    d2(last[0].0, last[1].0, left[i]).max(d2(last[0].1, last[1].1, right[i]));
                if i < 2 {
                    edge = edge.max(kink);
                } else {
                    inside = inside.max(kink);
                }
                last = [last[1], (left[i], right[i])];
            }
            if index >= WARM {
                worst = worst.max(edge / inside);
            }
        }
        worst
    }

    /// Plays `dsp`, calling `change(dsp, block)` before every measured block.
    fn drag(dsp: &mut Dsp, blocks: usize, mut change: impl FnMut(&mut Dsp, usize)) -> f32 {
        let mut warm = WARM;
        edge_kink(blocks, |index, left, right| {
            if warm > 0 {
                warm -= 1;
            } else {
                change(dsp, index);
            }
            for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                (*l, *r) = dsp.process_stereo(*l, *r);
            }
        })
    }

    /// Up and back down across `min..max`, a value per block, over `blocks`.
    fn sweep(min: f32, max: f32, block: usize, blocks: usize) -> f32 {
        let phase = block as f32 / blocks as f32 * 2.0;
        let x = if phase < 1.0 { phase } else { 2.0 - phase };
        min + (max - min) * x
    }

    /// A low bell that changes the test tone, so wet and dry differ.
    fn shaping() -> Dsp {
        let mut params = default_params();
        params.bands[2].active = true;
        params.bands[2].freq = 110.0;
        params.bands[2].gain_db = 9.0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        dsp
    }

    /// The measure itself sees a gain that steps once per block.
    #[test]
    fn the_edge_kink_measure_catches_a_stepped_gain() {
        let stepped = edge_kink(100, |index, left, right| {
            let gain = 0.25 + index as f32 * 0.01;
            left.iter_mut()
                .chain(right.iter_mut())
                .for_each(|s| *s *= gain);
        });
        assert!(stepped > 20.0, "{stepped}");
    }

    #[test]
    fn dragging_output_or_mix_does_not_zipper() {
        let mut dsp = shaping();
        let output = drag(&mut dsp, 375, |dsp, block| {
            let db = sweep(-24.0, 12.0, block, 375);
            assert!(dsp.apply_wire_param(ipc::OUTPUT_INDEX, db));
        });
        assert!(output < 4.0, "output drag jump x {output}");

        let mut dsp = shaping();
        let mix = drag(&mut dsp, 375, |dsp, block| {
            assert!(dsp.apply_wire_param(ipc::MIX_INDEX, sweep(0.0, 100.0, block, 375)));
        });
        assert!(mix < 4.0, "mix drag jump x {mix}");
    }

    #[test]
    fn toggling_power_crossfades_without_a_step() {
        let mut dsp = shaping();
        let jump = drag(&mut dsp, 300, |dsp, block| {
            if block.is_multiple_of(30) {
                let on = (block / 30) % 2 == 1;
                assert!(dsp.apply_wire_param(ipc::POWER_INDEX, if on { 1.0 } else { 0.0 }));
            }
        });
        assert!(jump < 4.0, "power toggle jump x {jump}");
        // Settled off is the dry signal exactly.
        assert!(dsp.apply_wire_param(ipc::POWER_INDEX, 0.0));
        for _ in 0..4_800 {
            let _ = dsp.process_stereo(0.1, 0.1);
        }
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }

    /// A project load lands on its values; it does not fade them in.
    #[test]
    fn restored_state_starts_settled() {
        let mut params = default_params();
        params.output_db = -12.0;
        params.mix = 50.0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let (l, _) = dsp.process_stereo(0.5, 0.5);
        let expected = 0.5 * 0.5 + 0.5 * 0.5 * db_to_linear(-12.0);
        assert!((l - expected).abs() < 1.0e-6, "{l} vs {expected}");
    }
}
