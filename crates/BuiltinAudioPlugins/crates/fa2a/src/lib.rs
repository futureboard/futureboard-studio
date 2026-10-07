//! FA-2A — optical / program-dependent compressor (LA-2A-style).
//!
//! The dynamics stage models the broad behaviour of an electro-optical,
//! feedback leveling amplifier: a soft control curve, roughly 10 ms attack,
//! and a two-stage, program-dependent release.

use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    linear_to_db, mix, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

/// Editor-facing parameter id table, re-exported at the crate root so the host
/// resolves ids the same way for every built-in (`<plugin>::ui_param_index`).
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.fa2a";

/// Full scale. A sample at or beyond this latches the corresponding clip flag.
const CLIP_THRESHOLD: f32 = 1.0;

/// Integration window for the RMS meters. 300 ms is the VU standard, and the
/// editor draws a VU face — matching it here means the needle ballistics are
/// the meter's, not an arbitrary smoothing.
const RMS_WINDOW_SECONDS: f32 = 0.300;

/// Fall time for the peak meters. Rise is instantaneous.
const PEAK_FALL_SECONDS: f32 = 0.400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Compress,
    Limit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compress => "compress",
            Self::Limit => "limit",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "compress" | "comp" => Some(Self::Compress),
            "limit" => Some(Self::Limit),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Compress => 0.0,
            Self::Limit => 1.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        if value.round() as i32 == 1 {
            Self::Limit
        } else {
            Self::Compress
        }
    }
}

/// Input/output telemetry plus the gain reduction the optical cell is applying.
///
/// `gain_reduction_db` is positive: it is how many decibels the compressor is
/// taking off right now, which is what the editor's VU meter reads in its
/// GAIN REDUCTION position.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gain_reduction_db: f32,
    /// Set when an input sample reached full scale. Sticky until the editor
    /// calls [`Dsp::clear_clip`].
    pub in_clip: bool,
    /// Set when an output sample reached full scale. Sticky.
    pub out_clip: bool,
}

/// Meter state owned by the audio thread. Peak rises instantly and falls on a
/// one-pole; RMS keeps a running mean square and defers the `sqrt` to the
/// reader, so the hot path stays multiply-adds.
#[derive(Debug, Clone)]
struct Meters {
    in_peak: f32,
    out_peak: f32,
    in_ms: f32,
    out_ms: f32,
    rms_coeff: f32,
    peak_coeff: f32,
    in_clip: bool,
    out_clip: bool,
}

impl Meters {
    fn new(sample_rate: f32) -> Self {
        Self {
            in_peak: 0.0,
            out_peak: 0.0,
            in_ms: 0.0,
            out_ms: 0.0,
            rms_coeff: time_constant(sample_rate, RMS_WINDOW_SECONDS),
            peak_coeff: time_constant(sample_rate, PEAK_FALL_SECONDS),
            in_clip: false,
            out_clip: false,
        }
    }

    fn reset(&mut self) {
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip = false;
        self.out_clip = false;
    }

    #[inline]
    fn push(&mut self, input: (f32, f32), output: (f32, f32)) {
        let in_abs = input.0.abs().max(input.1.abs());
        let out_abs = output.0.abs().max(output.1.abs());

        self.in_peak = if in_abs >= self.in_peak {
            in_abs
        } else {
            self.in_peak * self.peak_coeff
        };
        self.out_peak = if out_abs >= self.out_peak {
            out_abs
        } else {
            self.out_peak * self.peak_coeff
        };

        let in_square = (input.0 * input.0 + input.1 * input.1) * 0.5;
        let out_square = (output.0 * output.0 + output.1 * output.1) * 0.5;
        self.in_ms = self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * in_square;
        self.out_ms = self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * out_square;

        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub peak_reduction: f32,
    pub gain_db: f32,
    pub mode: Mode,
    pub emphasis: f32,
    pub mix: f32,
    pub color: f32,
    pub sidechain_low_cut_hz: f32,
    pub output_trim_db: f32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        peak_reduction: 35.0,
        gain_db: 0.0,
        mode: Mode::Compress,
        emphasis: 45.0,
        mix: 100.0,
        color: 12.0,
        sidechain_low_cut_hz: 90.0,
        output_trim_db: 0.0,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OpticalModel {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_sec: f32,
    pub release_sec: f32,
    pub release_tail_sec: f32,
    /// High-frequency boost in the detector path.
    pub emphasis_db: f32,
}

pub fn optical_model_from_params(params: &Params) -> OpticalModel {
    let amount = clamp(params.peak_reduction, 0.0, 100.0) / 100.0;
    let emphasis = clamp(params.emphasis, 0.0, 100.0) / 100.0;
    let limit = params.mode == Mode::Limit;
    let threshold_db = clamp(
        peak_reduction_to_threshold_db(params.peak_reduction),
        -54.0,
        -3.0,
    );
    OpticalModel {
        threshold_db,
        ratio: if limit {
            12.0 + amount * 8.0
        } else {
            2.2 + amount * 1.6
        },
        knee_db: if limit {
            3.0 + (1.0 - amount) * 2.0
        } else {
            10.0 + (1.0 - amount) * 6.0
        },
        // A real T4 cell is not a fast peak limiter. Its attack is around
        // 10 ms; Limit is only slightly quicker and primarily changes the
        // control curve.
        attack_sec: if limit { 0.007 } else { 0.010 },
        // About half the recovery happens quickly, followed by a much longer
        // memory tail. The tail coefficient is further modulated by the
        // amount of reduction in `OpticalCell::process_stereo_linked`.
        release_sec: 0.060 + amount * 0.020,
        release_tail_sec: (if limit { 1.4 } else { 0.8 }) + amount * if limit { 6.6 } else { 4.2 },
        // The original emphasis network changes detector sensitivity with
        // frequency; it must not act as a global threshold offset.
        emphasis_db: emphasis * 10.0,
    }
}

pub fn peak_reduction_to_threshold_db(peak_reduction: f32) -> f32 {
    let t = clamp(peak_reduction, 0.0, 100.0) / 100.0;
    -8.0 - t.powf(1.18) * 38.0
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "FA-2A",
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
                id: "mode",
                name: "Mode",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "peakReduction",
                name: "Peak Reduction",
                default_value: 35.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "gainDb",
                name: "Gain",
                default_value: 0.0,
                min: -12.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "emphasis",
                name: "Emphasis",
                default_value: 45.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
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
                id: "color",
                name: "Color",
                default_value: 12.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "sidechainLowCutHz",
                name: "Sidechain HPF",
                default_value: 90.0,
                min: 20.0,
                max: 500.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "outputTrimDb",
                name: "Output Trim",
                default_value: 0.0,
                min: -12.0,
                max: 12.0,
                unit: "dB",
            },
        ],
    }
}

/// Stereo-linked feedback gain cell with T4-style two-stage recovery.
///
/// Unlike a conventional feed-forward peak compressor, the detector observes
/// the signal after the cell's current attenuation. That feedback topology is
/// a large part of the forgiving leveling behaviour associated with an LA-2A.
#[derive(Debug, Clone)]
struct OpticalCell {
    sample_rate: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    attack_coeff: f32,
    release_fast_coeff: f32,
    release_tail_short_coeff: f32,
    release_tail_long_coeff: f32,
    detector_attack_coeff: f32,
    detector_release_coeff: f32,
    detector_envelope: f32,
    fast_gr_db: f32,
    slow_gr_db: f32,
    sidechain_coeff: f32,
    sidechain_x1: [f32; 2],
    sidechain_y1: [f32; 2],
    emphasis_coeff: f32,
    emphasis_gain: f32,
    emphasis_low: [f32; 2],
}

impl OpticalCell {
    fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut cell = Self {
            sample_rate: sr,
            threshold_db: -18.0,
            ratio: 3.0,
            knee_db: 12.0,
            attack_coeff: 0.0,
            release_fast_coeff: 0.0,
            release_tail_short_coeff: 0.0,
            release_tail_long_coeff: 0.0,
            detector_attack_coeff: 0.0,
            detector_release_coeff: 0.0,
            detector_envelope: 0.0,
            fast_gr_db: 0.0,
            slow_gr_db: 0.0,
            sidechain_coeff: 0.0,
            sidechain_x1: [0.0; 2],
            sidechain_y1: [0.0; 2],
            emphasis_coeff: 0.0,
            emphasis_gain: 1.0,
            emphasis_low: [0.0; 2],
        };
        cell.set_model(
            OpticalModel {
                threshold_db: -18.0,
                ratio: 3.0,
                knee_db: 12.0,
                attack_sec: 0.010,
                release_sec: 0.060,
                release_tail_sec: 2.0,
                emphasis_db: 4.5,
            },
            90.0,
        );
        cell
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
    }

    fn set_model(&mut self, model: OpticalModel, sidechain_cutoff_hz: f32) {
        self.threshold_db = model.threshold_db;
        self.ratio = model.ratio.max(1.0);
        self.knee_db = model.knee_db.max(0.0);
        self.attack_coeff = time_constant(self.sample_rate, model.attack_sec);
        self.release_fast_coeff = time_constant(self.sample_rate, model.release_sec);
        self.release_tail_short_coeff =
            time_constant(self.sample_rate, model.release_tail_sec.max(0.25) * 0.35);
        self.release_tail_long_coeff =
            time_constant(self.sample_rate, model.release_tail_sec.max(0.25));
        self.detector_attack_coeff = time_constant(self.sample_rate, 0.001);
        self.detector_release_coeff = time_constant(self.sample_rate, 0.040);

        let cutoff = clamp(sidechain_cutoff_hz, 20.0, self.sample_rate * 0.45);
        self.sidechain_coeff = (-2.0 * std::f32::consts::PI * cutoff / self.sample_rate).exp();
        self.emphasis_coeff = 1.0 - (-std::f32::consts::TAU * 1_500.0 / self.sample_rate).exp();
        self.emphasis_gain = db_to_linear(model.emphasis_db);
    }

    fn reset(&mut self) {
        self.detector_envelope = 0.0;
        self.fast_gr_db = 0.0;
        self.slow_gr_db = 0.0;
        self.sidechain_x1 = [0.0; 2];
        self.sidechain_y1 = [0.0; 2];
        self.emphasis_low = [0.0; 2];
    }

    #[inline]
    fn gain_reduction_db(&self) -> f32 {
        // The fast half gives the cell its initial recovery; the slow half is
        // the phosphor memory that makes release program-dependent.
        self.fast_gr_db * 0.5 + self.slow_gr_db * 0.5
    }

    #[inline]
    fn high_pass(&mut self, input: f32, channel: usize) -> f32 {
        let output = self.sidechain_coeff
            * (self.sidechain_y1[channel] + input - self.sidechain_x1[channel]);
        self.sidechain_x1[channel] = input;
        self.sidechain_y1[channel] = output;
        output
    }

    #[inline]
    fn emphasize(&mut self, input: f32, channel: usize) -> f32 {
        self.emphasis_low[channel] += self.emphasis_coeff * (input - self.emphasis_low[channel]);
        let high = input - self.emphasis_low[channel];
        self.emphasis_low[channel] + high * self.emphasis_gain
    }

    #[inline]
    fn target_reduction_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let half_knee = self.knee_db * 0.5;
        let curved_over = if over <= -half_knee {
            0.0
        } else if over >= half_knee {
            over
        } else {
            let t = over + half_knee;
            t * t / (2.0 * self.knee_db.max(1.0e-6))
        };

        // In a feedback topology this is loop gain, not the usual
        // feed-forward `(1 - 1 / ratio)` slope. `ratio - 1` produces the
        // intended closed-loop compression ratio.
        curved_over * (self.ratio - 1.0)
    }

    #[inline]
    fn follow(current: f32, target: f32, rise_coeff: f32, fall_coeff: f32) -> f32 {
        let coeff = if target > current {
            rise_coeff
        } else {
            fall_coeff
        };
        coeff * current + (1.0 - coeff) * target
    }

    #[inline]
    fn process_stereo_linked(&mut self, left: f32, right: f32) -> (f32, f32) {
        // The detector is fed from immediately after the gain cell, before
        // makeup gain. Use the previous sample's cell gain to avoid an
        // algebraic loop while retaining feedback behaviour.
        let cell_gain = db_to_linear(-self.gain_reduction_db());
        let hp_l = self.high_pass(left * cell_gain, 0);
        let hp_r = self.high_pass(right * cell_gain, 1);
        let sc_l = self.emphasize(hp_l, 0).abs();
        let sc_r = self.emphasize(hp_r, 1).abs();
        let detected = sc_l.max(sc_r);
        let detector_coeff = if detected > self.detector_envelope {
            self.detector_attack_coeff
        } else {
            self.detector_release_coeff
        };
        self.detector_envelope =
            detector_coeff * self.detector_envelope + (1.0 - detector_coeff) * detected;

        let level_db = linear_to_db(self.detector_envelope.max(1.0e-12));
        let target_gr_db = self.target_reduction_db(level_db);
        self.fast_gr_db = Self::follow(
            self.fast_gr_db,
            target_gr_db,
            self.attack_coeff,
            self.release_fast_coeff,
        );

        // Deeper reduction leaves a longer optical memory. Interpolating the
        // already-computed coefficients keeps the sample path inexpensive.
        let memory = clamp(self.slow_gr_db / 18.0, 0.0, 1.0);
        let tail_coeff = self.release_tail_short_coeff
            + (self.release_tail_long_coeff - self.release_tail_short_coeff) * memory;
        self.slow_gr_db =
            Self::follow(self.slow_gr_db, target_gr_db, self.attack_coeff, tail_coeff);

        let gain = db_to_linear(-self.gain_reduction_db());
        (left * gain, right * gain)
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    compressor: OpticalCell,
    output_gain: f32,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            params: default_params(),
            compressor: OpticalCell::new(sr),
            output_gain: 1.0,
            meters: Meters::new(sr),
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.compressor.gain_reduction_db()
    }

    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
    }

    /// Full telemetry for the editor's VU meter: input/output levels, the
    /// gain reduction the cell is applying right now, and the sticky clip
    /// latches.
    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            // Reported even while bypassed would be a lie: the cell is not in
            // the path, so it is taking nothing off.
            gain_reduction_db: if self.params.power {
                self.compressor.gain_reduction_db().max(0.0)
            } else {
                0.0
            },
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    /// Clear the sticky clip indicators (editor click-to-reset).
    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// Apply a compact wire update already resolved by the UI/control thread.
    ///
    /// The audio path never parses JSON or looks up string parameter ids. Every
    /// arm recomputes only the optical model and its coefficients, so this
    /// stays allocation-free and safe to call from the producer thread between
    /// blocks.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        // Every continuous parameter feeds the optical model or its detector
        // filters, so there is nothing to gain from fanning out per index.
        self.apply_params();
        true
    }

    /// Resolve a string id off the realtime path (project restore, tests).
    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// The feedback cell uses its previous gain state rather than lookahead,
    /// so it adds no latency for the graph to compensate.
    pub fn latency_samples(&self) -> usize {
        0
    }

    fn apply_params(&mut self) {
        let model = optical_model_from_params(&self.params);
        self.compressor
            .set_model(model, self.params.sidechain_low_cut_hz);
        self.output_gain = db_to_linear(self.params.gain_db + self.params.output_trim_db);
    }

    #[inline]
    fn apply_color(sample: f32, drive: f32) -> f32 {
        if drive <= 0.0 {
            return sample;
        }
        // Gentle transformer/tube curvature without a tanh ceiling. The old
        // normalized tanh stage raised low-level gain and flattened peaks,
        // which made FA-2A behave more like a soft clipper than a compressor.
        let squared = sample * sample;
        sample - (0.18 * drive) * sample * squared / (1.0 + squared)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.compressor.reset();
        self.meters.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.compressor.set_sample_rate(sr);
        self.meters = Meters::new(sr);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        // Metered on both sides of the cell even while bypassed: the editor's
        // meter is how you set gain staging *before* engaging it.
        if !self.params.power {
            self.meters.push((left, right), (left, right));
            return (left, right);
        }
        let (mut wet_l, mut wet_r) = self.compressor.process_stereo_linked(left, right);
        let drive = self.params.color / 100.0;
        wet_l = Self::apply_color(wet_l, drive) * self.output_gain;
        wet_r = Self::apply_color(wet_r, drive) * self.output_gain;
        let amount = self.params.mix / 100.0;
        let out_l = mix(left, wet_l, amount);
        let out_r = mix(right, wet_r, amount);
        self.meters.push((left, right), (out_l, out_r));
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the compressor with a steady tone loud enough to be over the
    /// threshold, then read its telemetry.
    fn run_tone(dsp: &mut Dsp, amplitude: f32, samples: usize) -> MeterFrame {
        for n in 0..samples {
            let x = (n as f32 * 0.05).sin() * amplitude;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
        }
        dsp.meter_frame()
    }

    fn run_sine(dsp: &mut Dsp, amplitude: f32, frequency: f32, samples: usize) -> MeterFrame {
        let increment = std::f32::consts::TAU * frequency / 48_000.0;
        for n in 0..samples {
            let x = (n as f32 * increment).sin() * amplitude;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
        }
        dsp.meter_frame()
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.category, PluginCategory::Effect);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len(), "duplicate parameter id in descriptor");

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (param.default_value - actual).abs() < 1.0e-6,
                "`{}`: descriptor says {}, default_params() says {actual}",
                param.id,
                param.default_value,
            );
            assert!(
                param.default_value >= param.min && param.default_value <= param.max,
                "`{}`: default {} is outside {}..{}",
                param.id,
                param.default_value,
                param.min,
                param.max,
            );
        }
    }

    #[test]
    fn optical_model_limit_is_harder() {
        let mut compress = default_params();
        compress.mode = Mode::Compress;
        let mut limit = default_params();
        limit.mode = Mode::Limit;
        let c = optical_model_from_params(&compress);
        let l = optical_model_from_params(&limit);
        assert!(l.ratio > c.ratio);
        assert!(l.knee_db < c.knee_db);
    }

    #[test]
    fn emphasis_weights_treble_without_moving_the_global_threshold() {
        let mut flat_params = default_params();
        flat_params.peak_reduction = 55.0;
        flat_params.emphasis = 0.0;
        flat_params.sidechain_low_cut_hz = 20.0;
        flat_params.color = 0.0;

        let mut emphasized_params = flat_params.clone();
        emphasized_params.emphasis = 100.0;
        assert_eq!(
            optical_model_from_params(&flat_params).threshold_db,
            optical_model_from_params(&emphasized_params).threshold_db
        );

        let mut flat = Dsp::new(48_000.0);
        flat.set_params(flat_params);
        let flat_gr = run_sine(&mut flat, 0.12, 8_000.0, 48_000).gain_reduction_db;

        let mut emphasized = Dsp::new(48_000.0);
        emphasized.set_params(emphasized_params);
        let emphasized_gr = run_sine(&mut emphasized, 0.12, 8_000.0, 48_000).gain_reduction_db;
        assert!(
            emphasized_gr > flat_gr + 2.0,
            "emphasis must increase treble detection: {flat_gr} vs {emphasized_gr}"
        );
    }

    #[test]
    fn processes_audio() {
        let mut dsp = Dsp::new(48_000.0);
        let (l, r) = dsp.process_stereo(0.8, -0.8);
        assert!(l.is_finite() && r.is_finite());
    }

    #[test]
    fn bypass_when_power_off() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.power = false;
        dsp.set_params(params);
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }

    /// The editor's VU meter reads this number directly, so it has to be a
    /// real measurement: nothing over the threshold means no reduction, and a
    /// hot signal means a positive amount that grows with peak reduction.
    #[test]
    fn gain_reduction_tracks_the_signal_and_the_control() {
        let mut quiet = Dsp::new(48_000.0);
        let frame = run_tone(&mut quiet, 0.002, 24_000);
        assert!(
            frame.gain_reduction_db < 1.0,
            "a signal under the threshold must not read as reduction: {}",
            frame.gain_reduction_db
        );

        let mut gentle = Dsp::new(48_000.0);
        let mut params = default_params();
        params.peak_reduction = 20.0;
        gentle.set_params(params.clone());
        let gentle_gr = run_tone(&mut gentle, 0.6, 48_000).gain_reduction_db;

        let mut hard = Dsp::new(48_000.0);
        params.peak_reduction = 90.0;
        hard.set_params(params);
        let hard_gr = run_tone(&mut hard, 0.6, 48_000).gain_reduction_db;

        assert!(gentle_gr > 0.0, "a hot signal must read some reduction");
        assert!(
            hard_gr > gentle_gr + 1.0,
            "more peak reduction must read more: {gentle_gr} vs {hard_gr}"
        );
    }

    /// Bypassed, the cell is out of the path — reporting reduction would put a
    /// needle on a meter for processing that is not happening.
    #[test]
    fn bypassed_reports_no_gain_reduction_but_still_meters_level() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.power = false;
        dsp.set_params(params);
        let frame = run_tone(&mut dsp, 0.6, 24_000);
        assert_eq!(frame.gain_reduction_db, 0.0);
        assert!(frame.in_rms > 0.05, "input must still be measured");
        assert!(frame.out_rms > 0.05, "output must still be measured");
    }

    #[test]
    fn meters_measure_both_sides_and_latch_clipping() {
        let mut dsp = Dsp::new(48_000.0);
        let frame = run_tone(&mut dsp, 0.5, 24_000);
        assert!(frame.in_peak > 0.4 && frame.in_peak <= 1.0);
        assert!(frame.in_rms > 0.0 && frame.in_rms < frame.in_peak + 1.0e-6);
        assert!(frame.out_peak > 0.0);
        assert!(!frame.in_clip && !frame.out_clip);

        let _ = dsp.process_stereo(1.5, 1.5);
        assert!(dsp.meter_frame().in_clip, "clip flag should latch");
        dsp.clear_clip();
        assert!(!dsp.meter_frame().in_clip);
        assert!(!dsp.meter_frame().out_clip);
    }

    #[test]
    fn meters_do_not_cancel_antiphase_stereo() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.power = false;
        dsp.set_params(params);
        for _ in 0..24_000 {
            let _ = dsp.process_stereo(0.5, -0.5);
        }
        let frame = dsp.meter_frame();
        assert!(frame.in_peak >= 0.5);
        assert!(frame.in_rms > 0.3);
        assert!(frame.out_peak >= 0.5);
        assert!(frame.out_rms > 0.3);
    }

    #[test]
    fn reset_clears_the_meters() {
        let mut dsp = Dsp::new(48_000.0);
        let _ = run_tone(&mut dsp, 0.7, 4_800);
        dsp.reset();
        let frame = dsp.meter_frame();
        assert_eq!(frame.in_peak, 0.0);
        assert_eq!(frame.in_rms, 0.0);
        assert_eq!(frame.out_peak, 0.0);
        assert_eq!(frame.out_rms, 0.0);
    }

    #[test]
    fn processes_finite_at_multiple_rates() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
            let mut dsp = Dsp::new(sr);
            let frame = run_tone(&mut dsp, 0.8, sr as usize / 4);
            assert!(frame.in_rms.is_finite() && frame.out_rms.is_finite());
            assert!(frame.gain_reduction_db.is_finite());
        }
    }

    #[test]
    fn optical_release_has_a_fast_recovery_and_a_slow_memory_tail() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.peak_reduction = 75.0;
        params.color = 0.0;
        dsp.set_params(params);

        let driven = run_tone(&mut dsp, 0.8, 48_000).gain_reduction_db;
        for _ in 0..4_800 {
            let _ = dsp.process_stereo(0.0, 0.0);
        }
        let after_100_ms = dsp.gain_reduction_db();
        for _ in 0..43_200 {
            let _ = dsp.process_stereo(0.0, 0.0);
        }
        let after_one_second = dsp.gain_reduction_db();

        assert!(driven > 6.0, "test signal must drive the cell: {driven}");
        assert!(
            after_100_ms < driven && after_100_ms > driven * 0.2,
            "the fast half should recover without erasing optical memory: \
             {driven} -> {after_100_ms}"
        );
        assert!(
            after_one_second < after_100_ms && after_one_second > 0.1,
            "the slow phosphor tail should keep recovering after one second: \
             {after_100_ms} -> {after_one_second}"
        );
    }

    #[test]
    fn color_curve_preserves_small_signal_gain_and_has_no_clip_ceiling() {
        let quiet = Dsp::apply_color(0.001, 1.0);
        assert!(
            (quiet - 0.001).abs() < 1.0e-8,
            "color must not act like normalized tanh makeup: {quiet}"
        );

        let one = Dsp::apply_color(1.0, 1.0);
        let two = Dsp::apply_color(2.0, 1.0);
        assert!(
            two > one * 1.5,
            "color must not flatten into a clip ceiling"
        );
        assert!(
            two > 1.0,
            "signals above full scale must not be hard bounded"
        );
    }

    #[test]
    fn wire_update_changes_only_authoritative_params() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_wire_param(ipc::PEAK_REDUCTION_INDEX, 80.0));
        assert_eq!(dsp.params().peak_reduction, 80.0);
        assert!(dsp.apply_ui_param("mode", Mode::Limit.to_wire()));
        assert_eq!(dsp.params().mode, Mode::Limit);
        assert!(!dsp.apply_wire_param(u32::MAX, 0.0));
        assert!(!dsp.apply_wire_param(ipc::GAIN_INDEX, f32::NAN));
    }
}
