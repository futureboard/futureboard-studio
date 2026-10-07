//! BurnLimit — modern brickwall limiter (Pro-L / Ozone–inspired).
//!
//! Lookahead peak limiting with style curves and a 4× cubic inter-sample peak
//! detector. The audio path remains allocation-free and bounded.

use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    linear_to_db, mix, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.burnlimit";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;

/// Maximum lookahead buffer. Covers the declared 10 ms range through 384 kHz.
const MAX_LOOKAHEAD_SAMPLES: usize = 4_096;
const TRUE_PEAK_PHASES: [f32; 3] = [0.25, 0.5, 0.75];
/// Four-point cubic interpolation needs one future sample. Three samples of
/// latency ensure the complete cubic support is known before its first sample
/// exits the delay line.
const TRUE_PEAK_MIN_DELAY_SAMPLES: usize = 3;
const TRUE_PEAK_GAIN_HOLD_SAMPLES: usize = 4;

/// Limiter character. Wire order is the persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Clean,
    Punch,
    Modern,
    Clip,
}

impl Style {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Punch => "punch",
            Self::Modern => "modern",
            Self::Clip => "clip",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(Self::Clean),
            "punch" => Some(Self::Punch),
            "modern" => Some(Self::Modern),
            "clip" => Some(Self::Clip),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Clean => 0.0,
            Self::Punch => 1.0,
            Self::Modern => 2.0,
            Self::Clip => 3.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Punch,
            2 => Self::Modern,
            3 => Self::Clip,
            _ => Self::Clean,
        }
    }

    /// Soft-knee width in dB — Clean is softest, Clip is nearly hard.
    pub fn knee_db(self) -> f32 {
        match self {
            Self::Clean => 4.0,
            Self::Punch => 2.0,
            Self::Modern => 1.5,
            Self::Clip => 0.2,
        }
    }

    /// Attack seconds layered under the user's release / lookahead.
    pub fn attack_sec(self) -> f32 {
        match self {
            Self::Clean => 0.002,
            Self::Punch => 0.0008,
            Self::Modern => 0.0004,
            Self::Clip => 0.00005,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

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
    fn push_stereo(&mut self, in_l: f32, in_r: f32, out_l: f32, out_r: f32) {
        let in_abs = in_l.abs().max(in_r.abs());
        let out_abs = out_l.abs().max(out_r.abs());

        self.in_peak = if in_abs > self.in_peak {
            in_abs
        } else {
            self.in_peak * self.peak_coeff
        };
        self.out_peak = if out_abs > self.out_peak {
            out_abs
        } else {
            self.out_peak * self.peak_coeff
        };

        let input_ms = (in_l * in_l + in_r * in_r) * 0.5;
        let output_ms = (out_l * out_l + out_r * out_r) * 0.5;
        self.in_ms = self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input_ms;
        self.out_ms = self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output_ms;

        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

/// Streaming 4× inter-sample peak estimator. Once four samples are present,
/// Catmull-Rom interpolation evaluates the interval between the middle pair.
/// No buffers grow and no work depends on signal content.
#[derive(Debug, Clone, Copy, Default)]
struct TruePeakDetector {
    history: [f32; 4],
    filled: usize,
}

impl TruePeakDetector {
    fn reset(&mut self) {
        *self = Self::default();
    }

    #[inline]
    fn push(&mut self, sample: f32) -> f32 {
        self.history.rotate_left(1);
        self.history[3] = sample;
        self.filled = (self.filled + 1).min(4);
        if self.filled < 4 {
            return sample.abs();
        }

        let [p0, p1, p2, p3] = self.history;
        let mut peak = p1.abs().max(p2.abs());
        for phase in TRUE_PEAK_PHASES {
            // Catmull-Rom cubic through p1..p2. This is a detector only; audio
            // is not resampled, so it adds no coloration to the signal path.
            let phase2 = phase * phase;
            let phase3 = phase2 * phase;
            let interpolated = 0.5
                * ((2.0 * p1)
                    + (-p0 + p2) * phase
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * phase2
                    + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * phase3);
            peak = peak.max(interpolated.abs());
        }
        peak
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub style: Style,
    pub gain_db: f32,
    pub ceiling_db: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    pub true_peak: bool,
    pub mix: f32,
    pub stereo_link: bool,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        style: Style::Modern,
        gain_db: 0.0,
        ceiling_db: -0.3,
        release_ms: 200.0,
        lookahead_ms: 2.0,
        true_peak: true,
        mix: 100.0,
        stereo_link: true,
    }
}

/// The gain computer's static curve: decibels taken off a level of
/// `level_db` held against `threshold_db`, with a quadratic knee `knee_db`
/// wide. Positive.
pub fn static_reduction_db(level_db: f32, threshold_db: f32, knee_db: f32) -> f32 {
    let knee = knee_db.max(1.0e-6);
    let knee_start = threshold_db - knee * 0.5;
    if level_db <= knee_start {
        return 0.0;
    }
    if level_db >= threshold_db + knee * 0.5 {
        level_db - threshold_db
    } else {
        let into_knee = level_db - knee_start;
        (into_knee * into_knee) / (2.0 * knee)
    }
}

/// The steady-state output level, in dBFS, of a peak held at `input_db`:
/// the drive, the style's knee against the ceiling, the final sample-peak
/// guard, then the dry blend. Bypassed, the output is the input.
pub fn transfer_db(params: &Params, input_db: f32) -> f32 {
    if !params.power {
        return input_db;
    }
    let driven_db = input_db + params.gain_db;
    let wet_db = (driven_db
        - static_reduction_db(driven_db, params.ceiling_db, params.style.knee_db()))
    .min(params.ceiling_db);
    let amount = clamp(params.mix, 0.0, 100.0) / 100.0;
    linear_to_db(mix(db_to_linear(input_db), db_to_linear(wet_db), amount).max(1.0e-9))
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "BurnLimit",
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
                id: "style",
                name: "Style",
                default_value: 2.0,
                min: 0.0,
                max: 3.0,
                unit: "enum",
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
                id: "ceilingDb",
                name: "Ceiling",
                default_value: -0.3,
                min: -6.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 200.0,
                min: 20.0,
                max: 2_000.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "lookaheadMs",
                name: "Lookahead",
                default_value: 2.0,
                min: 0.0,
                max: 10.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "truePeak",
                name: "True Peak",
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
                id: "stereoLink",
                name: "Link",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

#[derive(Debug, Clone)]
struct LookaheadLine {
    left: [f32; MAX_LOOKAHEAD_SAMPLES],
    right: [f32; MAX_LOOKAHEAD_SAMPLES],
    write: usize,
    delay: usize,
}

impl LookaheadLine {
    fn new() -> Self {
        Self {
            left: [0.0; MAX_LOOKAHEAD_SAMPLES],
            right: [0.0; MAX_LOOKAHEAD_SAMPLES],
            write: 0,
            delay: 0,
        }
    }

    fn reset(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.write = 0;
    }

    fn set_delay(&mut self, delay: usize) {
        let delay = delay.min(MAX_LOOKAHEAD_SAMPLES - 1);
        if delay != self.delay {
            self.reset();
            self.delay = delay;
        }
    }

    #[inline]
    fn push_read(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.delay == 0 {
            return (left, right);
        }
        let read = (self.write + MAX_LOOKAHEAD_SAMPLES - self.delay) % MAX_LOOKAHEAD_SAMPLES;
        let out_l = self.left[read];
        let out_r = self.right[read];
        self.left[self.write] = left;
        self.right[self.write] = right;
        self.write += 1;
        if self.write >= MAX_LOOKAHEAD_SAMPLES {
            self.write = 0;
        }
        (out_l, out_r)
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    sample_rate: f32,
    input_gain: f32,
    ceiling_linear: f32,
    attack_coeff: f32,
    release_coeff: f32,
    knee_db: f32,
    envelope_l: f32,
    envelope_r: f32,
    gr_db: f32,
    true_peak_l: TruePeakDetector,
    true_peak_r: TruePeakDetector,
    true_peak_gain_l: f32,
    true_peak_gain_r: f32,
    true_peak_hold_l: usize,
    true_peak_hold_r: usize,
    delay: LookaheadLine,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            params: default_params(),
            sample_rate: sr,
            input_gain: 1.0,
            ceiling_linear: 1.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            knee_db: 1.5,
            envelope_l: 0.0,
            envelope_r: 0.0,
            gr_db: 0.0,
            true_peak_l: TruePeakDetector::default(),
            true_peak_r: TruePeakDetector::default(),
            true_peak_gain_l: 1.0,
            true_peak_gain_r: 1.0,
            true_peak_hold_l: 0,
            true_peak_hold_r: 0,
            delay: LookaheadLine::new(),
            meters: Meters::new(sr),
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db.max(0.0)
    }

    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: if self.params.power {
                self.gr_db.max(0.0)
            } else {
                0.0
            },
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.apply_params();
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    pub fn latency_samples(&self) -> usize {
        self.delay.delay
    }

    fn apply_params(&mut self) {
        let style = self.params.style;
        self.knee_db = style.knee_db();
        self.attack_coeff = time_constant(self.sample_rate, style.attack_sec());
        self.release_coeff = time_constant(self.sample_rate, self.params.release_ms * 0.001);
        self.input_gain = db_to_linear(self.params.gain_db);

        self.ceiling_linear = db_to_linear(self.params.ceiling_db);

        let mut delay = ((self.params.lookahead_ms * 0.001) * self.sample_rate).round() as usize;
        if self.params.true_peak {
            delay = delay.max(TRUE_PEAK_MIN_DELAY_SAMPLES);
        } else {
            self.true_peak_l.reset();
            self.true_peak_r.reset();
            self.true_peak_gain_l = 1.0;
            self.true_peak_gain_r = 1.0;
            self.true_peak_hold_l = 0;
            self.true_peak_hold_r = 0;
        }
        self.delay.set_delay(delay.min(MAX_LOOKAHEAD_SAMPLES - 1));
    }

    /// Gain computer: how much linear gain keeps `level` at or under the ceiling.
    #[inline]
    fn compute_gain(&self, level: f32) -> f32 {
        if level <= 1.0e-12 {
            return 1.0;
        }
        let ceiling = self.ceiling_linear.max(1.0e-6);
        let gr_db = static_reduction_db(linear_to_db(level), linear_to_db(ceiling), self.knee_db);
        db_to_linear(-gr_db)
    }

    #[inline]
    fn smooth_envelope(envelope: &mut f32, target: f32, attack: f32, release: f32) {
        let coeff = if target > *envelope { attack } else { release };
        *envelope = coeff * *envelope + (1.0 - coeff) * target;
    }

    #[inline]
    fn smooth_safety_gain(gain: &mut f32, hold: &mut usize, target: f32, release: f32) {
        if target <= *gain {
            *gain = target;
            *hold = TRUE_PEAK_GAIN_HOLD_SAMPLES;
        } else if *hold > 0 {
            *hold -= 1;
        } else {
            *gain = release * *gain + (1.0 - release) * target;
        }
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.envelope_l = 0.0;
        self.envelope_r = 0.0;
        self.gr_db = 0.0;
        self.true_peak_l.reset();
        self.true_peak_r.reset();
        self.true_peak_gain_l = 1.0;
        self.true_peak_gain_r = 1.0;
        self.true_peak_hold_l = 0;
        self.true_peak_hold_r = 0;
        self.delay.reset();
        self.meters.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.meters = Meters::new(self.sample_rate);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        // The delay always carries the unprocessed signal. This keeps bypass
        // and dry/wet mixing aligned with the latency reported to the host.
        let (delayed_l, delayed_r) = self.delay.push_read(left, right);
        if !self.params.power {
            self.meters.push_stereo(left, right, delayed_l, delayed_r);
            self.envelope_l = 0.0;
            self.envelope_r = 0.0;
            self.gr_db = 0.0;
            self.true_peak_l.reset();
            self.true_peak_r.reset();
            self.true_peak_gain_l = 1.0;
            self.true_peak_gain_r = 1.0;
            self.true_peak_hold_l = 0;
            self.true_peak_hold_r = 0;
            return (delayed_l, delayed_r);
        }

        let driven_l = left * self.input_gain;
        let driven_r = right * self.input_gain;

        // Detect on the undelayed path so GR can start before the peak exits.
        let peak_l = if self.params.true_peak {
            self.true_peak_l.push(driven_l)
        } else {
            driven_l.abs()
        };
        let peak_r = if self.params.true_peak {
            self.true_peak_r.push(driven_r)
        } else {
            driven_r.abs()
        };
        if self.params.true_peak {
            let safety_l = (self.ceiling_linear / peak_l.max(1.0e-12)).min(1.0);
            let safety_r = (self.ceiling_linear / peak_r.max(1.0e-12)).min(1.0);
            if self.params.stereo_link {
                let linked = safety_l.min(safety_r);
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_l,
                    &mut self.true_peak_hold_l,
                    linked,
                    self.release_coeff,
                );
                self.true_peak_gain_r = self.true_peak_gain_l;
                self.true_peak_hold_r = self.true_peak_hold_l;
            } else {
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_l,
                    &mut self.true_peak_hold_l,
                    safety_l,
                    self.release_coeff,
                );
                Self::smooth_safety_gain(
                    &mut self.true_peak_gain_r,
                    &mut self.true_peak_hold_r,
                    safety_r,
                    self.release_coeff,
                );
            }
        }
        if self.params.stereo_link {
            let linked = peak_l.max(peak_r);
            Self::smooth_envelope(
                &mut self.envelope_l,
                linked,
                self.attack_coeff,
                self.release_coeff,
            );
            self.envelope_r = self.envelope_l;
        } else {
            Self::smooth_envelope(
                &mut self.envelope_l,
                peak_l,
                self.attack_coeff,
                self.release_coeff,
            );
            Self::smooth_envelope(
                &mut self.envelope_r,
                peak_r,
                self.attack_coeff,
                self.release_coeff,
            );
        }

        let gain_l = self.compute_gain(self.envelope_l);
        let gain_r = self.compute_gain(self.envelope_r);
        let delayed_driven_l = delayed_l * self.input_gain;
        let delayed_driven_r = delayed_r * self.input_gain;
        // A final sample-peak safety gain makes ceiling compliance independent
        // of style attack. The lookahead envelope still supplies the musical
        // shape; this guard only catches what would otherwise overshoot.
        let safety_l = (self.ceiling_linear / delayed_driven_l.abs().max(1.0e-12)).min(1.0);
        let safety_r = (self.ceiling_linear / delayed_driven_r.abs().max(1.0e-12)).min(1.0);
        let (applied_l, applied_r) = if self.params.stereo_link {
            let linked = gain_l
                .min(gain_r)
                .min(self.true_peak_gain_l)
                .min(self.true_peak_gain_r)
                .min(safety_l)
                .min(safety_r);
            (linked, linked)
        } else {
            (
                gain_l.min(self.true_peak_gain_l).min(safety_l),
                gain_r.min(self.true_peak_gain_r).min(safety_r),
            )
        };
        let deepest_gain = applied_l.min(applied_r);
        self.gr_db = -linear_to_db(deepest_gain.max(1.0e-12));

        let wet_l = delayed_driven_l * applied_l;
        let wet_r = delayed_driven_r * applied_r;

        let amount = self.params.mix / 100.0;
        let out_l = mix(delayed_l, wet_l, amount);
        let out_r = mix(delayed_r, wet_r, amount);
        self.meters.push_stereo(driven_l, driven_r, out_l, out_r);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_tone(dsp: &mut Dsp, amplitude: f32, samples: usize) -> MeterFrame {
        for n in 0..samples {
            let x = (n as f32 * 0.05).sin() * amplitude;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
        }
        dsp.meter_frame()
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);

        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(count, ids.len());

        let defaults = ipc::ui_values(&default_params());
        for param in d.params {
            let (_, actual) = defaults
                .iter()
                .find(|(id, _)| *id == param.id)
                .copied()
                .unwrap_or_else(|| panic!("`{}` is missing from ui_values", param.id));
            assert!(
                (actual - param.default_value).abs() < 1.0e-5,
                "{} default drifted",
                param.id
            );
        }
    }

    #[test]
    fn hot_signal_is_held_under_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 12.0;
        params.ceiling_db = -1.0;
        params.true_peak = false;
        params.style = Style::Clip;
        params.lookahead_ms = 3.0;
        params.release_ms = 50.0;
        dsp.set_params(params);

        let mut peak = 0.0f32;
        for _ in 0..8_000 {
            let (l, r) = dsp.process_stereo(0.9, 0.9);
            peak = peak.max(l.abs()).max(r.abs());
        }
        let ceiling = db_to_linear(-1.0);
        assert!(
            peak <= ceiling * 1.000_01,
            "peak {peak} exceeded ceiling {ceiling}"
        );
        assert!(dsp.gain_reduction_db() > 1.0);
    }

    #[test]
    fn transient_ceiling_is_strict_for_every_style() {
        for style in [Style::Clean, Style::Punch, Style::Modern, Style::Clip] {
            let mut dsp = Dsp::new(48_000.0);
            let mut params = default_params();
            params.style = style;
            params.gain_db = 24.0;
            params.ceiling_db = -3.0;
            params.true_peak = true;
            params.lookahead_ms = 0.0;
            params.mix = 100.0;
            dsp.set_params(params);

            let ceiling = db_to_linear(-3.0);
            for index in 0..512 {
                let sample = if index % 31 == 0 { 1.0 } else { -0.91 };
                let (left, right) = dsp.process_stereo(sample, -sample);
                assert!(
                    left.abs().max(right.abs()) <= ceiling * 1.000_01,
                    "{style:?} exceeded ceiling at sample {index}: {left}, {right}"
                );
            }
        }
    }

    #[test]
    fn lookahead_reports_latency() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.lookahead_ms = 5.0;
        dsp.set_params(params);
        assert_eq!(dsp.latency_samples(), 240);
    }

    #[test]
    fn true_peak_has_detector_latency_but_does_not_lower_the_user_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.lookahead_ms = 0.0;
        params.true_peak = true;
        params.ceiling_db = -1.0;
        dsp.set_params(params);
        assert_eq!(dsp.latency_samples(), TRUE_PEAK_MIN_DELAY_SAMPLES);
        assert!((dsp.ceiling_linear - db_to_linear(-1.0)).abs() < 1.0e-6);
    }

    #[test]
    fn true_peak_detector_catches_intersample_overshoot() {
        let mut detector = TruePeakDetector::default();
        let peak = [-1.0, 1.0, 1.0, -1.0]
            .into_iter()
            .fold(0.0_f32, |peak, sample| peak.max(detector.push(sample)));
        assert!(peak > 1.2, "expected cubic overshoot, measured {peak}");
    }

    #[test]
    fn true_peak_output_holds_reconstructed_signal_under_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.style = Style::Clip;
        params.gain_db = 0.0;
        params.ceiling_db = -1.0;
        params.lookahead_ms = 0.0;
        params.true_peak = true;
        params.mix = 100.0;
        dsp.set_params(params);

        // The samples themselves are below -1 dBFS, but Catmull-Rom
        // reconstruction reaches roughly +0.5 dBFS without TP limiting.
        let pattern = [-0.85, 0.85, 0.85, -0.85];
        let mut output_detector = TruePeakDetector::default();
        let mut reconstructed_peak = 0.0_f32;
        for index in 0..1_024 {
            let sample = pattern[index % pattern.len()];
            let (left, _) = dsp.process_stereo(sample, sample);
            reconstructed_peak = reconstructed_peak.max(output_detector.push(left));
        }

        let ceiling = db_to_linear(-1.0);
        assert!(
            reconstructed_peak <= ceiling * 1.001,
            "reconstructed peak {reconstructed_peak} exceeded ceiling {ceiling}"
        );
    }

    #[test]
    fn dry_wet_paths_are_latency_aligned() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 0.0;
        params.lookahead_ms = 1.0;
        params.true_peak = false;
        params.mix = 50.0;
        dsp.set_params(params);

        let delay = dsp.latency_samples();
        for index in 0..=delay {
            let input = if index == 0 { 0.25 } else { 0.0 };
            let (left, right) = dsp.process_stereo(input, input);
            let expected = if index == delay { 0.25 } else { 0.0 };
            assert!((left - expected).abs() < 1.0e-6);
            assert!((right - expected).abs() < 1.0e-6);
        }
    }

    #[test]
    fn anti_phase_input_does_not_cancel_the_meters() {
        let mut dsp = Dsp::new(48_000.0);
        for _ in 0..512 {
            let _ = dsp.process_stereo(0.75, -0.75);
        }
        let frame = dsp.meter_frame();
        assert!(frame.in_peak >= 0.74);
        assert!(frame.in_rms > 0.0);
        assert!(frame.out_peak > 0.0);
    }

    #[test]
    fn power_off_reports_zero_reduction() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.gain_db = 18.0;
        dsp.set_params(params);
        let _ = run_tone(&mut dsp, 0.9, 1_000);
        assert!(dsp.apply_ui_param("power", 0.0));
        let frame = run_tone(&mut dsp, 0.9, 256);
        assert_eq!(frame.gain_reduction_db, 0.0);
    }

    /// The output peak of a steady 1 kHz tone held at `input_db`, once the
    /// envelopes have settled.
    fn settled_peak_db(dsp: &mut Dsp, input_db: f32) -> f32 {
        let amplitude = db_to_linear(input_db);
        let rate = 48_000.0;
        let mut peak = 0.0f32;
        for n in 0..48_000 {
            let x = (std::f32::consts::TAU * 1_000.0 * n as f32 / rate).sin() * amplitude;
            let (l, _) = dsp.process_stereo(x, x);
            if n >= 36_000 {
                peak = peak.max(l.abs());
            }
        }
        linear_to_db(peak.max(1.0e-9))
    }

    /// The curve the editor draws is the level the limiter settles a tone
    /// at.
    #[test]
    fn the_transfer_curve_is_what_a_steady_tone_comes_out_at() {
        for (style, gain, input) in [
            (Style::Modern, 6.0, -12.0),
            (Style::Clean, 12.0, -6.0),
            (Style::Clip, 0.0, -20.0),
        ] {
            let mut params = default_params();
            params.style = style;
            params.gain_db = gain;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params.clone());
            let measured = settled_peak_db(&mut dsp, input);
            let drawn = transfer_db(&params, input);
            assert!(
                (measured - drawn).abs() < 0.5,
                "{style:?} +{gain} dB at {input} dB: measured {measured}, drawn {drawn}"
            );
        }
    }
}
