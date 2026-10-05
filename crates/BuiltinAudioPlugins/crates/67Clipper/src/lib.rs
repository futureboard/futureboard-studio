//! 67Clipper — Flatline-style clipper / limiter.
//!
//! Three modes (Clip, Hybrid, Limit) share a drive stage into a soft clipper
//! whose knee is shaped by the center `%` control, then a ceiling trim and
//! dry/wet mix. Hybrid and Limit add peak gain reduction; Limit is the most
//! aggressive and can stereo-link its detector.

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

pub const PLUGIN_ID: &str = "futureboard.clipper67";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
const DC_BLOCK_HZ: f32 = 5.0;
const INTERNAL_CEILING: f32 = 1.0;
const HYBRID_CATCH_THRESHOLD: f32 = 0.90;

/// Processing mode. Wire order: Clip, Hybrid, Limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Clip,
    Hybrid,
    Limit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clip => "clip",
            Self::Hybrid => "hybrid",
            Self::Limit => "limit",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "clip" => Some(Self::Clip),
            "hybrid" => Some(Self::Hybrid),
            "limit" => Some(Self::Limit),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Clip => 0.0,
            Self::Hybrid => 1.0,
            Self::Limit => 2.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Hybrid,
            2 => Self::Limit,
            _ => Self::Clip,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub mode: Mode,
    pub threshold_db: f32,
    pub shape: f32,
    pub ceiling_db: f32,
    pub mix: f32,
    pub stereo_link: bool,
    pub dc_filter: bool,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: Mode::Clip,
        threshold_db: -6.0,
        shape: 50.0,
        ceiling_db: -0.3,
        mix: 100.0,
        stereo_link: true,
        dc_filter: true,
    }
}

/// The clipper's waveshaper: `x` through a curve `shape` percent soft, from
/// a hard clip at 0 to a tanh knee at 100, never past full scale.
#[inline]
pub fn clip_curve(x: f32, shape: f32) -> f32 {
    let softness = clamp(shape, 0.0, 100.0) / 100.0;
    if softness <= 0.001 {
        x.clamp(-INTERNAL_CEILING, INTERNAL_CEILING)
    } else {
        let drive = 1.0 + (1.0 - softness) * 15.0;
        (x * drive).tanh() / drive.tanh()
    }
}

/// The steady-state output peak, in dBFS, of a peak held at `input_db`: the
/// threshold's drive, the mode's clip and peak gain once its envelope has
/// settled, the ceiling, then the dry blend. Bypassed, the output is the
/// input.
pub fn transfer_db(params: &Params, input_db: f32) -> f32 {
    if !params.power {
        return input_db;
    }
    let dry = db_to_linear(input_db);
    let driven = dry * db_to_linear(-params.threshold_db);
    let wet = match params.mode {
        Mode::Clip => clip_curve(driven, params.shape),
        Mode::Hybrid => {
            let clipped = clip_curve(driven, params.shape);
            if clipped <= HYBRID_CATCH_THRESHOLD {
                clipped
            } else {
                clipped * (HYBRID_CATCH_THRESHOLD / clipped).sqrt()
            }
        }
        Mode::Limit => driven.min(INTERNAL_CEILING),
    };
    let wet = wet * db_to_linear(params.ceiling_db);
    let amount = clamp(params.mix, 0.0, 100.0) / 100.0;
    linear_to_db(mix(dry, wet, amount).max(1.0e-9))
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "67Clipper",
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
                max: 2.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "thresholdDb",
                name: "Threshold",
                default_value: -6.0,
                min: -24.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "shape",
                name: "Shape",
                default_value: 50.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
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
            ParamDescriptor {
                id: "dcFilter",
                name: "DC Filter",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

/// One-pole DC blocker (~5 Hz). `y = x - x1 + r·y1`.
#[derive(Debug, Clone)]
struct DcBlock {
    r: f32,
    x1_l: f32,
    y1_l: f32,
    x1_r: f32,
    y1_r: f32,
}

impl DcBlock {
    fn new(sample_rate: f32) -> Self {
        let mut block = Self {
            r: 0.0,
            x1_l: 0.0,
            y1_l: 0.0,
            x1_r: 0.0,
            y1_r: 0.0,
        };
        block.set_sample_rate(sample_rate);
        block
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.r = (1.0 - (2.0 * std::f32::consts::PI * DC_BLOCK_HZ) / sr).clamp(0.9, 0.999_99);
    }

    fn reset(&mut self) {
        self.x1_l = 0.0;
        self.y1_l = 0.0;
        self.x1_r = 0.0;
        self.y1_r = 0.0;
    }

    #[inline]
    fn run(&mut self, left: f32, right: f32) -> (f32, f32) {
        let yl = left - self.x1_l + self.r * self.y1_l;
        let yr = right - self.x1_r + self.r * self.y1_r;
        self.x1_l = left;
        self.y1_l = yl;
        self.x1_r = right;
        self.y1_r = yr;
        (yl, yr)
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

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    sample_rate: f32,
    dc_block: DcBlock,
    meters: Meters,
    drive_gain: f32,
    ceiling_gain: f32,
    mix_amount: f32,
    limit_env_linked: f32,
    limit_env_l: f32,
    limit_env_r: f32,
    hybrid_env_linked: f32,
    hybrid_env_l: f32,
    hybrid_env_r: f32,
    limit_attack: f32,
    limit_release: f32,
    hybrid_attack: f32,
    hybrid_release: f32,
    gain_reduction_db: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut dsp = Self {
            params: default_params(),
            sample_rate: sr,
            dc_block: DcBlock::new(sr),
            meters: Meters::new(sr),
            drive_gain: 1.0,
            ceiling_gain: 1.0,
            mix_amount: 1.0,
            limit_env_linked: 0.0,
            limit_env_l: 0.0,
            limit_env_r: 0.0,
            hybrid_env_linked: 0.0,
            hybrid_env_l: 0.0,
            hybrid_env_r: 0.0,
            limit_attack: 0.0,
            limit_release: 0.0,
            hybrid_attack: 0.0,
            hybrid_release: 0.0,
            gain_reduction_db: 0.0,
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
                self.gain_reduction_db.max(0.0)
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
        self.drive_gain = db_to_linear(-self.params.threshold_db);
        self.ceiling_gain = db_to_linear(self.params.ceiling_db);
        self.mix_amount = self.params.mix / 100.0;
        let sr = self.sample_rate.max(1.0);
        self.limit_attack = time_constant(sr, 0.000_05);
        self.limit_release = time_constant(sr, 0.050);
        self.hybrid_attack = time_constant(sr, 0.001);
        self.hybrid_release = time_constant(sr, 0.150);
    }

    fn set_sample_rate_internal(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.dc_block.set_sample_rate(self.sample_rate);
        self.meters = Meters::new(self.sample_rate);
    }

    #[inline]
    fn shape_clip(x: f32, shape: f32) -> f32 {
        clip_curve(x, shape)
    }

    #[inline]
    fn update_envelope(env: f32, peak: f32, attack: f32, release: f32) -> f32 {
        if peak > env {
            peak + attack * (env - peak)
        } else {
            peak + release * (env - peak)
        }
    }

    #[inline]
    fn peak_gain(env: f32) -> f32 {
        if env > INTERNAL_CEILING {
            INTERNAL_CEILING / env.max(1.0e-12)
        } else {
            1.0
        }
    }

    #[inline]
    fn hybrid_peak_gain(env: f32) -> f32 {
        if env <= HYBRID_CATCH_THRESHOLD {
            1.0
        } else {
            let excess = env / HYBRID_CATCH_THRESHOLD;
            (1.0 / excess).sqrt()
        }
    }

    fn process_wet(&mut self, driven_l: f32, driven_r: f32) -> (f32, f32, f32) {
        match self.params.mode {
            Mode::Clip => {
                let wet_l = Self::shape_clip(driven_l, self.params.shape);
                let wet_r = Self::shape_clip(driven_r, self.params.shape);
                let gr = Self::clip_gr(driven_l, driven_r, wet_l, wet_r);
                (wet_l, wet_r, gr)
            }
            Mode::Hybrid => {
                let clipped_l = Self::shape_clip(driven_l, self.params.shape);
                let clipped_r = Self::shape_clip(driven_r, self.params.shape);
                if self.params.stereo_link {
                    let peak = clipped_l.abs().max(clipped_r.abs());
                    self.hybrid_env_linked = Self::update_envelope(
                        self.hybrid_env_linked,
                        peak,
                        self.hybrid_attack,
                        self.hybrid_release,
                    );
                    let gain = Self::hybrid_peak_gain(self.hybrid_env_linked);
                    let wet_l = clipped_l * gain;
                    let wet_r = clipped_r * gain;
                    let gr = Self::clip_gr(driven_l, driven_r, wet_l, wet_r);
                    (wet_l, wet_r, gr)
                } else {
                    let peak_l = clipped_l.abs();
                    let peak_r = clipped_r.abs();
                    self.hybrid_env_l = Self::update_envelope(
                        self.hybrid_env_l,
                        peak_l,
                        self.hybrid_attack,
                        self.hybrid_release,
                    );
                    self.hybrid_env_r = Self::update_envelope(
                        self.hybrid_env_r,
                        peak_r,
                        self.hybrid_attack,
                        self.hybrid_release,
                    );
                    let wet_l = clipped_l * Self::hybrid_peak_gain(self.hybrid_env_l);
                    let wet_r = clipped_r * Self::hybrid_peak_gain(self.hybrid_env_r);
                    let gr_l = Self::sample_gr(driven_l, wet_l);
                    let gr_r = Self::sample_gr(driven_r, wet_r);
                    (wet_l, wet_r, gr_l.max(gr_r))
                }
            }
            Mode::Limit => {
                if self.params.stereo_link {
                    let peak = driven_l.abs().max(driven_r.abs());
                    self.limit_env_linked = Self::update_envelope(
                        self.limit_env_linked,
                        peak,
                        self.limit_attack,
                        self.limit_release,
                    );
                    let gain = Self::peak_gain(self.limit_env_linked);
                    let wet_l = driven_l * gain;
                    let wet_r = driven_r * gain;
                    let gr = if gain < 1.0 {
                        linear_to_db(gain).abs()
                    } else {
                        0.0
                    };
                    (wet_l, wet_r, gr)
                } else {
                    let peak_l = driven_l.abs();
                    let peak_r = driven_r.abs();
                    self.limit_env_l = Self::update_envelope(
                        self.limit_env_l,
                        peak_l,
                        self.limit_attack,
                        self.limit_release,
                    );
                    self.limit_env_r = Self::update_envelope(
                        self.limit_env_r,
                        peak_r,
                        self.limit_attack,
                        self.limit_release,
                    );
                    let gain_l = Self::peak_gain(self.limit_env_l);
                    let gain_r = Self::peak_gain(self.limit_env_r);
                    let wet_l = driven_l * gain_l;
                    let wet_r = driven_r * gain_r;
                    let gr_l = if gain_l < 1.0 {
                        linear_to_db(gain_l).abs()
                    } else {
                        0.0
                    };
                    let gr_r = if gain_r < 1.0 {
                        linear_to_db(gain_r).abs()
                    } else {
                        0.0
                    };
                    (wet_l, wet_r, gr_l.max(gr_r))
                }
            }
        }
    }

    #[inline]
    fn sample_gr(input: f32, output: f32) -> f32 {
        let in_db = linear_to_db(input.abs().max(1.0e-12));
        let out_db = linear_to_db(output.abs().max(1.0e-12));
        (in_db - out_db).max(0.0)
    }

    fn clip_gr(driven_l: f32, driven_r: f32, wet_l: f32, wet_r: f32) -> f32 {
        Self::sample_gr(driven_l, wet_l).max(Self::sample_gr(driven_r, wet_r))
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.dc_block.reset();
        self.meters.reset();
        self.limit_env_linked = 0.0;
        self.limit_env_l = 0.0;
        self.limit_env_r = 0.0;
        self.hybrid_env_linked = 0.0;
        self.hybrid_env_l = 0.0;
        self.hybrid_env_r = 0.0;
        self.gain_reduction_db = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.set_sample_rate_internal(sample_rate);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        let in_sum = (left + right) * 0.5;
        if !self.params.power {
            self.meters.push(in_sum, in_sum);
            return (left, right);
        }

        let (mut proc_l, mut proc_r) = (left, right);
        if self.params.dc_filter {
            (proc_l, proc_r) = self.dc_block.run(proc_l, proc_r);
        }

        let driven_l = proc_l * self.drive_gain;
        let driven_r = proc_r * self.drive_gain;
        let (wet_l, wet_r, gr) = self.process_wet(driven_l, driven_r);
        self.gain_reduction_db = gr;

        let wet_l = wet_l * self.ceiling_gain;
        let wet_r = wet_r * self.ceiling_gain;
        let out_l = mix(left, wet_l, self.mix_amount);
        let out_r = mix(right, wet_r, self.mix_amount);
        self.meters.push(in_sum, (out_l + out_r) * 0.5);
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
        assert_eq!(d.name, "67Clipper");
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
    fn power_off_passes_signal_and_reports_zero_reduction() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_ui_param("power", 0.0));
        let (l, r) = dsp.process_stereo(0.5, -0.4);
        assert!((l - 0.5).abs() < 1.0e-6);
        assert!((r - (-0.4)).abs() < 1.0e-6);
        assert_eq!(dsp.meter_frame().gain_reduction_db, 0.0);
    }

    #[test]
    fn hot_signal_is_clipped_under_ceiling() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.threshold_db = -12.0;
        params.mode = Mode::Limit;
        params.mix = 100.0;
        params.shape = 0.0;
        dsp.set_params(params);
        let ceiling = db_to_linear(-0.3);
        let _ = run_tone(&mut dsp, 0.99, 8_000);
        let frame = dsp.meter_frame();
        assert!(
            frame.out_peak <= ceiling + 0.02,
            "out_peak={} ceiling={}",
            frame.out_peak,
            ceiling
        );
    }

    #[test]
    fn mode_wire_roundtrip() {
        let mut dsp = Dsp::new(48_000.0);
        for mode in [Mode::Clip, Mode::Hybrid, Mode::Limit] {
            assert!(dsp.apply_ui_param("mode", mode.to_wire()));
            assert_eq!(dsp.params().mode, mode);
        }
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

    /// The curve the editor draws is the peak a steady tone comes out at, in
    /// every mode.
    #[test]
    fn the_transfer_curve_is_what_a_steady_tone_comes_out_at() {
        for mode in [Mode::Clip, Mode::Hybrid, Mode::Limit] {
            for input in [-18.0, -6.0, -1.0] {
                let mut params = default_params();
                params.mode = mode;
                params.dc_filter = false;
                let mut dsp = Dsp::new(48_000.0);
                dsp.set_params(params.clone());
                let measured = settled_peak_db(&mut dsp, input);
                let drawn = transfer_db(&params, input);
                assert!(
                    (measured - drawn).abs() < 0.5,
                    "{mode:?} at {input} dB: measured {measured}, drawn {drawn}"
                );
            }
        }
    }
}
