//! Transient — attack / sustain transient shaper.
//!
//! A dual-envelope detector separates the fast (attack) and slow (body)
//! portions of the signal. Attack and Sustain knobs boost or cut each region
//! independently, with Speed scaling the envelope times and Mix blending wet
//! into dry. Allocation-free after construction.

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

pub const PLUGIN_ID: &str = "futureboard.transient";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
/// Peak shaping depth at ±100% attack / sustain, in dB.
pub const MAX_SHAPE_DB: f32 = 18.0;
const FAST_ATTACK_SEC: f32 = 0.000_2;
const FAST_RELEASE_BASE_SEC: f32 = 0.010;
const SLOW_ATTACK_BASE_SEC: f32 = 0.020;
const SLOW_RELEASE_BASE_SEC: f32 = 0.200;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Absolute magnitude of the applied dynamic gain change in dB.
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub attack: f32,
    pub sustain: f32,
    pub speed: f32,
    pub mix: f32,
    pub stereo_link: bool,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        attack: 0.0,
        sustain: 0.0,
        speed: 50.0,
        mix: 100.0,
        stereo_link: true,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Transient",
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
                id: "attack",
                name: "Attack",
                default_value: 0.0,
                min: -100.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "sustain",
                name: "Sustain",
                default_value: 0.0,
                min: -100.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "speed",
                name: "Speed",
                default_value: 50.0,
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
                id: "stereoLink",
                name: "Stereo Link",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
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
    fn push(&mut self, input: f32, output: f32) {
        let in_abs = input.abs();
        let out_abs = output.abs();

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

        self.in_ms = self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input * input;
        self.out_ms = self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output * output;

        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct EnvelopePair {
    fast: f32,
    slow: f32,
}

impl EnvelopePair {
    fn reset(&mut self) {
        self.fast = 0.0;
        self.slow = 0.0;
    }

    #[inline]
    fn push(&mut self, level: f32, fast_a: f32, fast_r: f32, slow_a: f32, slow_r: f32) {
        self.fast = if level > self.fast {
            level + fast_a * (self.fast - level)
        } else {
            level + fast_r * (self.fast - level)
        };
        self.slow = if level > self.slow {
            level + slow_a * (self.slow - level)
        } else {
            level + slow_r * (self.slow - level)
        };
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    sample_rate: f32,
    meters: Meters,
    linked: EnvelopePair,
    left: EnvelopePair,
    right: EnvelopePair,
    mix_amount: f32,
    attack_amt: f32,
    sustain_amt: f32,
    fast_attack: f32,
    fast_release: f32,
    slow_attack: f32,
    slow_release: f32,
    shape_db: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            params: default_params(),
            sample_rate: sr,
            meters: Meters::new(sr),
            linked: EnvelopePair::default(),
            left: EnvelopePair::default(),
            right: EnvelopePair::default(),
            mix_amount: 1.0,
            attack_amt: 0.0,
            sustain_amt: 0.0,
            fast_attack: 0.0,
            fast_release: 0.0,
            slow_attack: 0.0,
            slow_release: 0.0,
            shape_db: 0.0,
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
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
                self.shape_db.abs()
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
        0
    }

    fn apply_params(&mut self) {
        self.mix_amount = self.params.mix / 100.0;
        self.attack_amt = self.params.attack / 100.0;
        self.sustain_amt = self.params.sustain / 100.0;

        // Speed 0 = slowest envelopes, 100 = fastest. Map to a 0.25×…4× span.
        let speed_norm = clamp(self.params.speed, 0.0, 100.0) / 100.0;
        let speed_scale = 4.0_f32.powf(1.0 - 2.0 * speed_norm);
        let sr = self.sample_rate.max(1.0);
        self.fast_attack = time_constant(sr, FAST_ATTACK_SEC);
        self.fast_release = time_constant(sr, FAST_RELEASE_BASE_SEC * speed_scale);
        self.slow_attack = time_constant(sr, SLOW_ATTACK_BASE_SEC * speed_scale);
        self.slow_release = time_constant(sr, SLOW_RELEASE_BASE_SEC * speed_scale);
    }

    fn set_sample_rate_internal(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.meters = Meters::new(self.sample_rate);
    }

    /// Gain from the dual-envelope state. Attack weights the fast−slow
    /// difference; Sustain weights the residual body once the transient has
    /// passed.
    #[inline]
    fn shape_gain(env: &EnvelopePair, attack_amt: f32, sustain_amt: f32) -> f32 {
        let fast = env.fast.max(0.0);
        let slow = env.slow.max(0.0);
        let transient = (fast - slow).max(0.0);
        let denom = fast.max(1.0e-6);
        let transient_weight = (transient / denom).clamp(0.0, 1.0);
        let body_weight = 1.0 - transient_weight;

        let attack_db = attack_amt * MAX_SHAPE_DB * transient_weight;
        let sustain_db =
            sustain_amt * MAX_SHAPE_DB * body_weight * (slow / (slow + 1.0e-3)).min(1.0);
        db_to_linear(attack_db + sustain_db)
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.meters.reset();
        self.linked.reset();
        self.left.reset();
        self.right.reset();
        self.shape_db = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.set_sample_rate_internal(sample_rate);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        let in_sum = (left + right) * 0.5;
        if !self.params.power {
            self.meters.push(in_sum, in_sum);
            self.shape_db = 0.0;
            return (left, right);
        }

        let (gain_l, gain_r) = if self.params.stereo_link {
            let level = left.abs().max(right.abs());
            self.linked.push(
                level,
                self.fast_attack,
                self.fast_release,
                self.slow_attack,
                self.slow_release,
            );
            let gain = Self::shape_gain(&self.linked, self.attack_amt, self.sustain_amt);
            (gain, gain)
        } else {
            self.left.push(
                left.abs(),
                self.fast_attack,
                self.fast_release,
                self.slow_attack,
                self.slow_release,
            );
            self.right.push(
                right.abs(),
                self.fast_attack,
                self.fast_release,
                self.slow_attack,
                self.slow_release,
            );
            (
                Self::shape_gain(&self.left, self.attack_amt, self.sustain_amt),
                Self::shape_gain(&self.right, self.attack_amt, self.sustain_amt),
            )
        };

        let wet_l = left * gain_l;
        let wet_r = right * gain_r;
        let out_l = mix(left, wet_l, self.mix_amount);
        let out_r = mix(right, wet_r, self.mix_amount);

        let report_gain = gain_l.min(gain_r);
        self.shape_db = if report_gain > 1.0e-12 {
            linear_to_db(report_gain)
        } else {
            -90.0
        };

        self.meters.push(in_sum, (out_l + out_r) * 0.5);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_impulse(dsp: &mut Dsp, samples: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(samples);
        for n in 0..samples {
            let x = if n == 0 { 0.9 } else { 0.0 };
            let (l, _) = dsp.process_stereo(x, x);
            assert!(l.is_finite());
            out.push(l);
        }
        out
    }

    #[test]
    fn descriptor_ids_are_unique_and_match_defaults() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.name, "Transient");
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
    fn power_off_passes_signal_and_reports_zero_shaping() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("power", 0.0));
        let (l, r) = dsp.process_stereo(0.5, -0.4);
        assert!((l - 0.5).abs() < 1.0e-6);
        assert!((r - (-0.4)).abs() < 1.0e-6);
        assert_eq!(dsp.meter_frame().gain_reduction_db, 0.0);
    }

    #[test]
    fn boosting_attack_raises_the_impulse_peak() {
        let mut neutral = Dsp::new(48_000.0);
        let mut boosted = Dsp::new(48_000.0);
        assert!(boosted.apply_ui_param("attack", 80.0));
        assert!(boosted.apply_ui_param("sustain", 0.0));

        let n = run_impulse(&mut neutral, 64);
        let b = run_impulse(&mut boosted, 64);
        assert!(
            b[0].abs() > n[0].abs() + 0.05,
            "boosted={} neutral={}",
            b[0],
            n[0]
        );
    }

    #[test]
    fn cutting_sustain_lowers_the_tail() {
        let mut neutral = Dsp::new(48_000.0);
        let mut cut = Dsp::new(48_000.0);
        assert!(cut.apply_ui_param("attack", 0.0));
        assert!(cut.apply_ui_param("sustain", -80.0));

        // Drive a short burst so both envelopes rise, then compare the tail.
        for _ in 0..64 {
            let _ = neutral.process_stereo(0.6, 0.6);
            let _ = cut.process_stereo(0.6, 0.6);
        }
        let mut neutral_tail = 0.0f32;
        let mut cut_tail = 0.0f32;
        for _ in 0..256 {
            let (nl, _) = neutral.process_stereo(0.05, 0.05);
            let (cl, _) = cut.process_stereo(0.05, 0.05);
            neutral_tail = neutral_tail.max(nl.abs());
            cut_tail = cut_tail.max(cl.abs());
        }
        assert!(
            cut_tail < neutral_tail * 0.95,
            "cut_tail={cut_tail} neutral_tail={neutral_tail}"
        );
    }

    #[test]
    fn wire_params_round_trip() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("attack", -40.0));
        assert!(dsp.apply_ui_param("sustain", 25.0));
        assert!(dsp.apply_ui_param("speed", 10.0));
        assert!(dsp.apply_ui_param("mix", 50.0));
        assert!(dsp.apply_ui_param("stereoLink", 0.0));
        assert_eq!(dsp.params().attack, -40.0);
        assert_eq!(dsp.params().sustain, 25.0);
        assert_eq!(dsp.params().speed, 10.0);
        assert_eq!(dsp.params().mix, 50.0);
        assert!(!dsp.params().stereo_link);
    }
}
