//! EQ-ZX — 24-band dynamic mid/side parametric EQ.
//!
//! The wide sibling of [`equz8`](../equz8/index.html): same wire/state pattern,
//! but each band additionally carries a stereo-image placement (stereo, mid or
//! side) and cut bands expand into a Butterworth cascade so slopes run from 12
//! to 96 dB/oct.
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

use biquad::{Biquad, DirectForm1};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, flush_denormal, linear_to_db, make_eq_biquad, make_eq_coefficients,
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
/// curve this produces.
const DYNAMIC_KNEE_DB: f32 = 6.0;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BandChannel {
    Stereo,
    Mid,
    Side,
}

impl BandChannel {
    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Stereo => 0.0,
            Self::Mid => 1.0,
            Self::Side => 2.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Mid,
            2 => Self::Side,
            _ => Self::Stereo,
        }
    }

    /// Does this band touch the first processing path (left, or mid)?
    const fn on_path_a(self) -> bool {
        matches!(self, Self::Stereo | Self::Mid)
    }

    /// Does this band touch the second processing path (right, or side)?
    const fn on_path_b(self) -> bool {
        matches!(self, Self::Stereo | Self::Side)
    }
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
    /// dB/oct for cut shapes; ignored by everything else. Only even filter
    /// orders exist, so this is always a multiple of 12.
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
}

fn solo_none() -> i32 {
    ipc::SOLO_NONE
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
    }
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
        ],
    }
}

/// Butterworth section Qs for a cascade of the given filter order.
///
/// Order `n` needs `n/2` sections, the k-th at `1 / (2 cos((2k+1)π / 2n))`.
/// Mirrors the editor's `butterworthQs`, which is what makes the drawn slope
/// and the audible slope the same filter.
fn butterworth_q(order: usize, k: usize) -> f32 {
    let angle = (2.0 * k as f32 + 1.0) * std::f32::consts::PI / (2.0 * order as f32);
    1.0 / (2.0 * angle.cos())
}

/// How many biquad sections a band expands into.
fn section_count(band: &BandParams) -> usize {
    if !band.band_type.is_cut() {
        return 1;
    }
    let order = (band.slope / 6.0).round().max(2.0) as usize;
    (order / 2).clamp(1, MAX_SECTIONS)
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
    let count = section_count(band);
    let order = count * 2;
    (0..count)
        .map(|section| {
            let q = if band.band_type.is_cut() {
                butterworth_q(order, section)
            } else {
                band.q
            };
            make_eq_coefficients(
                band.band_type.section_kind(),
                band.freq,
                gain_db,
                q,
                sample_rate,
            )
            .map_or(0.0, |coefficients| {
                biquad_response_db(&coefficients, freq_hz, sample_rate)
            })
        })
        .sum()
}

/// [`band_response_at_gain_db`] at the band's own gain.
pub fn band_response_db(band: &BandParams, freq_hz: f32, sample_rate: f32) -> f32 {
    band_response_at_gain_db(band, band.gain_db, freq_hz, sample_rate)
}

/// Whether any switched-on band acts on the mid or the side alone, which
/// splits the response into a mid and a side curve.
pub fn uses_mid_side(params: &Params) -> bool {
    params
        .bands
        .iter()
        .any(|band| band.active && band.channel != BandChannel::Stereo)
}

/// The EQ's response at `freq_hz`, in dB, for the part of the image
/// `channel` names, output gain included: a mid curve is every stereo and
/// mid band, a side curve every stereo and side band, and a stereo curve
/// (all bands stereo) every band. What the insert plays with no band moving
/// dynamically.
pub fn response_db(params: &Params, channel: BandChannel, freq_hz: f32, sample_rate: f32) -> f32 {
    params
        .bands
        .iter()
        .filter(|band| match channel {
            BandChannel::Stereo => true,
            BandChannel::Mid => band.channel.on_path_a(),
            BandChannel::Side => band.channel.on_path_b(),
        })
        .map(|band| band_response_db(band, freq_hz, sample_rate))
        .sum::<f32>()
        + params.output_db
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
    /// Active band slots in processing order, and how many of them there are.
    /// Precomputed on the control thread so the hot loop skips empty slots
    /// without touching them.
    order: [u8; BAND_COUNT],
    order_len: usize,
    /// Set when any active band is mid- or side-only, which puts the whole
    /// chain in the M/S domain for the duration.
    ms_mode: bool,
    /// Audition bandpass for the soloed band, one per path.
    solo_a: Option<DirectForm1<f32>>,
    solo_b: Option<DirectForm1<f32>>,
    solo_channel: BandChannel,
    /// Linear output gain, smoothed per sample.
    output_gain: Glide,
    /// Dry → processed share, following `params.power`.
    power: Glide,
    /// Fast-path flag: skip detector/envelope work when nothing needs it.
    any_dynamic: bool,
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
            ms_mode: false,
            solo_a: None,
            solo_b: None,
            solo_channel: BandChannel::Stereo,
            output_gain: Glide::new(1.0, GAIN_SMOOTHING_MS, sample_rate.max(1.0)),
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

    /// Replace every parameter (project restore). Output and power land on
    /// the restored values rather than gliding in from the old ones.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.rebuild();
        self.settle();
    }

    /// Put every smoother on its target.
    fn settle(&mut self) {
        self.output_gain.settle();
        self.power.settle();
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
            ipc::POWER_INDEX => self.power.set(power_share(self.params.power)),
            ipc::OUTPUT_INDEX => self.output_gain.set(db_to_linear(self.params.output_db)),
            ipc::SOLO_INDEX => self.rebuild_solo(),
            _ => {
                if let Some((band, field)) = ipc::decode_band_wire(wire_index) {
                    self.rebuild_band(band);
                    // Enabling, disabling or re-placing a band changes which
                    // paths are occupied, and mid/side changes the domain the
                    // whole chain runs in.
                    if matches!(field, ipc::BAND_ENABLED | ipc::BAND_CHANNEL) {
                        self.refresh_routing();
                    }
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
        self.output_gain
            .set_time(GAIN_SMOOTHING_MS, self.sample_rate);
        self.power.set_time(POWER_FADE_MS, self.sample_rate);
        self.output_gain.set(db_to_linear(self.params.output_db));
        self.power.set(power_share(self.params.power));
        for index in 0..BAND_COUNT {
            self.rebuild_band(index);
        }
        self.refresh_routing();
        self.rebuild_solo();
        self.refresh_any_dynamic();
    }

    /// Recompute the active-band list and the stereo domain. Control thread only.
    fn refresh_routing(&mut self) {
        self.order_len = 0;
        self.ms_mode = false;
        for (index, band) in self.params.bands.iter().enumerate() {
            if !band.active {
                continue;
            }
            self.order[self.order_len] = index as u8;
            self.order_len += 1;
            if band.channel != BandChannel::Stereo {
                self.ms_mode = true;
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
                make_eq_biquad("bandpass", band.freq, 0.0, q, self.sample_rate),
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
        let order = count * 2;

        for section in 0..MAX_SECTIONS {
            let filter = if section < count {
                let q = if band.band_type.is_cut() {
                    butterworth_q(order, section)
                } else {
                    band.q
                };
                make_eq_biquad(
                    band.band_type.section_kind(),
                    band.freq,
                    band.gain_db,
                    q,
                    self.sample_rate,
                )
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
            make_eq_biquad("bandpass", band.freq, 0.0, q, self.sample_rate)
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
        let Some(coeffs) = make_eq_coefficients(
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

        let over = match band.dyn_mode {
            DynMode::Above => level_db - band.threshold_db,
            DynMode::Below => band.threshold_db - level_db,
        };
        let target = clamp(over / DYNAMIC_KNEE_DB, 0.0, 1.0);

        let coeff = if target > state.envelope {
            state.attack_coeff
        } else {
            state.release_coeff
        };
        state.envelope = flush_denormal(coeff * state.envelope + (1.0 - coeff) * target);
        state.envelope * band.range_db
    }

    /// Run one band's cascade over whichever paths it occupies.
    #[inline]
    fn run_band(&mut self, index: usize, a: &mut f32, b: &mut f32, ms: bool) {
        let channel = self.params.bands[index].channel;
        // Outside M/S mode the paths are plain left and right, so every band
        // runs on both regardless of the placement it would have had.
        let run_a = !ms || channel.on_path_a();
        let run_b = !ms || channel.on_path_b();
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
        }
        for filter in self.solo_a.iter_mut().chain(self.solo_b.iter_mut()) {
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
            return (left, right);
        }
        let on = self.power.next();
        let gain = self.output_gain.next();
        let (wet_l, wet_r) = self.process_on(left, right, gain);
        if on == 1.0 {
            (wet_l, wet_r)
        } else {
            // A step from dry, so a chain that leaves the signal untouched
            // stays bit-exact through the fade.
            (left + (wet_l - left) * on, right + (wet_r - right) * on)
        }
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
            };
        }

        let ms = self.ms_mode;
        // Encode once for the whole chain rather than per band. Mono material
        // simply lands entirely in mid with side silent, which is correct.
        let (mut a, mut b) = if ms {
            ((left + right) * 0.5, (left - right) * 0.5)
        } else {
            (left, right)
        };

        for slot in 0..self.order_len {
            let index = self.order[slot] as usize;
            let band = self.params.bands[index];

            if self.any_dynamic
                && band.dynamic
                && band.band_type.has_gain()
                && band.range_db.abs() > 1.0e-6
            {
                // The detector listens to the paths the band actually acts on,
                // so a side-placed dynamic band reacts to the sides only.
                let detector_input = match (ms, band.channel) {
                    (true, BandChannel::Mid) => a,
                    (true, BandChannel::Side) => b,
                    _ => a.abs().max(b.abs()),
                };
                let delta = self.dynamic_delta_db(index, detector_input);
                let applied = band.gain_db + delta;
                if (applied - self.dyn_state[index].last_applied_db).abs() >= DYNAMIC_GAIN_EPS_DB {
                    self.set_band_applied_gain(index, applied);
                }
            }

            self.run_band(index, &mut a, &mut b, ms);
        }

        let (mut out_l, mut out_r) = if ms { (a + b, a - b) } else { (a, b) };
        out_l *= gain;
        out_r *= gain;
        (out_l, out_r)
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
