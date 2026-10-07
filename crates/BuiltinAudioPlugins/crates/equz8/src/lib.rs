//! Equz8 — 8-band parametric EQ.
//!
//! Phase 1 (easy). Filter coefficients and runtime state use the MIT/Apache
//! [`biquad`] crate. No DirectAudioEngine dependency.

use biquad::{Biquad, DirectForm1};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, biquad_response_db, clamp,
    db_to_linear, flush_denormal, linear_to_db, make_eq_biquad, make_eq_coefficients, mix,
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

/// Soft span (dB) from threshold to full dynamic amount. Pro-Q-style: the
/// envelope does not hard-switch at the threshold, it ramps over this window.
const DYNAMIC_SPAN_DB: f32 = 24.0;

/// Rebuild the peaking/shelf filter when applied gain moves by at least this
/// much — avoids per-sample coefficient work on a quiet envelope.
const DYNAMIC_GAIN_EPS_DB: f32 = 0.05;

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
    make_eq_coefficients(
        band.band_type.as_str(),
        band.freq,
        gain_db,
        band.q,
        sample_rate,
    )
    .map_or(0.0, |coefficients| {
        biquad_response_db(&coefficients, freq_hz, sample_rate)
    })
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
                min: -24.0,
                max: 12.0,
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
}

impl DynamicBandState {
    fn new() -> Self {
        Self {
            envelope: 0.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            last_applied_db: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    left: [Option<DirectForm1<f32>>; BAND_COUNT],
    right: [Option<DirectForm1<f32>>; BAND_COUNT],
    /// Band-local detector for dynamic EQ (bandpass around the band centre).
    detector: [Option<DirectForm1<f32>>; BAND_COUNT],
    dyn_state: [DynamicBandState; BAND_COUNT],
    /// Audition bandpass for the soloed band, per channel. `None` when no band
    /// is soloed — built on the control thread so the audio path only runs it.
    solo_left: Option<DirectForm1<f32>>,
    solo_right: Option<DirectForm1<f32>>,
    output_gain: f32,
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
            left: [None, None, None, None, None, None, None, None],
            right: [None, None, None, None, None, None, None, None],
            detector: [None, None, None, None, None, None, None, None],
            dyn_state: [DynamicBandState::new(); BAND_COUNT],
            solo_left: None,
            solo_right: None,
            output_gain: 1.0,
            any_dynamic: false,
        };
        dsp.rebuild();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.rebuild();
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
            ipc::POWER_INDEX => {}
            ipc::MIX_INDEX => {}
            ipc::OUTPUT_INDEX => {
                self.output_gain = db_to_linear(self.params.output_db);
            }
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
                }
            }
        }
        true
    }

    fn rebuild(&mut self) {
        self.output_gain = db_to_linear(self.params.output_db);
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
            make_eq_biquad("bandpass", band.freq, 0.0, q, self.sample_rate)
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
            self.left[index] = None;
            self.right[index] = None;
            self.detector[index] = None;
            return;
        }
        let applied = band.gain_db;
        let filter = make_eq_biquad(
            band.band_type.as_str(),
            band.freq,
            applied,
            band.q,
            self.sample_rate,
        );
        self.left[index] = filter;
        self.right[index] = filter;
        self.dyn_state[index].last_applied_db = applied;

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

    fn set_band_applied_gain(&mut self, index: usize, applied_db: f32) {
        let band = self.params.bands[index];
        let Some(coeffs) = make_eq_coefficients(
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
        if let Some(filter) = self.left[index].as_mut() {
            filter.update_coefficients(coeffs);
        }
        if let Some(filter) = self.right[index].as_mut() {
            filter.update_coefficients(coeffs);
        }
        self.dyn_state[index].last_applied_db = applied_db;
    }

    #[inline]
    fn dynamic_amount(env_db: f32, threshold_db: f32) -> f32 {
        let over = env_db - threshold_db;
        if over <= 0.0 {
            0.0
        } else {
            clamp(over / DYNAMIC_SPAN_DB, 0.0, 1.0)
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        for filter in self.left.iter_mut().flatten() {
            filter.reset_state();
        }
        for filter in self.right.iter_mut().flatten() {
            filter.reset_state();
        }
        for filter in self.detector.iter_mut().flatten() {
            filter.reset_state();
        }
        for state in &mut self.dyn_state {
            state.envelope = 0.0;
        }
        for filter in self.solo_left.iter_mut().chain(self.solo_right.iter_mut()) {
            filter.reset_state();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rebuild();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            return (left, right);
        }

        // Band solo replaces the chain rather than adding to it, and
        // deliberately ignores Mix: auditioning a band means hearing that
        // frequency region alone, so blending the dry signal back in would
        // defeat the point. Output level still applies so the audition can be
        // matched in loudness.
        if let (Some(solo_l), Some(solo_r)) = (self.solo_left.as_mut(), self.solo_right.as_mut()) {
            return (
                solo_l.run(left) * self.output_gain,
                solo_r.run(right) * self.output_gain,
            );
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
                    let amount = Self::dynamic_amount(env_db, band.threshold_db);
                    let applied = band.gain_db + amount * band.range_db;
                    if (applied - last).abs() >= DYNAMIC_GAIN_EPS_DB {
                        self.set_band_applied_gain(index, applied);
                    }
                }
                if let Some(filter) = self.left[index].as_mut() {
                    wet_l = filter.run(wet_l);
                }
                if let Some(filter) = self.right[index].as_mut() {
                    wet_r = filter.run(wet_r);
                }
            }
            wet_l *= self.output_gain;
            wet_r *= self.output_gain;
            let amount = self.params.mix / 100.0;
            return (mix(left, wet_l, amount), mix(right, wet_r, amount));
        }

        let mut wet_l = left;
        let mut wet_r = right;
        for filter in self.left.iter_mut().flatten() {
            wet_l = filter.run(wet_l);
        }
        for filter in self.right.iter_mut().flatten() {
            wet_r = filter.run(wet_r);
        }
        wet_l *= self.output_gain;
        wet_r *= self.output_gain;

        let amount = self.params.mix / 100.0;
        (mix(left, wet_l, amount), mix(right, wet_r, amount))
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
}
