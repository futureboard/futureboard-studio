//! Z-Comp — ultimate multi-model dynamics processor.
//!
//! Four circuit models share one realtime-safe gain computer and colour stage:
//!
//! - **Comp 2500** — feed-forward VCA, wide soft knee, thrust sidechain, THD
//! - **Distressor** — FET feedback cell, hard ratios, British-mode grit
//! - **Avalon** — Class-A optical leveller with over-easy ratio and LDR memory
//! - **SSL** — bus glue with the dual time-constant auto-release network
//!
//! The algorithm itself lives in [`dsp`]; this module owns the parameter
//! schema, metering, and the [`StereoEffect`] wiring the host sees.

use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, db_to_linear, mix,
    time_constant,
};
use serde::{Deserialize, Serialize};

pub mod dsp;
pub mod ipc;
pub mod presets;
pub mod ui;

pub use dsp::{CellFrame, ModelCoeffs, curve_reduction_db, model_coeffs};
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

use dsp::{GainCell, Smoothed};

pub const PLUGIN_ID: &str = "futureboard.zcomp";

const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;

/// Circuit model selected by the editor. Wire order is part of the persisted
/// contract — append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompModel {
    Comp2500,
    Distressor,
    Avalon,
    Ssl,
}

impl CompModel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comp2500 => "comp2500",
            Self::Distressor => "distressor",
            Self::Avalon => "avalon",
            Self::Ssl => "ssl",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Comp2500 => "2500",
            Self::Distressor => "Distress",
            Self::Avalon => "Avalon",
            Self::Ssl => "SSL",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "comp2500" | "2500" | "api2500" => Some(Self::Comp2500),
            "distressor" | "distress" => Some(Self::Distressor),
            "avalon" => Some(Self::Avalon),
            "ssl" | "bus" => Some(Self::Ssl),
            _ => None,
        }
    }

    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Comp2500 => 0.0,
            Self::Distressor => 1.0,
            Self::Avalon => 2.0,
            Self::Ssl => 3.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        match value.round() as i32 {
            1 => Self::Distressor,
            2 => Self::Avalon,
            3 => Self::Ssl,
            _ => Self::Comp2500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Positive: how many dB the cell is taking off right now.
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
    pub model: CompModel,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub knee_db: f32,
    pub makeup_db: f32,
    pub mix: f32,
    pub sidechain_hpf_hz: f32,
    pub stereo_link: f32,
    pub color: f32,
    pub auto_release: bool,
    /// Audition the post-filter sidechain instead of the compressed signal, so
    /// the HPF and thrust settings can be heard while they are dialled.
    #[serde(default)]
    pub sc_listen: bool,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        model: CompModel::Ssl,
        threshold_db: -18.0,
        ratio: 4.0,
        attack_ms: 10.0,
        release_ms: 100.0,
        knee_db: 6.0,
        makeup_db: 0.0,
        mix: 100.0,
        sidechain_hpf_hz: 60.0,
        stereo_link: 100.0,
        color: 18.0,
        auto_release: true,
        sc_listen: false,
    }
}

/// The steady-state output level, in dBFS, of a level held at `input_db`:
/// the circuit's static curve against its own threshold, ratio and knee,
/// then makeup and the dry blend. Bypassed, the output is the input.
pub fn transfer_db(params: &Params, input_db: f32) -> f32 {
    if !params.power {
        return input_db;
    }
    let c = model_coeffs(params);
    let gr = curve_reduction_db(input_db, c.threshold_db, c.ratio, c.knee_db, c.over_easy_db);
    let dry = builtin_dsp_core::db_to_linear(input_db);
    let wet = dry * builtin_dsp_core::db_to_linear(params.makeup_db - gr);
    let amount = builtin_dsp_core::clamp(params.mix, 0.0, 100.0) / 100.0;
    builtin_dsp_core::linear_to_db(builtin_dsp_core::mix(dry, wet, amount).max(1.0e-9))
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Z-Comp",
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
                id: "model",
                name: "Model",
                default_value: 3.0, // CompModel::Ssl
                min: 0.0,
                max: 3.0,
                unit: "enum",
            },
            ParamDescriptor {
                id: "thresholdDb",
                name: "Threshold",
                default_value: -18.0,
                min: -60.0,
                max: 0.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "ratio",
                name: "Ratio",
                default_value: 4.0,
                min: 1.0,
                max: 20.0,
                unit: ":1",
            },
            ParamDescriptor {
                id: "attackMs",
                name: "Attack",
                default_value: 10.0,
                min: 0.01,
                max: 120.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "releaseMs",
                name: "Release",
                default_value: 100.0,
                min: 10.0,
                max: 2500.0,
                unit: "ms",
            },
            ParamDescriptor {
                id: "kneeDb",
                name: "Knee",
                default_value: 6.0,
                min: 0.0,
                max: 24.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "makeupDb",
                name: "Makeup",
                default_value: 0.0,
                min: -24.0,
                max: 24.0,
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
                id: "sidechainHpfHz",
                name: "Sidechain HPF",
                default_value: 60.0,
                min: 20.0,
                max: 500.0,
                unit: "Hz",
            },
            ParamDescriptor {
                id: "stereoLink",
                name: "Stereo Link",
                default_value: 100.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "color",
                name: "Color",
                default_value: 18.0,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            ParamDescriptor {
                id: "autoRelease",
                name: "Auto Release",
                default_value: 1.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
            ParamDescriptor {
                id: "scListen",
                name: "Sidechain Listen",
                default_value: 0.0,
                min: 0.0,
                max: 1.0,
                unit: "bool",
            },
        ],
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    cell: GainCell,
    makeup: Smoothed,
    mix_amount: Smoothed,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let mut dsp = Self {
            cell: GainCell::new(
                sr,
                model_coeffs(&params),
                params.sidechain_hpf_hz,
                params.stereo_link,
            ),
            makeup: Smoothed::new(sr, db_to_linear(params.makeup_db)),
            mix_amount: Smoothed::new(sr, params.mix / 100.0),
            meters: Meters::new(sr),
            params,
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.cell.gain_reduction_db()
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
                self.cell.gain_reduction_db().max(0.0)
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

    /// No lookahead: the cell reacts to the sample it is given, so the graph
    /// needs no delay compensation for this plugin.
    pub fn latency_samples(&self) -> usize {
        0
    }

    /// Control-thread work: resolve the model, retune the cell, and retarget
    /// the smoothed output gains. Never called from the audio callback.
    fn apply_params(&mut self) {
        self.cell.set_model(
            model_coeffs(&self.params),
            self.params.sidechain_hpf_hz,
            self.params.stereo_link,
        );
        self.makeup.set_target(db_to_linear(self.params.makeup_db));
        self.mix_amount.set_target(self.params.mix / 100.0);
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.cell.reset();
        self.meters.reset();
        self.makeup.snap(db_to_linear(self.params.makeup_db));
        self.mix_amount.snap(self.params.mix / 100.0);
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.cell.set_sample_rate(sr);
        self.makeup.set_sample_rate(sr);
        self.mix_amount.set_sample_rate(sr);
        self.meters = Meters::new(sr);
        self.apply_params();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            self.meters.push((left, right), (left, right));
            return (left, right);
        }

        let frame = self.cell.process_stereo(left, right);
        // Both smoothers advance every sample so a branch cannot desynchronise
        // them from the audio they scale.
        let makeup = self.makeup.next();
        let amount = self.mix_amount.next();

        let (out_l, out_r) = if self.params.sc_listen {
            // Auditioning the detector feed: the gain cell still runs so the
            // reduction meter keeps telling the truth about what it would do.
            (
                frame.sidechain_left * makeup,
                frame.sidechain_right * makeup,
            )
        } else {
            (
                mix(left, frame.left * makeup, amount),
                mix(right, frame.right * makeup, amount),
            )
        };

        self.meters.push((left, right), (out_l, out_r));
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
        assert_eq!(d.category, PluginCategory::Effect);

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
                (param.default_value - actual).abs() < 1.0e-6,
                "`{}`: descriptor {} vs default {actual}",
                param.id,
                param.default_value
            );
        }
    }

    #[test]
    fn all_models_process_finite_and_reduce_loud_signal() {
        for model in [
            CompModel::Comp2500,
            CompModel::Distressor,
            CompModel::Avalon,
            CompModel::Ssl,
        ] {
            let mut params = default_params();
            params.model = model;
            params.threshold_db = -30.0;
            params.ratio = 8.0;
            params.mix = 100.0;
            params.color = 40.0;
            params.auto_release = true;
            let mut dsp = Dsp::new(48_000.0);
            dsp.set_params(params);
            let frame = run_tone(&mut dsp, 0.85, 24_000);
            assert!(
                frame.gain_reduction_db > 0.5,
                "{:?} should reduce a loud tone, got {} dB",
                model,
                frame.gain_reduction_db
            );
            assert!(frame.out_peak < frame.in_peak || frame.gain_reduction_db > 1.0);
        }
    }

    #[test]
    fn distressor_is_more_aggressive_than_avalon() {
        let mut distress = default_params();
        distress.model = CompModel::Distressor;
        distress.threshold_db = -24.0;
        distress.ratio = 6.0;
        distress.color = 70.0;
        distress.mix = 100.0;

        let mut avalon = distress.clone();
        avalon.model = CompModel::Avalon;
        avalon.color = 20.0;

        let mut d_dsp = Dsp::new(48_000.0);
        d_dsp.set_params(distress);
        let d_frame = run_tone(&mut d_dsp, 0.9, 24_000);

        let mut a_dsp = Dsp::new(48_000.0);
        a_dsp.set_params(avalon);
        let a_frame = run_tone(&mut a_dsp, 0.9, 24_000);

        assert!(
            d_frame.gain_reduction_db > a_frame.gain_reduction_db,
            "distress {} vs avalon {}",
            d_frame.gain_reduction_db,
            a_frame.gain_reduction_db
        );
    }

    #[test]
    fn power_off_is_true_bypass() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.power = false;
        dsp.set_params(params);
        for n in 0..256 {
            let x = (n as f32 * 0.1).sin() * 0.5;
            let (l, r) = dsp.process_stereo(x, -x);
            assert_eq!(l, x);
            assert_eq!(r, -x);
        }
        assert_eq!(dsp.meter_frame().gain_reduction_db, 0.0);
    }

    #[test]
    fn wire_param_updates_reconfigure_without_allocating_panic() {
        let mut dsp = Dsp::new(48_000.0);
        assert!(dsp.apply_wire_param(ipc::MODEL_INDEX, CompModel::Comp2500.to_wire()));
        assert!(dsp.apply_wire_param(ipc::THRESHOLD_INDEX, -22.0));
        assert!(dsp.apply_wire_param(ipc::RATIO_INDEX, 10.0));
        let _ = run_tone(&mut dsp, 0.7, 2048);
        assert!(dsp.meter_frame().gain_reduction_db >= 0.0);
    }

    #[test]
    fn model_coeffs_ssl_auto_uses_a_dual_time_constant_network() {
        let mut params = default_params();
        params.model = CompModel::Ssl;
        params.auto_release = true;
        let auto = model_coeffs(&params);
        params.auto_release = false;
        params.release_ms = 50.0;
        let manual = model_coeffs(&params);

        assert!(auto.release_slow_sec > auto.release_fast_sec);
        assert!(auto.release_slow_sec > manual.release_fast_sec);
        assert!(auto.program_auto_bias > manual.program_auto_bias);
    }

    #[test]
    fn steady_state_reduction_tracks_the_dialled_ratio() {
        // Feed-forward VCA with no colour: the only things between the input
        // peak and the meter are the detector blend and the ballistics ripple.
        let mut params = default_params();
        params.model = CompModel::Comp2500;
        params.color = 0.0;
        params.knee_db = 0.0;
        params.ratio = 4.0;
        params.threshold_db = -30.0;
        params.mix = 100.0;
        params.makeup_db = 0.0;

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);
        // -10 dBFS peak is 20 dB over threshold; 4:1 asks for 15 dB off.
        let frame = run_tone(&mut dsp, 0.3162, 48_000);

        assert!(
            (frame.gain_reduction_db - 15.0).abs() < 2.5,
            "expected ~15 dB of reduction, got {}",
            frame.gain_reduction_db
        );
    }

    #[test]
    fn sidechain_listen_outputs_the_detector_feed() {
        let mut params = default_params();
        params.model = CompModel::Comp2500;
        params.color = 0.0;
        params.sidechain_hpf_hz = 500.0;
        params.sc_listen = true;
        params.makeup_db = 0.0;

        let mut dsp = Dsp::new(48_000.0);
        dsp.set_params(params);

        // 30 Hz is far below the sidechain corner, so auditioning the detector
        // feed should be near silent while the cell still reports its work.
        let step = 2.0 * std::f32::consts::PI * 30.0 / 48_000.0;
        let mut peak = 0.0_f32;
        for n in 0..24_000 {
            let x = (n as f32 * step).sin() * 0.8;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            if n > 12_000 {
                peak = peak.max(l.abs());
            }
        }
        assert!(peak < 0.08, "sidechain listen passed the lows: {peak}");
    }

    #[test]
    fn makeup_changes_do_not_step_the_output() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.mix = 100.0;
        params.makeup_db = 0.0;
        params.color = 0.0;
        dsp.set_params(params.clone());
        for n in 0..4_800 {
            dsp.process_stereo((n as f32 * 0.05).sin() * 0.2, 0.0);
        }

        params.makeup_db = 18.0;
        dsp.set_params(params);

        let mut previous = dsp.process_stereo(0.2, 0.0).0;
        for _ in 0..4_800 {
            let (out, _) = dsp.process_stereo(0.2, 0.0);
            assert!(
                (out - previous).abs() < 0.02,
                "makeup stepped by {}",
                out - previous
            );
            previous = out;
        }
    }
}
