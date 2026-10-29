//! EQ-ZX — 24-band dynamic mid/side parametric EQ.
//!
//! The wide sibling of [`equz8`](../equz8/index.html): same wire/state pattern,
//! but each band additionally carries a stereo-image placement (stereo, left,
//! right, mid or side) and cut bands expand into a Butterworth cascade so
//! slopes run from 6 to 96 dB/oct.
//!
//! Filter coefficients and runtime state use the MIT/Apache [`biquad`] crate
//! through `builtin_dsp_core`, so the embedded editor's drawn curve and the
//! audible result come from the same filter design.
//!
//! # Realtime shape
//!
//! Everything that allocates or designs filters happens on the control thread
//! ([`Dsp::set_params`], [`Dsp::apply_wire_param`]). The audio path walks a
//! precomputed active-band list, runs fixed-size filter arrays, and retunes
//! dynamic bands in place — no allocation, no string lookup, no branching on
//! band count.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, flush_denormal, linear_to_db, make_eq_coefficients, max_filter_frequency, mix,
    time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};

pub const PLUGIN_ID: &str = "futureboard.equzx";

/// Band slots. The editor creates bands on demand up to this many; a slot with
/// `active == false` is simply an empty one, which keeps the parameter table a
/// fixed size and the audio path free of any per-block resizing.
pub const BAND_COUNT: usize = 24;

/// Biquad sections a single band can expand into — 96 dB/oct is a 16th-order
/// cascade, which is 8 second-order sections.
pub const MAX_SECTIONS: usize = 8;

/// dB past the threshold at which a dynamic band reaches its full range. Matches
/// the editor's `DYN_KNEE_DB`, so the curve it draws for a moving band is the
/// curve this produces. The law of a band whose `ratio` is [`RATIO_LEGACY`].
const DYNAMIC_KNEE_DB: f32 = 6.0;

/// `ratio` value meaning "the fixed-knee law projects saved before the ratio
/// control existed were mixed with": the band reaches its full range
/// [`DYNAMIC_KNEE_DB`] past the threshold. A missing `ratio` field
/// deserializes to this, so those projects sound as they always did.
pub const RATIO_LEGACY: f32 = 0.0;

/// The ratio a band the editor creates starts at.
pub const DEFAULT_RATIO: f32 = 2.0;

/// Output peak meters fall back over about this long.
const PEAK_FALL_SECONDS: f32 = 0.4;

/// Log-spaced points between [`AUTO_GAIN_LOW_HZ`] and [`AUTO_GAIN_HIGH_HZ`]
/// the auto gain averages the response over.
pub const AUTO_GAIN_POINTS: usize = 32;
pub const AUTO_GAIN_LOW_HZ: f32 = 60.0;
pub const AUTO_GAIN_HIGH_HZ: f32 = 12_000.0;

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
    /// The auto gain's compensation in dB; 0 while it is off.
    pub const AUTO_GAIN_DB: usize = 2 + 2 * BAND_COUNT;
    /// The reading of a detector that is not listening.
    pub const SILENT_DB: f32 = -120.0;

    const _: () = assert!(AUTO_GAIN_DB < SLOTS);
}

/// Rebuild a band's coefficients when its applied gain moves by at least this
/// much — avoids per-sample filter design on a barely-moving envelope.
const DYNAMIC_GAIN_EPS_DB: f32 = 0.05;

/// Time constant of the rectified detector's level follower. Short enough to
/// track syllables, long enough that the envelope is not riding the waveform.
const DETECTOR_FOLLOW_SECONDS: f32 = 0.005;

/// Q used to audition band shapes that have no meaningful centre width.
///
/// Shelves and cuts are broad by construction, so reusing their own Q would open
/// an audition window either far too wide to isolate anything or so narrow it
/// sits on a slope. A moderate fixed Q gives a consistent "what lives around
/// this corner frequency" listen. Same value and reasoning as EQ-Z8.
const SOLO_WIDE_Q: f32 = 0.9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandType {
    LowCut,
    LowShelf,
    Bell,
    Notch,
    BandPass,
    HighShelf,
    HighCut,
}

impl BandType {
    /// The `builtin_dsp_core` filter kind a single section of this band uses.
    pub fn section_kind(self) -> &'static str {
        match self {
            Self::LowCut => "highpass",
            Self::LowShelf => "lowshelf",
            Self::Bell => "bell",
            Self::Notch => "notch",
            Self::BandPass => "bandpass",
            Self::HighShelf => "highshelf",
            Self::HighCut => "lowpass",
        }
    }

    /// The editor's own name for this shape, as it appears in saved state.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LowCut => "lowcut",
            Self::LowShelf => "lowshelf",
            Self::Bell => "bell",
            Self::Notch => "notch",
            Self::BandPass => "bandpass",
            Self::HighShelf => "highshelf",
            Self::HighCut => "highcut",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "lowcut" | "highpass" | "hp" => Some(Self::LowCut),
            "lowshelf" | "ls" => Some(Self::LowShelf),
            "bell" | "peak" | "peaking" => Some(Self::Bell),
            "notch" => Some(Self::Notch),
            "bandpass" | "bp" => Some(Self::BandPass),
            "highshelf" | "hs" => Some(Self::HighShelf),
            "highcut" | "lowpass" | "lp" => Some(Self::HighCut),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::LowCut => 0.0,
            Self::LowShelf => 1.0,
            Self::Bell => 2.0,
            Self::Notch => 3.0,
            Self::BandPass => 4.0,
            Self::HighShelf => 5.0,
            Self::HighCut => 6.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            0 => Self::LowCut,
            1 => Self::LowShelf,
            3 => Self::Notch,
            4 => Self::BandPass,
            5 => Self::HighShelf,
            6 => Self::HighCut,
            _ => Self::Bell,
        }
    }

    /// Dynamic gain only applies to shapes that have a gain stage.
    pub const fn has_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Bell | Self::HighShelf)
    }

    /// Cut shapes expand into a Butterworth cascade sized by their slope.
    pub const fn is_cut(self) -> bool {
        matches!(self, Self::LowCut | Self::HighCut)
    }
}

/// Which part of the stereo image a band acts on.
///
/// Mid is `(L+R)/2` and side is `(L-R)/2`. A `Stereo` band is the same filter
/// applied to both halves, which is identical to filtering L/R — mid/side is a
/// linear transform, so matched filters commute through it.
///
/// Wire order is the persisted contract: Left and Right were appended after
/// Side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandChannel {
    Stereo,
    Mid,
    Side,
    Left,
    Right,
}

impl BandChannel {
    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Stereo => 0.0,
            Self::Mid => 1.0,
            Self::Side => 2.0,
            Self::Left => 3.0,
            Self::Right => 4.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Mid,
            2 => Self::Side,
            3 => Self::Left,
            4 => Self::Right,
            _ => Self::Stereo,
        }
    }

    /// Whether the band works on the mid/side image rather than on left and
    /// right.
    pub const fn is_mid_side(self) -> bool {
        matches!(self, Self::Mid | Self::Side)
    }

    /// Whether the band works on one of left and right alone.
    pub const fn is_left_right(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    /// Does this band touch the first path of its domain (left, or mid)?
    const fn on_path_a(self) -> bool {
        matches!(self, Self::Stereo | Self::Mid | Self::Left)
    }

    /// Does this band touch the second path of its domain (right, or side)?
    const fn on_path_b(self) -> bool {
        matches!(self, Self::Stereo | Self::Side | Self::Right)
    }

    /// Whether a band placed at `self` shapes the part of the image `part`
    /// names, in a single-path response curve.
    const fn shapes(self, part: Self) -> bool {
        match part {
            Self::Stereo => true,
            Self::Mid => matches!(self, Self::Stereo | Self::Mid),
            Self::Side => matches!(self, Self::Stereo | Self::Side),
            Self::Left => matches!(self, Self::Stereo | Self::Left),
            Self::Right => matches!(self, Self::Stereo | Self::Right),
        }
    }
}

/// How the active bands are laid across the two stereo domains.
///
/// * `Lr` — no mid- or side-only band: everything runs on left and right, a
///   left or right band on its own channel.
/// * `Ms` — mid/side bands but no left/right ones: the whole chain runs in
///   M/S, exactly as it did before left and right existed (stereo bands on
///   both mid and side), so those projects play unchanged.
/// * `Split` — both kinds: every stereo, left and right band runs first on
///   left/right, then the mid and side bands in M/S. Every static band is a
///   linear time-invariant filter, so the order across the two stages does
///   not change what a static chain plays; only a dynamic band's detector
///   hears its own stage's signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Routing {
    Lr,
    Ms,
    Split,
}

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BandParams {
    pub active: bool,
    pub band_type: BandType,
    /// Stereo placement. `serde(default)` so state written before mid/side
    /// existed loads as a plain stereo band.
    #[serde(default = "default_channel")]
    pub channel: BandChannel,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
    /// dB/oct for cut shapes; ignored by everything else. One of
    /// [`ipc::SLOPES`]: 6 is a first-order section, the rest even-order
    /// Butterworth cascades.
    #[serde(default = "default_slope")]
    pub slope: f32,
    /// When true (and the shape has gain), the band's applied gain moves from
    /// `gain_db` toward `gain_db + range_db` as its own detector crosses
    /// `threshold_db` in the direction `dyn_mode` selects.
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default = "default_dyn_mode")]
    pub dyn_mode: DynMode,
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f32,
    #[serde(default)]
    pub range_db: f32,
    #[serde(default = "default_attack_ms")]
    pub attack_ms: f32,
    #[serde(default = "default_release_ms")]
    pub release_ms: f32,
    /// Dynamic ratio 1..10, or [`RATIO_LEGACY`] for the fixed-knee law
    /// (what a missing field loads as).
    #[serde(default)]
    pub ratio: f32,
}

fn default_channel() -> BandChannel {
    BandChannel::Stereo
}

fn default_slope() -> f32 {
    24.0
}

fn default_dyn_mode() -> DynMode {
    DynMode::Above
}

fn default_threshold_db() -> f32 {
    -24.0
}

fn default_attack_ms() -> f32 {
    20.0
}

fn default_release_ms() -> f32 {
    200.0
}

fn flat_band() -> BandParams {
    BandParams {
        active: false,
        band_type: BandType::Bell,
        channel: BandChannel::Stereo,
        freq: 1_000.0,
        gain_db: 0.0,
        q: 1.0,
        slope: default_slope(),
        dynamic: false,
        dyn_mode: default_dyn_mode(),
        threshold_db: default_threshold_db(),
        range_db: 0.0,
        attack_ms: default_attack_ms(),
        release_ms: default_release_ms(),
        ratio: DEFAULT_RATIO,
    }
}

/// A band array that deserializes from a shorter (or absent) list.
///
/// The editor only ever sends the bands that exist, so state written with three
/// bands must not fail to load into 24 slots — the remainder fill with empty
/// ones. Serialization stays a plain 24-entry array.
mod band_array {
    use super::{BAND_COUNT, BandParams, flat_band};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        bands: &[BandParams; BAND_COUNT],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bands.as_slice().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[BandParams; BAND_COUNT], D::Error> {
        let listed = Vec::<BandParams>::deserialize(deserializer)?;
        let mut bands = [flat_band(); BAND_COUNT];
        for (slot, band) in bands.iter_mut().zip(listed.into_iter().take(BAND_COUNT)) {
            *slot = band;
        }
        Ok(bands)
    }
}

fn default_bands() -> [BandParams; BAND_COUNT] {
    [flat_band(); BAND_COUNT]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub output_db: f32,
    #[serde(default = "default_bands", with = "band_array")]
    pub bands: [BandParams; BAND_COUNT],
    /// Index of the band being auditioned in isolation, or [`ipc::SOLO_NONE`].
    #[serde(default = "solo_none")]
    pub solo_band: i32,
    /// Wet share, 0..100. Projects saved before the control load fully wet.
    #[serde(default = "full_mix")]
    pub mix: f32,
    /// Level-matches the processed signal against the curve's average
    /// boost or cut ([`auto_gain_db`]).
    #[serde(default)]
    pub auto_gain: bool,
}

fn solo_none() -> i32 {
    ipc::SOLO_NONE
}

fn full_mix() -> f32 {
    100.0
}

/// Neutral insert state: every slot empty, output flat. The editor opens on a
/// flat curve and the user clicks the display to create bands, so there is no
/// starting layout to seed here.
pub fn default_params() -> Params {
    Params {
        power: true,
        output_db: 0.0,
        bands: default_bands(),
        solo_band: ipc::SOLO_NONE,
        mix: full_mix(),
        auto_gain: false,
    }
}

/// How far toward its full range a dynamic band is pulled, 0..1, by a
/// detector reading `level_db` — the steady state the band's attack and
/// release glide toward.
///
/// With a ratio (1..10) the gain moves like a compressor's: `Above`, by
/// `(level − threshold)·(1 − 1/ratio)` dB; `Below`, by
/// `(threshold − level)·(ratio − 1)·0.5` dB; either way never past the range.
/// With [`RATIO_LEGACY`] the full range is reached [`DYNAMIC_KNEE_DB`] past
/// the threshold.
pub fn dynamic_fraction(band: &BandParams, level_db: f32) -> f32 {
    let over = match band.dyn_mode {
        DynMode::Above => level_db - band.threshold_db,
        DynMode::Below => band.threshold_db - level_db,
    };
    if band.ratio < 1.0 {
        return clamp(over / DYNAMIC_KNEE_DB, 0.0, 1.0);
    }
    if over <= 0.0 {
        return 0.0;
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

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "EQ-ZX",
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
                id: "outputDb",
                name: "Output",
                default_value: 0.0,
                min: ipc::OUTPUT_MIN_DB,
                max: ipc::OUTPUT_MAX_DB,
                unit: "dB",
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
                id: "autoGain",
                name: "Auto Gain",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

/// Butterworth section Qs for a cascade of the given filter order.
///
/// Order `n` needs `n/2` sections, the k-th at `1 / (2 cos((2k+1)π / 2n))`.
/// Mirrors the editor's `butterworthQs`, which is what makes the drawn slope
/// and the audible slope the same filter. (An odd order's real pole is the
/// first-order section [`first_order_cut`] adds.)
fn butterworth_q(order: usize, k: usize) -> f32 {
    let angle = (2.0 * k as f32 + 1.0) * std::f32::consts::PI / (2.0 * order as f32);
    1.0 / (2.0 * angle.cos())
}

/// A cut's filter order: its slope over 6 dB/oct.
fn cut_order(band: &BandParams) -> usize {
    ((band.slope / 6.0).round() as usize).clamp(1, MAX_SECTIONS * 2)
}

/// How many sections a band expands into: one for a non-cut shape; for a
/// cut, one per pole pair plus a first-order section for an odd order.
pub fn section_count(band: &BandParams) -> usize {
    if !band.band_type.is_cut() {
        return 1;
    }
    cut_order(band).div_ceil(2).clamp(1, MAX_SECTIONS)
}

/// A first-order Butterworth cut at `freq_hz` (bilinear, prewarped), as a
/// biquad with its second-order terms zero: −3 dB at the corner, 6 dB/oct
/// past it.
fn first_order_cut(high_pass: bool, freq_hz: f32, sample_rate: f32) -> Coefficients<f32> {
    let fs = f64::from(sample_rate.max(1.0));
    let f0 = f64::from(clamp(freq_hz, 10.0, max_filter_frequency(sample_rate)));
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let a1 = (k - 1.0) / (k + 1.0);
    let (b0, b1) = if high_pass {
        let b0 = 1.0 / (1.0 + k);
        (b0, -b0)
    } else {
        let b0 = k / (1.0 + k);
        (b0, b0)
    };
    Coefficients {
        a1: a1 as f32,
        a2: 0.0,
        b0: b0 as f32,
        b1: b1 as f32,
        b2: 0.0,
    }
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

/// The coefficients of section `section` of `band` at gain `gain_db`: for a
/// cut, the pole pairs of its Butterworth cascade and, on an odd order, the
/// first-order section last; otherwise the one section at the band's Q.
fn section_coefficients(
    band: &BandParams,
    section: usize,
    gain_db: f32,
    sample_rate: f32,
) -> Option<Coefficients<f32>> {
    if !band.band_type.is_cut() {
        return design(
            band.band_type.section_kind(),
            band.freq,
            gain_db,
            band.q,
            sample_rate,
        );
    }
    let order = cut_order(band);
    if order % 2 == 1 && section == order / 2 {
        return Some(first_order_cut(
            band.band_type == BandType::LowCut,
            band.freq,
            sample_rate,
        ));
    }
    design(
        band.band_type.section_kind(),
        band.freq,
        gain_db,
        butterworth_q(order, section),
        sample_rate,
    )
}

/// What `band` does to a sine at `freq_hz`, in dB, with its gain at
/// `gain_db`: the same sections [`Dsp`] builds for it — a Butterworth cascade
/// for a cut, one section otherwise — so a drawn curve is the curve that
/// plays. 0 dB for a band that is switched off. Control/UI side only.
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

/// [`band_response_at_gain_db`] at the band's own gain.
pub fn band_response_db(band: &BandParams, freq_hz: f32, sample_rate: f32) -> f32 {
    band_response_at_gain_db(band, band.gain_db, freq_hz, sample_rate)
}

/// Whether any switched-on band acts on one part of the image alone (mid,
/// side, left or right), which splits the response into per-part curves.
pub fn uses_mid_side(params: &Params) -> bool {
    params
        .bands
        .iter()
        .any(|band| band.active && band.channel != BandChannel::Stereo)
}

/// The EQ's response at `freq_hz`, in dB, for the part of the image
/// `channel` names, output gain included: a mid curve is every stereo and
/// mid band, a side curve every stereo and side band, a left (right) curve
/// every stereo and left (right) band, and a stereo curve every band. A band
/// placed in the other domain (a side band on the left curve) is left out:
/// what it does to one channel depends on the signal, not on the curve.
/// What the insert plays with no band moving dynamically.
pub fn response_db(params: &Params, channel: BandChannel, freq_hz: f32, sample_rate: f32) -> f32 {
    params
        .bands
        .iter()
        .filter(|band| band.channel.shapes(channel))
        .map(|band| band_response_db(band, freq_hz, sample_rate))
        .sum::<f32>()
        + params.output_db
}

/// `(cos w, sin w, cos 2w, sin 2w)` at each of the auto gain's points.
type AutoGainGrid = [[f64; 4]; AUTO_GAIN_POINTS];

fn auto_gain_grid(sample_rate: f32) -> AutoGainGrid {
    let mut grid = [[0.0; 4]; AUTO_GAIN_POINTS];
    let span = f64::from(AUTO_GAIN_HIGH_HZ / AUTO_GAIN_LOW_HZ);
    for (index, point) in grid.iter_mut().enumerate() {
        let hz =
            f64::from(AUTO_GAIN_LOW_HZ) * span.powf(index as f64 / (AUTO_GAIN_POINTS - 1) as f64);
        let w = std::f64::consts::TAU * hz / f64::from(sample_rate.max(1.0));
        *point = [w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin()];
    }
    grid
}

/// `coefficients`' power gain at one grid point (what
/// [`biquad_response_db`] computes, with the trig done once).
#[inline]
fn power_at(c: &Coefficients<f32>, [cos1, sin1, cos2, sin2]: [f64; 4]) -> f64 {
    let num_re = f64::from(c.b0) + f64::from(c.b1) * cos1 + f64::from(c.b2) * cos2;
    let num_im = -(f64::from(c.b1) * sin1 + f64::from(c.b2) * sin2);
    let den_re = 1.0 + f64::from(c.a1) * cos1 + f64::from(c.a2) * cos2;
    let den_im = -(f64::from(c.a1) * sin1 + f64::from(c.a2) * sin2);
    ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im).max(1e-30))
        .max(1e-30)
}

/// The auto gain on `grid`: every section is designed once and evaluated at
/// each point, nothing allocates. The audio path and [`auto_gain_db`] share
/// it, so the readout is the gain that plays.
fn auto_gain_on(params: &Params, sample_rate: f32, grid: &AutoGainGrid) -> f32 {
    if !params.power || !params.auto_gain {
        return 0.0;
    }
    let mut sum_db = [0.0f64; AUTO_GAIN_POINTS];
    for band in params.bands.iter().filter(|band| band.active) {
        // A band's sections multiply; one log per point per band.
        let mut power = [1.0f64; AUTO_GAIN_POINTS];
        for section in 0..section_count(band) {
            let Some(coefficients) = section_coefficients(band, section, band.gain_db, sample_rate)
            else {
                continue;
            };
            for (total, point) in power.iter_mut().zip(grid) {
                *total *= power_at(&coefficients, *point);
            }
        }
        for (total, power) in sum_db.iter_mut().zip(power) {
            *total += 10.0 * power.max(1e-300).log10();
        }
    }
    let average = sum_db.iter().sum::<f64>() / AUTO_GAIN_POINTS as f64;
    clamp(-average as f32, ipc::OUTPUT_MIN_DB, ipc::OUTPUT_MAX_DB)
}

/// The level the auto gain sets, in dB: minus the average of the static
/// response every active band makes (output gain aside), over
/// [`AUTO_GAIN_POINTS`] log-spaced points from 60 Hz to 12 kHz. 0 while auto
/// gain or the plug-in is off. Dynamic bands count at their static gain.
pub fn auto_gain_db(params: &Params, sample_rate: f32) -> f32 {
    auto_gain_on(params, sample_rate, &auto_gain_grid(sample_rate))
}

#[derive(Debug, Clone, Copy)]
struct DynamicBandState {
    /// Smoothed rectified detector output, linear.
    level: f32,
    /// Smoothed engagement, 0..1.
    envelope: f32,
    attack_coeff: f32,
    release_coeff: f32,
    follow_coeff: f32,
    last_applied_db: f32,
    /// The detector's last reading and the offset it produced, for telemetry.
    level_db: f32,
    offset_db: f32,
}

impl DynamicBandState {
    fn new() -> Self {
        Self {
            level: 0.0,
            envelope: 0.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            follow_coeff: 0.0,
            last_applied_db: 0.0,
            level_db: telemetry::SILENT_DB,
            offset_db: 0.0,
        }
    }
}

/// Output gain glides over this long, so a dragged knob — which arrives as
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

type Cascade = [Option<DirectForm1<f32>>; MAX_SECTIONS];

const EMPTY_CASCADE: Cascade = [None; MAX_SECTIONS];

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    /// First processing path: left, or mid when [`Self::ms_mode`] is set.
    path_a: [Cascade; BAND_COUNT],
    /// Second processing path: right, or side.
    path_b: [Cascade; BAND_COUNT],
    /// Band-local detector for dynamic EQ (bandpass around the band centre).
    detector: [Option<DirectForm1<f32>>; BAND_COUNT],
    dyn_state: [DynamicBandState; BAND_COUNT],
    /// Sections each band actually runs, so the audio path never scans past them.
    sections: [u8; BAND_COUNT],
    /// Active band slots in processing order, and how many of them there are,
    /// for the left/right stage. Precomputed on the control thread so the hot
    /// loop skips empty slots without touching them.
    order: [u8; BAND_COUNT],
    order_len: usize,
    /// The same for the mid/side stage.
    ms_order: [u8; BAND_COUNT],
    ms_order_len: usize,
    /// Set when the chain has a mid/side stage (see [`Routing`]).
    ms_mode: bool,
    routing: Routing,
    /// Audition bandpass for the soloed band, one per path.
    solo_a: Option<DirectForm1<f32>>,
    solo_b: Option<DirectForm1<f32>>,
    solo_channel: BandChannel,
    /// Linear output gain, smoothed per sample.
    output_gain: Glide,
    /// Wet share, 0..1.
    mix: Glide,
    /// Linear auto-gain compensation, and whether it needs recomputing.
    auto_gain: Glide,
    auto_gain_db: f32,
    auto_gain_dirty: bool,
    auto_gain_grid: AutoGainGrid,
    /// Dry → processed share, following `params.power`.
    power: Glide,
    /// Fast-path flag: skip detector/envelope work when nothing needs it.
    any_dynamic: bool,
    /// Output peaks, left and right, and their per-sample fall.
    out_peak: [f32; 2],
    peak_fall: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let mut dsp = Self {
            sample_rate: sample_rate.max(1.0),
            params: default_params(),
            path_a: [EMPTY_CASCADE; BAND_COUNT],
            path_b: [EMPTY_CASCADE; BAND_COUNT],
            detector: [None; BAND_COUNT],
            dyn_state: [DynamicBandState::new(); BAND_COUNT],
            sections: [0; BAND_COUNT],
            order: [0; BAND_COUNT],
            order_len: 0,
            ms_order: [0; BAND_COUNT],
            ms_order_len: 0,
            ms_mode: false,
            routing: Routing::Lr,
            solo_a: None,
            solo_b: None,
            solo_channel: BandChannel::Stereo,
            output_gain: Glide::new(1.0, GAIN_SMOOTHING_MS, sample_rate.max(1.0)),
            mix: Glide::new(1.0, GAIN_SMOOTHING_MS, sample_rate.max(1.0)),
            auto_gain: Glide::new(1.0, GAIN_SMOOTHING_MS, sample_rate.max(1.0)),
            auto_gain_db: 0.0,
            auto_gain_dirty: false,
            auto_gain_grid: auto_gain_grid(sample_rate),
            power: Glide::new(1.0, POWER_FADE_MS, sample_rate.max(1.0)),
            any_dynamic: false,
            out_peak: [0.0; 2],
            peak_fall: time_constant(sample_rate.max(1.0), PEAK_FALL_SECONDS),
        };
        dsp.rebuild();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter (project restore). Output, mix, auto gain and
    /// power land on the restored values rather than gliding in from the old
    /// ones.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.rebuild();
        self.refresh_auto_gain();
        self.settle();
    }

    /// Put every smoother on its target.
    fn settle(&mut self) {
        self.output_gain.settle();
        self.mix.settle();
        self.auto_gain.settle();
        self.power.settle();
    }

    /// Recomputes the auto gain's target. Bounded (one coefficient design
    /// per section, 32 points) and allocation-free; runs at most once per
    /// block, on the first sample after the params changed.
    fn refresh_auto_gain(&mut self) {
        self.auto_gain_dirty = false;
        self.auto_gain_db = auto_gain_on(&self.params, self.sample_rate, &self.auto_gain_grid);
        self.auto_gain.set(db_to_linear(self.auto_gain_db));
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
        block[telemetry::AUTO_GAIN_DB] = self.auto_gain_db;
        block
    }

    #[inline]
    fn track_peaks(&mut self, left: f32, right: f32) {
        let fall = self.peak_fall;
        let [l, r] = &mut self.out_peak;
        *l = flush_denormal(left.abs().max(*l * fall));
        *r = flush_denormal(right.abs().max(*r * fall));
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    ///
    /// The audio path never parses JSON or looks up string parameter ids. A host
    /// bridge drains its bounded parameter ring between blocks and calls this.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }

        match wire_index {
            ipc::POWER_INDEX => {
                self.power.set(power_share(self.params.power));
                self.auto_gain_dirty = true;
            }
            ipc::OUTPUT_INDEX => self.output_gain.set(db_to_linear(self.params.output_db)),
            ipc::MIX_INDEX => self.mix.set(self.params.mix / 100.0),
            ipc::AUTO_GAIN_INDEX => self.auto_gain_dirty = true,
            ipc::SOLO_INDEX => self.rebuild_solo(),
            _ => {
                if let Some((band, field)) = ipc::decode_band_wire(wire_index) {
                    self.rebuild_band(band);
                    self.auto_gain_dirty = true;
                    // Enabling, disabling or re-placing a band changes which
                    // paths are occupied, and mid/side changes the domain the
                    // whole chain runs in.
                    if matches!(field, ipc::BAND_ENABLED | ipc::BAND_CHANNEL) {
                        self.refresh_routing();
                    }
                    if self.params.solo_band == band as i32 {
                        self.rebuild_solo();
                    }
                } else if ipc::decode_ratio_wire(wire_index).is_some() {
                    // The envelope law only — applied gain follows sample by
                    // sample.
                } else if let Some((band, field)) = ipc::decode_dyn_wire(wire_index) {
                    match field {
                        ipc::BAND_DYN_ENABLED => {
                            self.rebuild_band(band);
                            self.refresh_any_dynamic();
                        }
                        ipc::BAND_DYN_ATTACK | ipc::BAND_DYN_RELEASE => {
                            self.refresh_dyn_timing(band);
                        }
                        // Threshold, range and mode only reshape the envelope
                        // curve — applied gain follows sample by sample.
                        _ => {}
                    }
                }
            }
        }
        true
    }

    fn rebuild(&mut self) {
        self.peak_fall = time_constant(self.sample_rate, PEAK_FALL_SECONDS);
        self.auto_gain_grid = auto_gain_grid(self.sample_rate);
        self.auto_gain_dirty = true;
        self.output_gain
            .set_time(GAIN_SMOOTHING_MS, self.sample_rate);
        self.mix.set_time(GAIN_SMOOTHING_MS, self.sample_rate);
        self.auto_gain.set_time(GAIN_SMOOTHING_MS, self.sample_rate);
        self.power.set_time(POWER_FADE_MS, self.sample_rate);
        self.output_gain.set(db_to_linear(self.params.output_db));
        self.mix.set(self.params.mix / 100.0);
        self.power.set(power_share(self.params.power));
        for index in 0..BAND_COUNT {
            self.rebuild_band(index);
        }
        self.refresh_routing();
        self.rebuild_solo();
        self.refresh_any_dynamic();
    }

    /// Recompute the active-band lists and the stereo domains ([`Routing`]).
    /// Control thread only.
    fn refresh_routing(&mut self) {
        let active = || self.params.bands.iter().filter(|band| band.active);
        let mid_side = active().any(|band| band.channel.is_mid_side());
        let left_right = active().any(|band| band.channel.is_left_right());
        self.routing = match (mid_side, left_right) {
            (false, _) => Routing::Lr,
            (true, false) => Routing::Ms,
            (true, true) => Routing::Split,
        };
        self.ms_mode = self.routing != Routing::Lr;
        self.order_len = 0;
        self.ms_order_len = 0;
        for (index, band) in self.params.bands.iter().enumerate() {
            if !band.active {
                continue;
            }
            let in_ms_stage = match self.routing {
                Routing::Lr => false,
                Routing::Ms => true,
                Routing::Split => band.channel.is_mid_side(),
            };
            if in_ms_stage {
                self.ms_order[self.ms_order_len] = index as u8;
                self.ms_order_len += 1;
            } else {
                self.order[self.order_len] = index as u8;
                self.order_len += 1;
            }
        }
    }

    fn refresh_any_dynamic(&mut self) {
        self.any_dynamic = self.params.bands.iter().any(|band| {
            band.active && band.dynamic && band.band_type.has_gain() && band.range_db.abs() > 1.0e-6
        });
    }

    fn refresh_dyn_timing(&mut self, index: usize) {
        let band = self.params.bands[index];
        let sample_rate = self.sample_rate;
        let state = &mut self.dyn_state[index];
        state.attack_coeff = time_constant(sample_rate, band.attack_ms.max(0.1) * 0.001);
        state.release_coeff = time_constant(sample_rate, band.release_ms.max(1.0) * 0.001);
        state.follow_coeff = time_constant(sample_rate, DETECTOR_FOLLOW_SECONDS);
    }

    /// Build the audition bandpass for the soloed band, if any.
    fn rebuild_solo(&mut self) {
        let index = self.params.solo_band;
        let (filter, channel) = if index >= 0 && (index as usize) < BAND_COUNT {
            let band = self.params.bands[index as usize];
            let q = match band.band_type {
                BandType::Bell | BandType::Notch | BandType::BandPass => band.q,
                _ => SOLO_WIDE_Q,
            };
            (
                design("bandpass", band.freq, 0.0, q, self.sample_rate)
                    .map(DirectForm1::<f32>::new),
                band.channel,
            )
        } else {
            (None, BandChannel::Stereo)
        };
        self.solo_a = filter;
        self.solo_b = filter;
        self.solo_channel = channel;
    }

    fn rebuild_band(&mut self, index: usize) {
        let band = self.params.bands[index];
        self.refresh_dyn_timing(index);

        if !band.active {
            self.path_a[index] = EMPTY_CASCADE;
            self.path_b[index] = EMPTY_CASCADE;
            self.detector[index] = None;
            self.sections[index] = 0;
            return;
        }

        let count = section_count(&band);
        self.sections[index] = count as u8;

        for section in 0..MAX_SECTIONS {
            let filter = if section < count {
                section_coefficients(&band, section, band.gain_db, self.sample_rate)
                    .map(DirectForm1::<f32>::new)
            } else {
                None
            };
            // A band with a single section only ever occupies slot 0; the rest
            // stay `None` so the audio path can stop at `sections[index]`.
            self.path_a[index][section] = filter;
            self.path_b[index][section] = filter;
        }
        self.dyn_state[index].last_applied_db = band.gain_db;

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

    /// Retune a dynamic band's sections to a new applied gain.
    ///
    /// Only gain-bearing shapes reach this, and those are always single-section,
    /// so there is exactly one filter per path to retune.
    fn set_band_applied_gain(&mut self, index: usize, applied_db: f32) {
        let band = self.params.bands[index];
        let Some(coeffs) = design(
            band.band_type.section_kind(),
            band.freq,
            applied_db,
            band.q,
            self.sample_rate,
        ) else {
            return;
        };
        // Retune in place so filter history survives — installing a fresh
        // DirectForm1 would zero the delay line and click on every gain step.
        if let Some(filter) = self.path_a[index][0].as_mut() {
            filter.update_coefficients(coeffs);
        }
        if let Some(filter) = self.path_b[index][0].as_mut() {
            filter.update_coefficients(coeffs);
        }
        self.dyn_state[index].last_applied_db = applied_db;
    }

    /// One control step of a band's dynamics, returning the gain offset in dB.
    ///
    /// Mirrors the editor's `dynamicStep`: distance past the threshold in the
    /// engaging direction maps across a soft knee to 0..1, which is then smoothed
    /// with attack going up and release coming down.
    #[inline]
    fn dynamic_delta_db(&mut self, index: usize, detector_input: f32) -> f32 {
        let band = self.params.bands[index];
        let detected = match self.detector[index].as_mut() {
            Some(filter) => filter.run(detector_input).abs(),
            None => detector_input.abs(),
        };

        let state = &mut self.dyn_state[index];
        state.level = flush_denormal(
            state.follow_coeff * state.level + (1.0 - state.follow_coeff) * detected,
        );
        let level_db = linear_to_db(state.level.max(1.0e-6));
        let target = dynamic_fraction(&band, level_db);

        let coeff = if target > state.envelope {
            state.attack_coeff
        } else {
            state.release_coeff
        };
        state.envelope = flush_denormal(coeff * state.envelope + (1.0 - coeff) * target);
        let offset = state.envelope * band.range_db;
        state.level_db = level_db;
        state.offset_db = offset;
        offset
    }

    /// Run one band's cascade over whichever paths of its stage it occupies.
    #[inline]
    fn run_band(&mut self, index: usize, a: &mut f32, b: &mut f32) {
        let channel = self.params.bands[index].channel;
        let run_a = channel.on_path_a();
        let run_b = channel.on_path_b();
        let count = self.sections[index] as usize;

        for section in 0..count {
            if run_a && let Some(filter) = self.path_a[index][section].as_mut() {
                *a = filter.run(*a);
            }
            if run_b && let Some(filter) = self.path_b[index][section].as_mut() {
                *b = filter.run(*b);
            }
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        for cascade in self.path_a.iter_mut().chain(self.path_b.iter_mut()) {
            for filter in cascade.iter_mut().flatten() {
                filter.reset_state();
            }
        }
        for filter in self.detector.iter_mut().flatten() {
            filter.reset_state();
        }
        for state in &mut self.dyn_state {
            state.level = 0.0;
            state.envelope = 0.0;
            state.level_db = telemetry::SILENT_DB;
            state.offset_db = 0.0;
        }
        for filter in self.solo_a.iter_mut().chain(self.solo_b.iter_mut()) {
            filter.reset_state();
        }
        self.out_peak = [0.0; 2];
        if self.auto_gain_dirty {
            self.refresh_auto_gain();
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
        if self.auto_gain_dirty {
            self.refresh_auto_gain();
        }
        let on = self.power.next();
        let output = self.output_gain.next();
        let auto = self.auto_gain.next();
        let amount = self.mix.next();
        // A band audition is heard alone at the output level: neither the
        // dry signal nor the curve's level match belongs in it.
        let soloing = self.solo_a.is_some();
        // The auto gain is a gain like the output's: 1.0 exactly while off,
        // so a chain without it multiplies by what it always did.
        let gain = if soloing { output } else { output * auto };
        let (wet_l, wet_r) = self.process_on(left, right, gain);
        // At full mix `mix` is the wet signal exactly.
        let (wet_l, wet_r) = if soloing {
            (wet_l, wet_r)
        } else {
            (mix(left, wet_l, amount), mix(right, wet_r, amount))
        };
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
    /// The powered chain for one sample at output `gain` (already smoothed).
    #[inline]
    fn process_on(&mut self, left: f32, right: f32, gain: f32) -> (f32, f32) {
        // Band solo replaces the chain rather than adding to it: auditioning a
        // band means hearing that region alone. A mid- or side-placed band is
        // auditioned in its own domain, so soloing a side band gives you the
        // sides on their own rather than folded back into the image.
        if let (Some(solo_a), Some(solo_b)) = (self.solo_a.as_mut(), self.solo_b.as_mut()) {
            return match self.solo_channel {
                BandChannel::Stereo => (solo_a.run(left) * gain, solo_b.run(right) * gain),
                BandChannel::Mid => {
                    let mid = solo_a.run((left + right) * 0.5) * gain;
                    (mid, mid)
                }
                BandChannel::Side => {
                    let side = solo_b.run((left - right) * 0.5) * gain;
                    (side, -side)
                }
                // A one-sided band is auditioned on its own side, the other
                // silent, so the audition says where the band works.
                BandChannel::Left => (solo_a.run(left) * gain, 0.0),
                BandChannel::Right => (0.0, solo_b.run(right) * gain),
            };
        }

        // Left/right stage: everything in `Lr`, the non-mid/side bands in
        // `Split`, nothing in `Ms`.
        let (mut a, mut b) = (left, right);
        for slot in 0..self.order_len {
            let index = self.order[slot] as usize;
            self.step_band(index, &mut a, &mut b);
        }

        if self.ms_order_len == 0 {
            return (a * gain, b * gain);
        }
        // Mid/side stage. Encode once for the stage rather than per band. Mono
        // material simply lands entirely in mid with side silent, which is
        // correct.
        let (mut mid, mut side) = ((a + b) * 0.5, (a - b) * 0.5);
        for slot in 0..self.ms_order_len {
            let index = self.ms_order[slot] as usize;
            self.step_band(index, &mut mid, &mut side);
        }
        ((mid + side) * gain, (mid - side) * gain)
    }

    /// One band at its place in its stage: its dynamics, if any, then its
    /// cascade over the paths it occupies.
    #[inline]
    fn step_band(&mut self, index: usize, a: &mut f32, b: &mut f32) {
        let band = self.params.bands[index];
        if self.any_dynamic
            && band.dynamic
            && band.band_type.has_gain()
            && band.range_db.abs() > 1.0e-6
        {
            // The detector listens to the paths the band actually acts on, so
            // a side-placed dynamic band reacts to the sides only.
            let detector_input = match band.channel {
                BandChannel::Mid | BandChannel::Left => *a,
                BandChannel::Side | BandChannel::Right => *b,
                BandChannel::Stereo => a.abs().max(b.abs()),
            };
            let delta = self.dynamic_delta_db(index, detector_input);
            let applied = band.gain_db + delta;
            if (applied - self.dyn_state[index].last_applied_db).abs() >= DYNAMIC_GAIN_EPS_DB {
                self.set_band_applied_gain(index, applied);
            }
        }
        self.run_band(index, a, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_bell(freq: f32, gain_db: f32, q: f32) -> BandParams {
        BandParams {
            active: true,
            band_type: BandType::Bell,
            freq,
            gain_db,
            q,
            ..flat_band()
        }
    }

    /// Measure output level at one frequency by running a sine through the DSP.
    fn level_at(dsp: &mut Dsp, freq_hz: f32) -> f32 {
        dsp.reset();
        let sr = 48_000.0f32;
        let step = std::f32::consts::TAU * freq_hz / sr;
        for i in 0..4_000 {
            let _ = dsp.process_stereo((i as f32 * step).sin(), (i as f32 * step).sin());
        }
        let mut peak = 0.0f32;
        for i in 4_000..8_000 {
            let (l, _) = dsp.process_stereo((i as f32 * step).sin(), (i as f32 * step).sin());
            peak = peak.max(l.abs());
        }
        peak
    }

    #[test]
    fn descriptor_id() {
        assert_eq!(descriptor().id, PLUGIN_ID);
    }

    /// The curves the editor draws are what plays: a mono sine (pure mid)
    /// through the running DSP lands on the mid curve — steep cut cascade,
    /// mid and stereo bands included, the side-only band ignored.
    #[test]
    fn the_drawn_response_is_what_plays() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            active: true,
            band_type: BandType::LowCut,
            freq: 180.0,
            slope: 48.0,
            ..flat_band()
        };
        params.bands[1] = enabled_bell(1_000.0, 6.0, 1.4);
        params.bands[2] = BandParams {
            channel: BandChannel::Mid,
            ..enabled_bell(3_000.0, -5.0, 2.0)
        };
        params.bands[3] = BandParams {
            active: true,
            band_type: BandType::HighShelf,
            channel: BandChannel::Side,
            freq: 6_000.0,
            gain_db: 9.0,
            q: 0.7,
            ..flat_band()
        };
        params.output_db = 1.5;
        assert!(uses_mid_side(&params));
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        for hz in [123.0, 257.0, 1_013.0, 3_011.0, 9_007.0] {
            let heard = linear_to_db(level_at(&mut dsp, hz));
            let drawn = response_db(&params, BandChannel::Mid, hz, 48_000.0);
            assert!(
                (heard - drawn).abs() < 0.3,
                "{hz} Hz: heard {heard} drawn {drawn}"
            );
        }
        // The side curve carries the shelf, the mid curve does not.
        let side = response_db(&params, BandChannel::Side, 12_000.0, 48_000.0);
        let mid = response_db(&params, BandChannel::Mid, 12_000.0, 48_000.0);
        assert!(side - mid > 7.0, "side {side} mid {mid}");
        // 48 dB/oct: an octave under the corner is about 48 dB down.
        let octave_under = band_response_db(&params.bands[0], 90.0, 48_000.0);
        assert!((octave_under + 48.0).abs() < 3.0, "{octave_under}");
    }

    #[test]
    fn default_is_flat_and_empty() {
        let params = default_params();
        assert!(params.bands.iter().all(|band| !band.active));
        assert_eq!(params.solo_band, ipc::SOLO_NONE);
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
    fn empty_chain_passes_audio_through_unchanged() {
        let mut dsp = Dsp::new(48_000.0);
        let (l, r) = dsp.process_stereo(0.25, -0.25);
        assert!((l - 0.25).abs() < 1.0e-6);
        assert!((r + 0.25).abs() < 1.0e-6);
    }

    #[test]
    fn processes_without_nan() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        for (index, band) in params.bands.iter_mut().enumerate() {
            *band = enabled_bell(100.0 * (index as f32 + 1.0), 6.0, 2.0);
        }
        dsp.set_params(params);
        let (l, r) = dsp.process_stereo(0.5, -0.5);
        assert!(l.is_finite() && r.is_finite());
    }

    #[test]
    fn bell_boost_lifts_its_own_frequency() {
        let mut params = default_params();
        params.bands[0] = enabled_bell(1_000.0, 12.0, 2.0);
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let at_band = level_at(&mut dsp, 1_000.0);
        let far = level_at(&mut dsp, 100.0);
        assert!(
            at_band > 2.0,
            "+12 dB bell should lift its centre: {at_band}"
        );
        assert!(far < 1.2, "far from the band should stay near unity: {far}");
    }

    /// The whole point of the slope control: a steeper cut has to reject more.
    #[test]
    fn steeper_cut_slopes_reject_more() {
        let measure = |slope: f32| {
            let mut params = default_params();
            params.bands[0] = BandParams {
                active: true,
                band_type: BandType::LowCut,
                freq: 1_000.0,
                slope,
                ..flat_band()
            };
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            level_at(&mut dsp, 250.0)
        };

        let gentle = measure(12.0);
        let steep = measure(96.0);
        assert!(
            steep < gentle * 0.5,
            "96 dB/oct must reject far more than 12 at two octaves down: {steep} vs {gentle}"
        );
    }

    #[test]
    fn cut_bands_expand_into_the_expected_section_count() {
        let cut = |slope: f32| {
            section_count(&BandParams {
                band_type: BandType::LowCut,
                slope,
                ..flat_band()
            })
        };
        assert_eq!(cut(12.0), 1);
        assert_eq!(cut(24.0), 2);
        assert_eq!(cut(96.0), MAX_SECTIONS);
        // A shape that is not a cut is always one section, whatever the slope.
        assert_eq!(section_count(&enabled_bell(1_000.0, 0.0, 1.0)), 1);
    }

    /// A mid-placed band must not touch a purely-sided signal, and vice versa.
    /// This is the property that makes mid/side worth having at all.
    #[test]
    fn mid_band_leaves_a_pure_side_signal_alone() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            channel: BandChannel::Mid,
            ..enabled_bell(1_000.0, 18.0, 1.0)
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        // Anti-phase input is pure side: mid is zero, so a mid band has nothing
        // to act on and the signal must come back untouched.
        let sr = 48_000.0f32;
        let step = std::f32::consts::TAU * 1_000.0 / sr;
        let mut peak_error = 0.0f32;
        for i in 0..8_000 {
            let x = (i as f32 * step).sin();
            let (l, r) = dsp.process_stereo(x, -x);
            if i > 4_000 {
                peak_error = peak_error.max((l - x).abs()).max((r + x).abs());
            }
        }
        assert!(
            peak_error < 1.0e-3,
            "a mid band boosted 18 dB changed a pure side signal by {peak_error}"
        );
    }

    #[test]
    fn side_band_leaves_a_pure_mono_signal_alone() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            channel: BandChannel::Side,
            ..enabled_bell(1_000.0, 18.0, 1.0)
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let sr = 48_000.0f32;
        let step = std::f32::consts::TAU * 1_000.0 / sr;
        let mut peak_error = 0.0f32;
        for i in 0..8_000 {
            let x = (i as f32 * step).sin();
            let (l, r) = dsp.process_stereo(x, x);
            if i > 4_000 {
                peak_error = peak_error.max((l - x).abs()).max((r - x).abs());
            }
        }
        assert!(
            peak_error < 1.0e-3,
            "a side band boosted 18 dB changed a mono signal by {peak_error}"
        );
    }

    /// M/S encode and decode must be lossless when nothing is placed off-centre,
    /// or simply enabling a mid band would shift the image of every other band.
    #[test]
    fn ms_mode_round_trips_a_stereo_band_unchanged() {
        let build = |channel: BandChannel| {
            let mut params = default_params();
            params.bands[0] = enabled_bell(1_000.0, 6.0, 1.0);
            // A second, inactive-in-effect band that only flips the domain.
            params.bands[1] = BandParams {
                channel,
                ..enabled_bell(8_000.0, 0.0, 1.0)
            };
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            dsp
        };

        let mut stereo_domain = build(BandChannel::Stereo);
        let mut ms_domain = build(BandChannel::Mid);
        assert!(!stereo_domain.ms_mode && ms_domain.ms_mode);

        let sr = 48_000.0f32;
        let step = std::f32::consts::TAU * 1_000.0 / sr;
        let mut peak_error = 0.0f32;
        for i in 0..8_000 {
            let l = (i as f32 * step).sin();
            let r = (i as f32 * step * 0.5).sin();
            let plain = stereo_domain.process_stereo(l, r);
            let encoded = ms_domain.process_stereo(l, r);
            if i > 4_000 {
                peak_error = peak_error
                    .max((plain.0 - encoded.0).abs())
                    .max((plain.1 - encoded.1).abs());
            }
        }
        assert!(
            peak_error < 1.0e-3,
            "a stereo band should sound identical in either domain, differed by {peak_error}"
        );
    }

    #[test]
    fn solo_isolates_the_band_frequency_region() {
        let mut params = default_params();
        params.bands[4] = enabled_bell(1_000.0, 0.0, 2.0);
        params.solo_band = 4;

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let in_band = level_at(&mut dsp, 1_000.0);
        let below = level_at(&mut dsp, 100.0);
        let above = level_at(&mut dsp, 10_000.0);

        assert!(in_band > 0.5, "the soloed region must pass: {in_band}");
        assert!(below < in_band * 0.2, "below must be rejected: {below}");
        assert!(above < in_band * 0.2, "above must be rejected: {above}");
    }

    /// Soloing a side band should give the sides alone — a mono input has none,
    /// so it must come back near silent.
    #[test]
    fn soloing_a_side_band_auditions_the_sides_only() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            channel: BandChannel::Side,
            ..enabled_bell(1_000.0, 0.0, 1.0)
        };
        params.solo_band = 0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let mono = level_at(&mut dsp, 1_000.0);
        assert!(
            mono < 0.05,
            "a mono signal has no side content to audition, got {mono}"
        );
    }

    #[test]
    fn clearing_solo_restores_the_eq_chain() {
        let mut params = default_params();
        params.bands[2] = enabled_bell(500.0, 3.0, 1.0);
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
        params.bands[4] = enabled_bell(1_000.0, 0.0, 2.0);
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
    fn wire_updates_reach_the_running_filters() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_wire_param(ipc::band_wire_index(2, ipc::BAND_ENABLED), 1.0));
        assert!(dsp.apply_wire_param(ipc::band_wire_index(2, ipc::BAND_GAIN), 6.0));
        assert_eq!(dsp.params().bands[2].gain_db, 6.0);
        assert_eq!(dsp.order_len, 1, "enabling a band must add it to the order");

        assert!(dsp.apply_wire_param(ipc::band_wire_index(2, ipc::BAND_CHANNEL), 1.0));
        assert!(dsp.ms_mode, "a mid band must put the chain in M/S");

        assert!(dsp.apply_wire_param(ipc::band_wire_index(2, ipc::BAND_ENABLED), 0.0));
        assert_eq!(dsp.order_len, 0);
        assert!(!dsp.ms_mode, "no active mid band means no M/S domain");
    }

    #[test]
    fn dynamic_cut_reduces_hot_in_band_signal() {
        let mut params = default_params();
        params.bands[4] = BandParams {
            dynamic: true,
            dyn_mode: DynMode::Above,
            threshold_db: -40.0,
            range_db: -12.0,
            attack_ms: 0.1,
            release_ms: 50.0,
            ..enabled_bell(1_000.0, 0.0, 2.0)
        };

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let hot = level_at(&mut dsp, 1_000.0);
        assert!(hot < 0.85, "dynamic cut must reduce a hot tone: {hot}");

        let far = level_at(&mut dsp, 80.0);
        assert!(
            far > hot,
            "out-of-band content must stay closer to unity: far={far} hot={hot}"
        );
    }

    /// `below` mode is the mirror image: a quiet band gets moved, a hot one is
    /// left alone. Without this the mode switch would be a no-op control.
    #[test]
    fn dynamic_below_mode_engages_on_quiet_signal() {
        let mut params = default_params();
        params.bands[4] = BandParams {
            dynamic: true,
            dyn_mode: DynMode::Below,
            threshold_db: -12.0,
            range_db: -12.0,
            attack_ms: 0.1,
            release_ms: 50.0,
            ..enabled_bell(1_000.0, 0.0, 2.0)
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        // Quiet tone sits below the threshold, so the cut engages.
        dsp.reset();
        let sr = 48_000.0f32;
        let step = std::f32::consts::TAU * 1_000.0 / sr;
        let mut quiet_peak = 0.0f32;
        for i in 0..8_000 {
            let x = 0.01 * (i as f32 * step).sin();
            let (l, _) = dsp.process_stereo(x, x);
            if i > 4_000 {
                quiet_peak = quiet_peak.max(l.abs());
            }
        }
        assert!(
            quiet_peak < 0.01 * 0.85,
            "below-mode must pull down a quiet in-band tone, got {quiet_peak}"
        );
    }

    #[test]
    fn state_json_round_trips_through_the_dsp() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            channel: BandChannel::Mid,
            slope: 48.0,
            ..enabled_bell(440.0, -6.0, 3.0)
        };
        let json = ipc::EquzxState::new(params).to_json().expect("serialize");
        let restored = ipc::EquzxState::from_json(&json).expect("deserialize");

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(restored.params);
        assert_eq!(dsp.params().bands[0].channel, BandChannel::Mid);
        assert_eq!(dsp.params().bands[0].gain_db, -6.0);
        assert!(dsp.ms_mode);
    }

    /// Out-of-range values from a hand-edited project must be pinned, not passed
    /// to the filter design.
    #[test]
    fn restored_state_is_sanitized() {
        let mut params = default_params();
        params.output_db = 999.0;
        params.solo_band = 400;
        params.bands[0] = BandParams {
            freq: 1.0e9,
            gain_db: 400.0,
            q: -3.0,
            slope: 37.0,
            ..enabled_bell(1_000.0, 0.0, 1.0)
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        let band = dsp.params().bands[0];
        assert_eq!(dsp.params().output_db, ipc::OUTPUT_MAX_DB);
        assert_eq!(dsp.params().solo_band, ipc::SOLO_NONE);
        assert_eq!(band.freq, ipc::FREQ_MAX);
        assert_eq!(band.gain_db, ipc::GAIN_MAX_DB);
        assert_eq!(band.q, ipc::Q_MIN);
        assert_eq!(band.slope, 36.0);
        assert!(dsp.process_stereo(0.5, 0.5).0.is_finite());
    }

    #[test]
    fn sample_rate_change_keeps_the_chain_valid() {
        let mut params = default_params();
        params.bands[0] = enabled_bell(15_000.0, 6.0, 1.0);
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        // 15 kHz is above Nyquist-limited placement at 22.05 kHz sample rate;
        // the shared coefficient builder has to clamp rather than blow up.
        dsp.set_sample_rate(22_050.0);
        let (l, r) = dsp.process_stereo(0.5, -0.5);
        assert!(l.is_finite() && r.is_finite());
    }

    /// A project saved before left/right, 6 dB/oct, ratio, mix and auto gain
    /// plays exactly as it did: the new fields load as their legacy values,
    /// and a static M/S chain is bit-identical to one built straight from
    /// the shared builder in the one domain the old chain ran in.
    #[test]
    fn legacy_state_plays_exactly_as_before() {
        let json = r#"{"version":1,"params":{"power":true,"outputDb":-2.0,"bands":[
            {"active":true,"bandType":"lowcut","freq":60.0,"gainDb":0.0,"q":1.0,"slope":24.0},
            {"active":true,"bandType":"bell","channel":"mid","freq":900.0,"gainDb":-4.0,"q":2.0},
            {"active":true,"bandType":"highshelf","channel":"side","freq":6000.0,"gainDb":5.0,"q":0.7},
            {"active":true,"bandType":"bell","freq":3000.0,"gainDb":2.0,"q":1.0,
             "dynamic":true,"thresholdDb":-30.0,"rangeDb":-6.0}
        ]}}"#;
        let state = ipc::EquzxState::from_json(json).expect("legacy json");
        assert_eq!(state.params.mix, 100.0);
        assert!(!state.params.auto_gain);
        // The listed bands keep the legacy law; the empty slots the list
        // leaves are fresh bands.
        assert!(
            state.params.bands[..4]
                .iter()
                .all(|band| band.ratio == RATIO_LEGACY)
        );
        assert!(
            state.params.bands[4..]
                .iter()
                .all(|band| band.ratio == DEFAULT_RATIO)
        );
        assert_eq!(state.params.bands[1].channel, BandChannel::Mid);

        // The dynamic band keeps the 6 dB knee.
        let dynamic = state.params.bands[3];
        assert_eq!(dynamic_fraction(&dynamic, -27.0), 0.5);
        assert_eq!(dynamic_fraction(&dynamic, -10.0), 1.0);

        let mut params = state.params;
        params.bands[3].dynamic = false;
        let section = |kind: &str, freq: f32, gain: f32, q: f32| {
            builtin_dsp_core::make_eq_biquad(kind, freq, gain, q, 48_000.0).expect("filter")
        };
        let q0 = butterworth_q(4, 0);
        let q1 = butterworth_q(4, 1);
        // Mid path: the cut, the mid bell, the stereo bell; side: the cut,
        // the shelf, the stereo bell — in band order, as the old chain ran.
        let mut mid = [
            section("highpass", 60.0, 0.0, q0),
            section("highpass", 60.0, 0.0, q1),
            section("bell", 900.0, -4.0, 2.0),
            section("bell", 3000.0, 2.0, 1.0),
        ];
        let mut side = [
            section("highpass", 60.0, 0.0, q0),
            section("highpass", 60.0, 0.0, q1),
            section("highshelf", 6000.0, 5.0, 0.7),
            section("bell", 3000.0, 2.0, 1.0),
        ];
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let gain = db_to_linear(-2.0);
        for n in 0..4_000 {
            let l = ((n as f32) * 0.037).sin() * 0.5;
            let r = ((n as f32) * 0.21).sin() * 0.3;
            let (mut a, mut b) = ((l + r) * 0.5, (l - r) * 0.5);
            for filter in &mut mid {
                a = filter.run(a);
            }
            for filter in &mut side {
                b = filter.run(b);
            }
            let expected = ((a + b) * gain, (a - b) * gain);
            assert_eq!(dsp.process_stereo(l, r), expected, "sample {n}");
        }
    }

    #[test]
    fn the_ratio_law_replaces_the_legacy_knee() {
        let mut band = BandParams {
            dynamic: true,
            threshold_db: -30.0,
            range_db: -12.0,
            ratio: 4.0,
            ..enabled_bell(1_000.0, 0.0, 1.0)
        };
        assert!((dynamic_offset_db(&band, -22.0) + 6.0).abs() < 1.0e-5);
        assert!((dynamic_offset_db(&band, 0.0) + 12.0).abs() < 1.0e-5);
        assert_eq!(dynamic_offset_db(&band, -40.0), 0.0);
        band.dyn_mode = DynMode::Below;
        assert!((dynamic_offset_db(&band, -34.0) + 6.0).abs() < 1.0e-5);
        band.ratio = RATIO_LEGACY;
        assert!((dynamic_offset_db(&band, -33.0) + 6.0).abs() < 1.0e-5);
    }

    #[test]
    fn a_six_db_slope_is_first_order() {
        let band = |kind: BandType| BandParams {
            active: true,
            band_type: kind,
            freq: 1_000.0,
            slope: 6.0,
            ..flat_band()
        };
        let low_cut = band(BandType::LowCut);
        assert_eq!(section_count(&low_cut), 1);
        let at = |band: &BandParams, hz: f32| band_response_db(band, hz, 48_000.0);
        assert!(
            (at(&low_cut, 1_000.0) + 3.01).abs() < 0.05,
            "{}",
            at(&low_cut, 1_000.0)
        );
        // Three octaves under: about −18 dB.
        assert!(
            (at(&low_cut, 125.0) + 18.1).abs() < 0.5,
            "{}",
            at(&low_cut, 125.0)
        );
        let high_cut = band(BandType::HighCut);
        assert!((at(&high_cut, 1_000.0) + 3.01).abs() < 0.05);
        assert!(at(&high_cut, 100.0).abs() < 0.1);

        // What plays is what is drawn.
        let mut params = default_params();
        params.bands[0] = low_cut;
        params.bands[1] = BandParams {
            freq: 8_000.0,
            ..high_cut
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        for hz in [123.0, 1_013.0, 8_011.0] {
            let heard = linear_to_db(level_at(&mut dsp, hz));
            let drawn = response_db(&params, BandChannel::Stereo, hz, 48_000.0);
            assert!((heard - drawn).abs() < 0.3, "{hz}: {heard} vs {drawn}");
        }
    }

    /// Plays `dsp` with a 1 kHz sine `(left, right)` scaled, and returns the
    /// worst deviation of each output channel from `(expect_l, expect_r)`
    /// times the input.
    fn channel_errors(dsp: &mut Dsp, gains: (f32, f32), expect: (f32, f32)) -> (f32, f32) {
        let step = std::f32::consts::TAU * 1_000.0 / 48_000.0;
        let mut worst = (0.0f32, 0.0f32);
        for i in 0..8_000 {
            let x = (i as f32 * step).sin();
            let (l, r) = dsp.process_stereo(x * gains.0, x * gains.1);
            if i > 4_000 {
                worst.0 = worst.0.max((l - x * expect.0).abs());
                worst.1 = worst.1.max((r - x * expect.1).abs());
            }
        }
        worst
    }

    #[test]
    fn left_and_right_bands_touch_their_own_channel_only() {
        for (channel, lit) in [(BandChannel::Left, 0), (BandChannel::Right, 1)] {
            let mut params = default_params();
            params.bands[0] = BandParams {
                channel,
                ..enabled_bell(1_000.0, 12.0, 1.0)
            };
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            assert!(!dsp.ms_mode);
            let errors = channel_errors(&mut dsp, (1.0, 1.0), (1.0, 1.0));
            let (moved, kept) = if lit == 0 {
                (errors.0, errors.1)
            } else {
                (errors.1, errors.0)
            };
            assert!(kept < 1.0e-3, "{channel:?}: other side moved {kept}");
            assert!(moved > 2.0, "{channel:?}: own side moved only {moved}");
        }
    }

    /// With left/right and mid/side bands together the chain splits into two
    /// stages, and each band still keeps to its part of the image.
    #[test]
    fn split_routing_keeps_every_band_to_its_part() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            channel: BandChannel::Left,
            ..enabled_bell(1_000.0, 12.0, 1.0)
        };
        params.bands[1] = BandParams {
            channel: BandChannel::Side,
            ..enabled_bell(1_000.0, 12.0, 1.0)
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        assert_eq!(dsp.routing, Routing::Split);
        assert_eq!((dsp.order_len, dsp.ms_order_len), (1, 1));
        // Input only on the right: the left band has nothing to act on, and
        // the side band sees side = −right/2.
        let errors = channel_errors(&mut dsp, (0.0, 1.0), (0.0, 1.0));
        assert!(
            errors.0 > 0.5 && errors.1 > 0.5,
            "side band must act: {errors:?}"
        );

        // A mid band with the left band: pure side input (x, −x) — the left
        // band boosts left, the mid band sees the imbalance only.
        params.bands[1].channel = BandChannel::Mid;
        params.bands[1].gain_db = 0.0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let errors = channel_errors(&mut dsp, (1.0, -1.0), (1.0, -1.0));
        assert!(
            errors.1 < 1.0e-3,
            "right untouched by a left band and a flat mid one"
        );
        assert!(errors.0 > 2.0);
    }

    #[test]
    fn auto_gain_cancels_a_broad_boost() {
        let mut params = default_params();
        params.bands[0] = BandParams {
            active: true,
            band_type: BandType::HighShelf,
            freq: 30.0,
            gain_db: 6.0,
            q: 0.7,
            ..flat_band()
        };
        params.auto_gain = true;
        let compensation = auto_gain_db(&params, 48_000.0);
        assert!(
            (-6.5..-4.5).contains(&compensation),
            "compensation {compensation}"
        );
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        let heard = linear_to_db(level_at(&mut dsp, 1_013.0));
        assert!(heard.abs() < 1.0, "auto gain left {heard} dB at 1 kHz");
        assert_eq!(dsp.telemetry()[telemetry::AUTO_GAIN_DB], compensation);

        // Off, or bypassed, it does nothing.
        params.auto_gain = false;
        assert_eq!(auto_gain_db(&params, 48_000.0), 0.0);
        assert!(dsp.apply_wire_param(ipc::AUTO_GAIN_INDEX, 0.0));
        let heard = linear_to_db(level_at(&mut dsp, 1_013.0));
        assert!((heard - 6.0).abs() < 0.5, "{heard}");
    }

    #[test]
    fn telemetry_reports_peaks_and_dynamics() {
        let mut params = default_params();
        params.bands[4] = BandParams {
            dynamic: true,
            threshold_db: -40.0,
            range_db: -12.0,
            attack_ms: 0.1,
            ratio: 4.0,
            ..enabled_bell(1_000.0, 0.0, 2.0)
        };
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let _ = level_at(&mut dsp, 1_000.0);
        let block = dsp.telemetry();
        assert!(block[telemetry::OUT_L] > 0.1 && block[telemetry::OUT_R] > 0.1);
        assert!(block[telemetry::DETECTOR_BASE + 4] > -40.0);
        assert!(block[telemetry::GAIN_BASE + 4] < -1.0);
        assert_eq!(block[telemetry::DETECTOR_BASE], telemetry::SILENT_DB);
        assert_eq!(block[telemetry::GAIN_BASE], 0.0);
        assert_eq!(block[telemetry::AUTO_GAIN_DB], 0.0);
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

    #[test]
    fn a_wide_q_designs_past_the_shared_ceiling() {
        let mut band = enabled_bell(750.0, 20.0, ipc::Q_MAX);
        let narrow = band_response_db(&band, 750.0 * 1.03, 48_000.0);
        band.q = 12.0;
        let capped = band_response_db(&band, 750.0 * 1.03, 48_000.0);
        assert!(narrow < capped - 0.5, "{narrow} vs {capped}");
        // And it still runs.
        let mut params = default_params();
        params.bands[0] = enabled_bell(1_000.0, ipc::GAIN_MAX_DB, ipc::Q_MAX);
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        let heard = linear_to_db(level_at(&mut dsp, 1_000.0));
        let drawn = response_db(&params, BandChannel::Stereo, 1_000.0, 48_000.0);
        assert!(
            heard.is_finite() && (heard - drawn).abs() < 1.0,
            "{heard} vs {drawn}"
        );
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

    /// Power off at block 0, then flipped every 30 blocks.
    fn toggle_power(dsp: &mut Dsp, block: usize) {
        if block.is_multiple_of(30) {
            let on = (block / 30) % 2 == 1;
            assert!(dsp.apply_wire_param(ipc::POWER_INDEX, if on { 1.0 } else { 0.0 }));
        }
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

    /// A low bell that changes the test tone, so wet and dry differ.
    fn shaping() -> Dsp {
        let mut params = default_params();
        params.bands[0] = enabled_bell(110.0, 9.0, 1.0);
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        dsp
    }

    #[test]
    fn dragging_output_does_not_zipper() {
        let mut dsp = shaping();
        let jump = drag(&mut dsp, 375, |dsp, block| {
            let db = sweep(ipc::OUTPUT_MIN_DB, ipc::OUTPUT_MAX_DB, block, 375);
            assert!(dsp.apply_wire_param(ipc::OUTPUT_INDEX, db));
        });
        assert!(jump < 4.0, "output drag jump x {jump}");
    }

    #[test]
    fn toggling_power_crossfades_without_a_step() {
        let mut dsp = shaping();
        let jump = drag(&mut dsp, 300, toggle_power);
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
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        let (l, _) = dsp.process_stereo(0.5, 0.5);
        assert!((l - 0.5 * db_to_linear(-12.0)).abs() < 1.0e-6, "{l}");
    }
}
