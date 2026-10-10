//! FA-76 — FET / ultra-fast compressor (1176-style).
//!
//! Ratio buttons map to classic 4 / 8 / 12 / 20 / All curves. The gain cell
//! uses the feedback topology and sub-millisecond timing of the hardware.

use builtin_dsp_core::delay::{Smoothed, smoothing_step};
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

pub const PLUGIN_ID: &str = "futureboard.fa76";

/// Full scale. A sample at or beyond this latches the corresponding clip flag.
const CLIP_THRESHOLD: f32 = 1.0;

/// Integration window for the RMS meters. 300 ms is the VU standard, and the
/// editor draws a VU face — matching it here means the needle ballistics are
/// the meter's, not an arbitrary smoothing.
const RMS_WINDOW_SECONDS: f32 = 0.300;

/// Fall time for the peak meters. Rise is instantaneous.
const PEAK_FALL_SECONDS: f32 = 0.400;

/// Input, output and mix glide over this long, so a dragged knob — which
/// arrives as one new value per block — does not step the level at every
/// block edge.
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

/// Ratio pushbuttons on the FET faceplate. Wire order is the persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RatioButton {
    R4,
    R8,
    R12,
    R20,
    /// "All buttons in" — aggressive limiting curve.
    All,
}

impl RatioButton {
    pub fn ratio(self) -> f32 {
        match self {
            Self::R4 => 4.0,
            Self::R8 => 8.0,
            Self::R12 => 12.0,
            Self::R20 => 20.0,
            // The all-buttons curve is not an infinite brick-wall ratio. Its
            // aggression also comes from the shifted knee and faster recovery.
            Self::All => 30.0,
        }
    }

    pub fn knee_db(self) -> f32 {
        match self {
            Self::R4 => 4.0,
            Self::R8 => 3.0,
            Self::R12 => 2.0,
            Self::R20 => 1.0,
            Self::All => 8.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::R4 => "r4",
            Self::R8 => "r8",
            Self::R12 => "r12",
            Self::R20 => "r20",
            Self::All => "all",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "r4" | "4" => Some(Self::R4),
            "r8" | "8" => Some(Self::R8),
            "r12" | "12" => Some(Self::R12),
            "r20" | "20" => Some(Self::R20),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::R4 => 0.0,
            Self::R8 => 1.0,
            Self::R12 => 2.0,
            Self::R20 => 3.0,
            Self::All => 4.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::R8,
            2 => Self::R12,
            3 => Self::R20,
            4 => Self::All,
            _ => Self::R4,
        }
    }
}

/// Input/output telemetry plus the gain reduction the FET cell is applying.
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
    pub input_db: f32,
    pub output_db: f32,
    pub attack_us: f32,
    pub release_ms: f32,
    pub ratio: RatioButton,
    pub mix: f32,
    pub sidechain_hpf_hz: f32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        input_db: 18.0,
        output_db: -12.0,
        attack_us: 20.0,
        release_ms: 100.0,
        ratio: RatioButton::R4,
        mix: 100.0,
        sidechain_hpf_hz: 60.0,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "FA-76",
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
                id: "ratio",
                name: "Ratio",
                default_value: 0.0,
                min: 0.0,
                max: 4.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "inputDb",
                name: "Input",
                default_value: 18.0,
                min: -12.0,
                max: 36.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "outputDb",
                name: "Output",
                default_value: -12.0,
                min: -36.0,
                max: 12.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "attackUs",
                name: "Attack",
                default_value: 20.0,
                min: 20.0,
                max: 800.0,
                unit: "µs",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 100.0,
                min: 50.0,
                max: 1_100.0,
                unit: "ms",
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
                id: "sidechainHpfHz",
                name: "Sidechain HPF",
                default_value: 60.0,
                min: 0.0,
                max: 500.0,
                unit: "Hz",
            },
        ],
    }
}

/// Stereo-linked feedback FET gain cell.
///
/// The static curve is the closed-form solution of the hardware feedback loop;
/// solving it directly avoids the unstable one-sample control-loop delay that
/// a literal digital feedback model creates at 20 µs attack and high ratios.
/// Each sidechain channel is high-passed before rectification: filtering an
/// already-rectified stereo maximum would turn bass into DC/harmonics and make
/// the sidechain HPF ineffective.
#[derive(Debug, Clone)]
struct FetCell {
    sample_rate: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
    gain_reduction_db: f32,
    sidechain_coeff: f32,
    sidechain_x1: [f32; 2],
    sidechain_y1: [f32; 2],
}

impl FetCell {
    fn new(sample_rate: f32) -> Self {
        let mut cell = Self {
            sample_rate: sample_rate.max(1.0),
            threshold_db: -24.0,
            ratio: 4.0,
            knee_db: 4.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            gain_reduction_db: 0.0,
            sidechain_coeff: 0.0,
            sidechain_x1: [0.0; 2],
            sidechain_y1: [0.0; 2],
        };
        cell.set_model(-24.0, 4.0, 4.0, 20.0, 100.0, 60.0);
        cell
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
    }

    fn set_model(
        &mut self,
        threshold_db: f32,
        ratio: f32,
        knee_db: f32,
        attack_us: f32,
        release_ms: f32,
        sidechain_hpf_hz: f32,
    ) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.knee_db = knee_db.max(0.0);
        self.attack_coeff = time_constant(self.sample_rate, attack_us * 1.0e-6);
        self.release_coeff = time_constant(self.sample_rate, release_ms * 0.001);
        let cutoff = clamp(sidechain_hpf_hz, 0.0, self.sample_rate * 0.45);
        self.sidechain_coeff = if cutoff < 10.0 {
            0.0
        } else {
            (-std::f32::consts::TAU * cutoff / self.sample_rate).exp()
        };
    }

    fn reset(&mut self) {
        self.gain_reduction_db = 0.0;
        self.sidechain_x1 = [0.0; 2];
        self.sidechain_y1 = [0.0; 2];
    }

    #[inline]
    fn high_pass(&mut self, input: f32, channel: usize) -> f32 {
        if self.sidechain_coeff == 0.0 {
            return input;
        }
        let output = self.sidechain_coeff
            * (self.sidechain_y1[channel] + input - self.sidechain_x1[channel]);
        self.sidechain_x1[channel] = input;
        self.sidechain_y1[channel] = output;
        output
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
        // Closed-form gain reduction for the selected input/output ratio. It
        // is equivalent to solving the static feedback loop without inserting
        // a destabilising sample of delay into that loop.
        curved_over * (1.0 - 1.0 / self.ratio)
    }

    #[inline]
    fn process_stereo_linked(&mut self, left: f32, right: f32) -> (f32, f32) {
        let sc_l = self.high_pass(left, 0).abs();
        let sc_r = self.high_pass(right, 1).abs();
        let level_db = linear_to_db(sc_l.max(sc_r).max(1.0e-12));
        let target = self.target_reduction_db(level_db);
        let coeff = if target > self.gain_reduction_db {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain_reduction_db = coeff * self.gain_reduction_db + (1.0 - coeff) * target;
        let gain = db_to_linear(-self.gain_reduction_db);
        (left * gain, right * gain)
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    compressor: FetCell,
    /// Linear input and output gain, the input stage's drive (it follows
    /// Input) and the wet share, all glided per sample.
    input_gain: Glide,
    output_gain: Glide,
    input_color: Glide,
    mix: Glide,
    /// Dry → processed share, following `params.power`.
    power: Glide,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            params: default_params(),
            compressor: FetCell::new(sr),
            input_gain: Glide::new(1.0, GAIN_SMOOTHING_MS, sr),
            output_gain: Glide::new(1.0, GAIN_SMOOTHING_MS, sr),
            input_color: Glide::new(0.0, GAIN_SMOOTHING_MS, sr),
            mix: Glide::new(1.0, GAIN_SMOOTHING_MS, sr),
            power: Glide::new(1.0, POWER_FADE_MS, sr),
            meters: Meters::new(sr),
        };
        dsp.apply_params();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.compressor.gain_reduction_db
    }

    /// Replace every parameter (project restore). Gains and power land on the
    /// restored values rather than gliding in from the old ones.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
        self.settle();
    }

    /// Put every glide on its target.
    fn settle(&mut self) {
        self.input_gain.settle();
        self.output_gain.settle();
        self.input_color.settle();
        self.mix.settle();
        self.power.settle();
    }

    /// Full telemetry for the editor's VU meter: input/output levels, the
    /// gain reduction the FET is applying right now, and the sticky clip
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
                self.compressor.gain_reduction_db.max(0.0)
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
    /// arm recomputes only the FET model and its coefficients, so this stays
    /// allocation-free and safe to call from the producer thread between blocks.
    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
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

    /// FA-76 solves its feedback curve without lookahead, so it adds no
    /// latency for the graph to compensate.
    pub fn latency_samples(&self) -> usize {
        0
    }

    fn apply_params(&mut self) {
        // FET units are threshold-fixed; "input" drives into a fixed knee.
        let all_buttons = self.params.ratio == RatioButton::All;
        let threshold_db = if all_buttons { -26.0 } else { -24.0 };
        let attack_us = self.params.attack_us * if all_buttons { 0.65 } else { 1.0 };
        let release_ms = self.params.release_ms * if all_buttons { 0.55 } else { 1.0 };
        self.compressor.set_model(
            threshold_db,
            self.params.ratio.ratio(),
            self.params.ratio.knee_db(),
            attack_us,
            release_ms,
            self.params.sidechain_hpf_hz,
        );
        let sample_rate = self.compressor.sample_rate;
        for glide in [
            &mut self.input_gain,
            &mut self.output_gain,
            &mut self.input_color,
            &mut self.mix,
        ] {
            glide.set_time(GAIN_SMOOTHING_MS, sample_rate);
        }
        self.power.set_time(POWER_FADE_MS, sample_rate);
        self.input_gain.set(db_to_linear(self.params.input_db));
        self.output_gain.set(db_to_linear(self.params.output_db));
        self.input_color
            .set(0.03 + 0.09 * ((self.params.input_db + 12.0) / 48.0));
        self.mix.set(self.params.mix / 100.0);
        self.power.set(power_share(self.params.power));
    }

    /// Gentle input-amplifier/transformer curvature. The transfer remains
    /// monotonic and keeps unity small-signal gain, avoiding an aliased hard
    /// clip while still adding the odd harmonics expected when Input is driven.
    #[inline]
    fn apply_input_stage(sample: f32, color: f32) -> f32 {
        let squared = sample * sample;
        sample - color * sample * squared / (1.0 + squared)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.compressor.reset();
        self.meters.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.compressor.set_sample_rate(sr);
        self.meters = Meters::new(sr);
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        // Metered on both sides of the cell even while bypassed: the editor's
        // meter is how you set gain staging *before* engaging it. Fully off
        // and settled, the cell does not run.
        if self.power.is_settled() && !self.params.power {
            self.meters.push((left, right), (left, right));
            return (left, right);
        }
        let on = self.power.next();
        let input_gain = self.input_gain.next();
        let color = self.input_color.next();
        let output_gain = self.output_gain.next();
        let amount = self.mix.next();
        let driven_l = Self::apply_input_stage(left * input_gain, color);
        let driven_r = Self::apply_input_stage(right * input_gain, color);
        let (mut wet_l, mut wet_r) = self.compressor.process_stereo_linked(driven_l, driven_r);
        wet_l *= output_gain;
        wet_r *= output_gain;
        let mut out_l = mix(left, wet_l, amount);
        let mut out_r = mix(right, wet_r, amount);
        if on != 1.0 {
            // Power fading: a step from the untouched input.
            out_l = left + (out_l - left) * on;
            out_r = right + (out_r - right) * on;
        }
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
    fn all_buttons_has_highest_ratio() {
        assert!(RatioButton::All.ratio() > RatioButton::R20.ratio());
    }

    #[test]
    fn all_buttons_is_more_aggressive_than_twenty_to_one() {
        let mut params = default_params();
        params.input_db = 12.0;
        params.output_db = -12.0;
        params.ratio = RatioButton::R20;
        let mut r20 = Dsp::new(48_000.0);
        r20.set_params(params.clone());
        let r20_gr = run_tone(&mut r20, 0.35, 24_000).gain_reduction_db;

        params.ratio = RatioButton::All;
        let mut all = Dsp::new(48_000.0);
        all.set_params(params);
        let all_gr = run_tone(&mut all, 0.35, 24_000).gain_reduction_db;

        assert!(
            all_gr > r20_gr + 1.0,
            "all-buttons must clamp harder: {r20_gr} vs {all_gr}"
        );
    }

    #[test]
    fn input_stage_preserves_small_signals_and_stays_monotonic() {
        let quiet = Dsp::apply_input_stage(0.001, 0.12);
        assert!((quiet - 0.001).abs() < 1.0e-8);

        let one = Dsp::apply_input_stage(1.0, 0.12);
        let two = Dsp::apply_input_stage(2.0, 0.12);
        assert!(two > one);
        assert!(two < 2.0, "driven input stage must add curvature");
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
                (actual - param.default_value).abs() < 1.0e-5,
                "{} default drifted: descriptor={} ui_values={}",
                param.id,
                param.default_value,
                actual
            );
        }
    }

    #[test]
    fn fast_attack_compresses() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.ratio = RatioButton::R20;
        params.attack_us = 20.0;
        params.input_db = 24.0;
        params.output_db = -18.0;
        dsp.set_params(params);
        let frame = run_tone(&mut dsp, 0.8, 2_000);
        assert!(frame.gain_reduction_db > 3.0);
        assert!(
            frame.gain_reduction_db < 60.0,
            "control loop over-compressed: {} dB",
            frame.gain_reduction_db
        );
        assert!(frame.in_rms > 0.0);
    }

    #[test]
    fn sidechain_hpf_rejects_bass_before_rectification() {
        let mut flat = Dsp::new(48_000.0);
        let mut params = default_params();
        params.input_db = 18.0;
        params.sidechain_hpf_hz = 0.0;
        flat.set_params(params.clone());
        let flat_gr = run_sine(&mut flat, 0.25, 40.0, 48_000).gain_reduction_db;

        let mut filtered = Dsp::new(48_000.0);
        params.sidechain_hpf_hz = 500.0;
        filtered.set_params(params);
        let filtered_gr = run_sine(&mut filtered, 0.25, 40.0, 48_000).gain_reduction_db;

        assert!(
            flat_gr > 3.0,
            "test tone must drive the detector: {flat_gr}"
        );
        assert!(
            filtered_gr + 3.0 < flat_gr,
            "HPF must reduce bass-driven compression: {flat_gr} vs {filtered_gr}"
        );
    }

    #[test]
    fn wire_param_moves_ratio_and_meters_stay_finite() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("ratio", RatioButton::All.to_wire()));
        assert_eq!(dsp.params().ratio, RatioButton::All);
        let frame = run_tone(&mut dsp, 0.5, 512);
        assert!(frame.out_peak.is_finite());
        assert!(frame.gain_reduction_db.is_finite());
    }

    #[test]
    fn power_off_reports_zero_reduction() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.input_db = 30.0;
        params.ratio = RatioButton::R20;
        dsp.set_params(params);
        let _ = run_tone(&mut dsp, 0.9, 1_000);
        assert!(dsp.apply_ui_param("power", 0.0));
        let frame = run_tone(&mut dsp, 0.9, 256);
        assert_eq!(frame.gain_reduction_db, 0.0);
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

    #[test]
    fn dragging_input_output_or_mix_does_not_zipper() {
        for (index, min, max) in [
            (ipc::INPUT_INDEX, -12.0, 36.0),
            (ipc::OUTPUT_INDEX, -36.0, 12.0),
            (ipc::MIX_INDEX, 0.0, 100.0),
        ] {
            let mut dsp = Dsp::new(48_000.0);
            let jump = drag(&mut dsp, 375, |dsp, block| {
                assert!(dsp.apply_wire_param(index, sweep(min, max, block, 375)));
            });
            assert!(jump < 4.0, "wire {index} drag jump x {jump}");
        }
    }

    #[test]
    fn toggling_power_crossfades_without_a_step() {
        let mut dsp = Dsp::new(48_000.0);
        let jump = drag(&mut dsp, 300, toggle_power);
        assert!(jump < 4.0, "power toggle jump x {jump}");
        // Settled off is the input exactly.
        assert!(dsp.apply_wire_param(ipc::POWER_INDEX, 0.0));
        let _ = run_tone(&mut dsp, 0.5, 4_800);
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }

    /// A project load lands on its values; it does not fade them in.
    #[test]
    fn restored_state_starts_settled() {
        let mut params = default_params();
        params.input_db = 0.0;
        params.output_db = 6.0;
        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params.clone());
        // Far under the knee: only the output gain applies.
        let x = 1.0e-4;
        let (l, _) = dsp.process_stereo(x, x);
        assert!((l - x * db_to_linear(6.0)).abs() < 1.0e-9, "{l}");
        params.power = false;
        dsp.set_params(params);
        assert_eq!(dsp.process_stereo(0.25, -0.25), (0.25, -0.25));
    }
}
