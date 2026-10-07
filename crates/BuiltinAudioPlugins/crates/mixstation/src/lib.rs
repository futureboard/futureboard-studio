//! MixStation — a zero-latency built-in channel-strip effect.
//!
//! Fixed order: input trim → HPF/LPF → four-band EQ → linked compressor →
//! saturation → stereo width → output trim → zero-latency limiter.

use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, linear_to_db, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod dsp;
pub mod ipc;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

use dsp::{
    DcBlocker, Filters, LIMITER_KNEE_DB, Limiter, Saturator, SmoothedGain, StripCompressor,
    compressor_curve_db, proportional_q, saturate, saturation_active, soft_over_db, stereo_width,
};

pub const PLUGIN_ID: &str = "futureboard.mixstation";
/// Rack positions, and equally the number of distinct modules.
pub const RACK_SLOTS: usize = 6;
const CLIP_HOLD_SECONDS: f32 = 1.0;
/// Bottom of the high-pass range; parked here the cut leaves the path.
const HPF_OPEN_HZ: f32 = 20.0;
/// Top of the low-pass range; parked here the cut leaves the path.
const LPF_OPEN_HZ: f32 = 20_000.0;
/// Drive percentage to waveshaper amount: 100 % reaches a hard knee.
const SATURATION_DRIVE_SCALE: f32 = 0.06;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub input_trim_db: f32,
    pub filters_enabled: bool,
    pub hpf_hz: f32,
    pub lpf_hz: f32,
    pub eq_enabled: bool,
    pub low_gain_db: f32,
    pub low_mid_freq_hz: f32,
    pub low_mid_gain_db: f32,
    pub high_mid_freq_hz: f32,
    pub high_mid_gain_db: f32,
    pub high_gain_db: f32,
    pub comp_enabled: bool,
    pub comp_threshold_db: f32,
    pub comp_ratio: f32,
    pub comp_attack_ms: f32,
    pub comp_release_ms: f32,
    pub comp_makeup_db: f32,
    pub sat_enabled: bool,
    pub sat_drive_pct: f32,
    pub sat_character_pct: f32,
    pub width_enabled: bool,
    pub width_pct: f32,
    pub output_trim_db: f32,
    pub limiter_enabled: bool,
    pub limiter_ceiling_db: f32,
    pub limiter_release_ms: f32,
    #[serde(default = "legacy_slot_1")]
    pub slot1_module: u8,
    #[serde(default = "legacy_slot_2")]
    pub slot2_module: u8,
    #[serde(default = "legacy_slot_3")]
    pub slot3_module: u8,
    #[serde(default = "legacy_slot_4")]
    pub slot4_module: u8,
    #[serde(default = "legacy_slot_5")]
    pub slot5_module: u8,
    #[serde(default = "legacy_slot_6")]
    pub slot6_module: u8,
    /// Per-module output trim in dB. Keyed by module rather than rack slot, so
    /// reordering the chain carries each stage's trim along with it. Added
    /// after the original state version, hence the defaults: a project saved
    /// before these existed loads at unity.
    #[serde(default)]
    pub filters_trim_db: f32,
    #[serde(default)]
    pub eq_trim_db: f32,
    #[serde(default)]
    pub comp_trim_db: f32,
    #[serde(default)]
    pub sat_trim_db: f32,
    #[serde(default)]
    pub width_trim_db: f32,
    #[serde(default)]
    pub limiter_trim_db: f32,
}

const fn legacy_slot_1() -> u8 {
    1
}
const fn legacy_slot_2() -> u8 {
    2
}
const fn legacy_slot_3() -> u8 {
    3
}
const fn legacy_slot_4() -> u8 {
    4
}
const fn legacy_slot_5() -> u8 {
    5
}
const fn legacy_slot_6() -> u8 {
    6
}

pub fn default_params() -> Params {
    Params {
        power: true,
        input_trim_db: 0.0,
        filters_enabled: false,
        hpf_hz: 30.0,
        lpf_hz: 20_000.0,
        eq_enabled: false,
        low_gain_db: 0.0,
        low_mid_freq_hz: 400.0,
        low_mid_gain_db: 0.0,
        high_mid_freq_hz: 2_500.0,
        high_mid_gain_db: 0.0,
        high_gain_db: 0.0,
        comp_enabled: false,
        comp_threshold_db: -18.0,
        comp_ratio: 4.0,
        comp_attack_ms: 10.0,
        comp_release_ms: 120.0,
        comp_makeup_db: 0.0,
        sat_enabled: false,
        sat_drive_pct: 0.0,
        sat_character_pct: 50.0,
        width_enabled: false,
        width_pct: 100.0,
        output_trim_db: 0.0,
        limiter_enabled: false,
        limiter_ceiling_db: -0.3,
        limiter_release_ms: 100.0,
        slot1_module: 0,
        slot2_module: 0,
        slot3_module: 0,
        slot4_module: 0,
        slot5_module: 0,
        slot6_module: 0,
        filters_trim_db: 0.0,
        eq_trim_db: 0.0,
        comp_trim_db: 0.0,
        sat_trim_db: 0.0,
        width_trim_db: 0.0,
        limiter_trim_db: 0.0,
    }
}

/// The compressor's knee, in dB.
pub const COMP_KNEE_DB: f32 = 6.0;
/// The EQ's fixed shelf corners, in Hz.
pub const LOW_SHELF_HZ: f32 = 100.0;
pub const HIGH_SHELF_HZ: f32 = 10_000.0;

/// Tunes the cuts and the EQ to `p` at `sample_rate`, as the strip runs them.
fn configure_filters(filters: &mut Filters, p: &Params, sample_rate: f32) {
    filters.set_high_pass(sample_rate, p.hpf_hz, HPF_OPEN_HZ);
    filters.set_low_pass(sample_rate, p.lpf_hz, LPF_OPEN_HZ);
    filters.eq[0].set_low_shelf(sample_rate, LOW_SHELF_HZ, p.low_gain_db);
    filters.eq[1].set_peak(
        sample_rate,
        p.low_mid_freq_hz,
        p.low_mid_gain_db,
        proportional_q(p.low_mid_gain_db),
    );
    filters.eq[2].set_peak(
        sample_rate,
        p.high_mid_freq_hz,
        p.high_mid_gain_db,
        proportional_q(p.high_mid_gain_db),
    );
    filters.eq[3].set_high_shelf(sample_rate, HIGH_SHELF_HZ, p.high_gain_db);
}

/// The Filters module's gain at each of `frequencies`, in dB, at
/// `sample_rate`: the two cuts as the strip runs them.
pub fn filter_response_db(p: &Params, sample_rate: f32, frequencies: &[f32]) -> Vec<f32> {
    let mut filters = Filters::new();
    configure_filters(&mut filters, p, sample_rate);
    frequencies
        .iter()
        .map(|hz| filters.cuts_db(sample_rate, *hz))
        .collect()
}

/// The EQ module's gain at each of `frequencies`, in dB, at `sample_rate`.
pub fn eq_response_db(p: &Params, sample_rate: f32, frequencies: &[f32]) -> Vec<f32> {
    let mut filters = Filters::new();
    configure_filters(&mut filters, p, sample_rate);
    frequencies
        .iter()
        .map(|hz| filters.eq_db(sample_rate, *hz))
        .collect()
}

/// The compressor's steady-state output, in dB, for a level held at
/// `input_db`: its static curve plus makeup.
pub fn comp_transfer_db(p: &Params, input_db: f32) -> f32 {
    input_db
        + compressor_curve_db(input_db, p.comp_threshold_db, p.comp_ratio, COMP_KNEE_DB)
        + p.comp_makeup_db
}

/// The limiter's steady-state output peak, in dB, for a peak held at
/// `input_db`: the knee into the ceiling, never past it.
pub fn limiter_transfer_db(p: &Params, input_db: f32) -> f32 {
    (input_db + soft_over_db(input_db - p.limiter_ceiling_db, LIMITER_KNEE_DB))
        .min(p.limiter_ceiling_db)
}

/// The Drive module's waveshaper: what one sample `x` becomes.
pub fn drive_curve(p: &Params, x: f32) -> f32 {
    saturate(
        x,
        p.sat_drive_pct * SATURATION_DRIVE_SCALE,
        p.sat_character_pct * 0.01,
    )
}

/// What the Width module makes of a hard-panned pair `(left, right)`.
pub fn width_of(p: &Params, left: f32, right: f32) -> (f32, f32) {
    stereo_width(left, right, p.width_pct * 0.01)
}

const fn param(
    id: &'static str,
    name: &'static str,
    default_value: f32,
    min: f32,
    max: f32,
    unit: &'static str,
) -> ParamDescriptor {
    ParamDescriptor {
        id,
        name,
        default_value,
        min,
        max,
        unit,
    }
}

const PARAMS: &[ParamDescriptor] = &[
    param("power", "Power", 1.0, 0.0, 1.0, "bool"),
    param("inputTrimDb", "Input Trim", 0.0, -24.0, 24.0, "dB"),
    param("filtersEnabled", "Filters", 0.0, 0.0, 1.0, "bool"),
    param("hpfHz", "High Pass", 30.0, 20.0, 500.0, "Hz"),
    param("lpfHz", "Low Pass", 20_000.0, 1_000.0, 20_000.0, "Hz"),
    param("eqEnabled", "EQ", 0.0, 0.0, 1.0, "bool"),
    param("lowGainDb", "Low Gain", 0.0, -18.0, 18.0, "dB"),
    param(
        "lowMidFreqHz",
        "Low Mid Frequency",
        400.0,
        80.0,
        2_000.0,
        "Hz",
    ),
    param("lowMidGainDb", "Low Mid Gain", 0.0, -18.0, 18.0, "dB"),
    param(
        "highMidFreqHz",
        "High Mid Frequency",
        2_500.0,
        500.0,
        12_000.0,
        "Hz",
    ),
    param("highMidGainDb", "High Mid Gain", 0.0, -18.0, 18.0, "dB"),
    param("highGainDb", "High Gain", 0.0, -18.0, 18.0, "dB"),
    param("compEnabled", "Compressor", 0.0, 0.0, 1.0, "bool"),
    param("compThresholdDb", "Threshold", -18.0, -60.0, 0.0, "dB"),
    param("compRatio", "Ratio", 4.0, 1.0, 20.0, ":1"),
    param("compAttackMs", "Attack", 10.0, 0.1, 100.0, "ms"),
    param("compReleaseMs", "Release", 120.0, 10.0, 1_000.0, "ms"),
    param("compMakeupDb", "Makeup", 0.0, -12.0, 24.0, "dB"),
    param("satEnabled", "Saturation", 0.0, 0.0, 1.0, "bool"),
    param("satDrivePct", "Drive", 0.0, 0.0, 100.0, "%"),
    param("satCharacterPct", "Character", 50.0, 0.0, 100.0, "%"),
    param("widthEnabled", "Width", 0.0, 0.0, 1.0, "bool"),
    param("widthPct", "Stereo Width", 100.0, 0.0, 200.0, "%"),
    param("outputTrimDb", "Output Trim", 0.0, -24.0, 24.0, "dB"),
    param("limiterEnabled", "Limiter", 0.0, 0.0, 1.0, "bool"),
    param("limiterCeilingDb", "Ceiling", -0.3, -12.0, 0.0, "dB"),
    param(
        "limiterReleaseMs",
        "Limiter Release",
        100.0,
        10.0,
        1_000.0,
        "ms",
    ),
    param("slot1Module", "Rack Slot 1", 0.0, 0.0, 6.0, "module"),
    param("slot2Module", "Rack Slot 2", 0.0, 0.0, 6.0, "module"),
    param("slot3Module", "Rack Slot 3", 0.0, 0.0, 6.0, "module"),
    param("slot4Module", "Rack Slot 4", 0.0, 0.0, 6.0, "module"),
    param("slot5Module", "Rack Slot 5", 0.0, 0.0, 6.0, "module"),
    param("slot6Module", "Rack Slot 6", 0.0, 0.0, 6.0, "module"),
    param("filtersTrimDb", "Filters Trim", 0.0, -24.0, 24.0, "dB"),
    param("eqTrimDb", "EQ Trim", 0.0, -24.0, 24.0, "dB"),
    param("compTrimDb", "Compressor Trim", 0.0, -24.0, 24.0, "dB"),
    param("satTrimDb", "Saturation Trim", 0.0, -24.0, 24.0, "dB"),
    param("widthTrimDb", "Width Trim", 0.0, -24.0, 24.0, "dB"),
    param("limiterTrimDb", "Limiter Trim", 0.0, -24.0, 24.0, "dB"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "MixStation",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Linked compressor and limiter reduction combined, in positive dB.
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
    /// Level entering each rack position, in chain order, linear peak.
    pub slot_in_peak: [f32; RACK_SLOTS],
    /// Level leaving each rack position, after that module's output trim.
    pub slot_out_peak: [f32; RACK_SLOTS],
}

#[derive(Debug, Clone)]
struct Meters {
    in_peak: f32,
    out_peak: f32,
    in_ms: f32,
    out_ms: f32,
    rms_coeff: f32,
    peak_coeff: f32,
    clip_hold_samples: usize,
    in_clip_remaining: usize,
    out_clip_remaining: usize,
    /// Per-rack-position envelopes, sharing the master meters' peak ballistics.
    slot_in: [f32; RACK_SLOTS],
    slot_out: [f32; RACK_SLOTS],
}

impl Meters {
    fn new(sample_rate: f32) -> Self {
        Self {
            in_peak: 0.0,
            out_peak: 0.0,
            in_ms: 0.0,
            out_ms: 0.0,
            rms_coeff: time_constant(sample_rate, 0.300),
            peak_coeff: time_constant(sample_rate, 0.400),
            clip_hold_samples: (sample_rate.max(1.0) * CLIP_HOLD_SECONDS) as usize,
            in_clip_remaining: 0,
            out_clip_remaining: 0,
            slot_in: [0.0; RACK_SLOTS],
            slot_out: [0.0; RACK_SLOTS],
        }
    }

    fn reset(&mut self) {
        self.in_peak = 0.0;
        self.out_peak = 0.0;
        self.in_ms = 0.0;
        self.out_ms = 0.0;
        self.in_clip_remaining = 0;
        self.out_clip_remaining = 0;
        self.slot_in = [0.0; RACK_SLOTS];
        self.slot_out = [0.0; RACK_SLOTS];
    }

    /// Publish one rack position's in/out levels. Same decay as the master
    /// meters so a row and the footer agree on what a peak looks like.
    #[inline]
    fn push_stage(&mut self, slot: usize, input: f32, output: f32) {
        if slot >= RACK_SLOTS {
            return;
        }
        self.slot_in[slot] = input.max(self.slot_in[slot] * self.peak_coeff);
        self.slot_out[slot] = output.max(self.slot_out[slot] * self.peak_coeff);
    }

    #[inline]
    fn push(&mut self, input: (f32, f32), output: (f32, f32)) {
        let input_peak = input.0.abs().max(input.1.abs());
        let output_peak = output.0.abs().max(output.1.abs());
        self.in_peak = input_peak.max(self.in_peak * self.peak_coeff);
        self.out_peak = output_peak.max(self.out_peak * self.peak_coeff);
        self.in_ms = self.rms_coeff * self.in_ms
            + (1.0 - self.rms_coeff) * (input.0 * input.0 + input.1 * input.1) * 0.5;
        self.out_ms = self.rms_coeff * self.out_ms
            + (1.0 - self.rms_coeff) * (output.0 * output.0 + output.1 * output.1) * 0.5;
        self.in_clip_remaining = self.in_clip_remaining.saturating_sub(1);
        self.out_clip_remaining = self.out_clip_remaining.saturating_sub(1);
        if input_peak >= 1.0 {
            self.in_clip_remaining = self.clip_hold_samples;
        }
        if output_peak >= 1.0 {
            self.out_clip_remaining = self.clip_hold_samples;
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    input_gain: SmoothedGain,
    output_gain: SmoothedGain,
    /// Per-module output trim, indexed by module code minus one.
    module_trim: [SmoothedGain; RACK_SLOTS],
    filters: Filters,
    compressor: StripCompressor,
    saturation_drive: f32,
    saturation_character: f32,
    saturation: Saturator,
    saturation_dc: DcBlocker,
    width: f32,
    limiter: Limiter,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let params = default_params();
        let mut dsp = Self {
            sample_rate,
            input_gain: SmoothedGain::new(sample_rate, params.input_trim_db),
            output_gain: SmoothedGain::new(sample_rate, params.output_trim_db),
            module_trim: [SmoothedGain::new(sample_rate, 0.0); RACK_SLOTS],
            filters: Filters::new(),
            compressor: StripCompressor::new(sample_rate),
            saturation_drive: 0.0,
            saturation_character: 0.5,
            saturation: Saturator::new(),
            saturation_dc: DcBlocker::new(sample_rate),
            width: 1.0,
            limiter: Limiter::new(sample_rate),
            meters: Meters::new(sample_rate),
            params,
        };
        dsp.apply_params();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, mut params: Params) {
        ipc::sanitize_params(&mut params);
        self.params = params;
        self.apply_params();
    }

    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, index, value) {
            return false;
        }
        self.apply_param_state(index);
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        ipc::ui_param_index(id).is_some_and(|index| self.apply_wire_param(index, value))
    }

    pub const fn latency_samples(&self) -> usize {
        0
    }

    pub fn meter_frame(&self) -> MeterFrame {
        let compressor_gr =
            if self.params.power && self.params.comp_enabled && self.rack_contains(3) {
                self.compressor.gain_reduction_db().max(0.0)
            } else {
                0.0
            };
        let limiter_gr =
            if self.params.power && self.params.limiter_enabled && self.rack_contains(6) {
                -linear_to_db(self.limiter.gain()).min(0.0)
            } else {
                0.0
            };
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: compressor_gr + limiter_gr,
            in_clip: self.meters.in_clip_remaining > 0,
            out_clip: self.meters.out_clip_remaining > 0,
            slot_in_peak: self.meters.slot_in,
            slot_out_peak: self.meters.slot_out,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip_remaining = 0;
        self.meters.out_clip_remaining = 0;
    }

    /// Resolves all transcendental coefficient work on the control thread.
    fn apply_params(&mut self) {
        let p = &self.params;
        self.input_gain.set_db(p.input_trim_db);
        self.output_gain.set_db(p.output_trim_db);
        configure_filters(&mut self.filters, p, self.sample_rate);
        self.compressor.set_curve(
            p.comp_threshold_db,
            p.comp_ratio,
            COMP_KNEE_DB,
            p.comp_makeup_db,
        );
        self.compressor
            .set_timing(p.comp_attack_ms * 0.001, p.comp_release_ms * 0.001);
        self.saturation_drive = p.sat_drive_pct * SATURATION_DRIVE_SCALE;
        self.saturation_character = p.sat_character_pct * 0.01;
        self.width = p.width_pct * 0.01;
        self.limiter.set_ceiling_db(p.limiter_ceiling_db);
        self.limiter
            .set_release(self.sample_rate, p.limiter_release_ms * 0.001);
        for (gain, db) in self.module_trim.iter_mut().zip(Self::module_trim_db(p)) {
            gain.set_db(db);
        }
    }

    /// Per-module trim in dB, indexed by module code minus one.
    const fn module_trim_db(params: &Params) -> [f32; RACK_SLOTS] {
        [
            params.filters_trim_db,
            params.eq_trim_db,
            params.comp_trim_db,
            params.sat_trim_db,
            params.width_trim_db,
            params.limiter_trim_db,
        ]
    }

    fn rack_contains(&self, module: u8) -> bool {
        [
            self.params.slot1_module,
            self.params.slot2_module,
            self.params.slot3_module,
            self.params.slot4_module,
            self.params.slot5_module,
            self.params.slot6_module,
        ]
        .contains(&module)
    }

    /// Update only the resolved state affected by one wire edit. The bridge
    /// drains edits on its dedicated audio producer between blocks, so a
    /// preset must not rebuild every filter and coefficient for every one of
    /// its parameter messages.
    fn apply_param_state(&mut self, index: u32) {
        let p = &self.params;
        match index {
            ipc::POWER_INDEX => {
                self.filters.reset();
                self.compressor.reset();
                self.saturation.reset();
                self.saturation_dc.reset();
                self.limiter.reset();
            }
            ipc::INPUT_TRIM_INDEX => self.input_gain.set_db(p.input_trim_db),
            ipc::HPF_INDEX => self
                .filters
                .set_high_pass(self.sample_rate, p.hpf_hz, HPF_OPEN_HZ),
            ipc::LPF_INDEX => self
                .filters
                .set_low_pass(self.sample_rate, p.lpf_hz, LPF_OPEN_HZ),
            ipc::LOW_GAIN_INDEX => {
                self.filters.eq[0].set_low_shelf(self.sample_rate, 100.0, p.low_gain_db);
            }
            ipc::LOW_MID_FREQ_INDEX | ipc::LOW_MID_GAIN_INDEX => {
                self.filters.eq[1].set_peak(
                    self.sample_rate,
                    p.low_mid_freq_hz,
                    p.low_mid_gain_db,
                    proportional_q(p.low_mid_gain_db),
                );
            }
            ipc::HIGH_MID_FREQ_INDEX | ipc::HIGH_MID_GAIN_INDEX => {
                self.filters.eq[2].set_peak(
                    self.sample_rate,
                    p.high_mid_freq_hz,
                    p.high_mid_gain_db,
                    proportional_q(p.high_mid_gain_db),
                );
            }
            ipc::HIGH_GAIN_INDEX => {
                self.filters.eq[3].set_high_shelf(self.sample_rate, 10_000.0, p.high_gain_db);
            }
            ipc::COMP_THRESHOLD_INDEX | ipc::COMP_RATIO_INDEX | ipc::COMP_MAKEUP_INDEX => {
                self.compressor
                    .set_curve(p.comp_threshold_db, p.comp_ratio, 6.0, p.comp_makeup_db);
            }
            ipc::COMP_ATTACK_INDEX | ipc::COMP_RELEASE_INDEX => self
                .compressor
                .set_timing(p.comp_attack_ms * 0.001, p.comp_release_ms * 0.001),
            ipc::SAT_DRIVE_INDEX => {
                let drive = p.sat_drive_pct * SATURATION_DRIVE_SCALE;
                if !saturation_active(self.saturation_drive) && saturation_active(drive) {
                    self.saturation_dc.reset();
                }
                self.saturation_drive = drive;
                self.saturation.recurve();
            }
            ipc::SAT_CHARACTER_INDEX => {
                self.saturation_character = p.sat_character_pct * 0.01;
                self.saturation.recurve();
            }
            ipc::SAT_ENABLED_INDEX => {
                self.saturation.reset();
                self.saturation_dc.reset();
            }
            ipc::WIDTH_INDEX => {
                self.width = p.width_pct * 0.01;
            }
            ipc::OUTPUT_TRIM_INDEX => self.output_gain.set_db(p.output_trim_db),
            ipc::FILTERS_TRIM_INDEX
            | ipc::EQ_TRIM_INDEX
            | ipc::COMP_TRIM_INDEX
            | ipc::SAT_TRIM_INDEX
            | ipc::WIDTH_TRIM_INDEX
            | ipc::LIMITER_TRIM_INDEX => {
                let slot = (index - ipc::FILTERS_TRIM_INDEX) as usize;
                self.module_trim[slot].set_db(Self::module_trim_db(p)[slot]);
            }
            ipc::LIMITER_CEILING_INDEX => self.limiter.set_ceiling_db(p.limiter_ceiling_db),
            ipc::LIMITER_RELEASE_INDEX => self
                .limiter
                .set_release(self.sample_rate, p.limiter_release_ms * 0.001),
            ipc::SLOT_1_INDEX
            | ipc::SLOT_2_INDEX
            | ipc::SLOT_3_INDEX
            | ipc::SLOT_4_INDEX
            | ipc::SLOT_5_INDEX
            | ipc::SLOT_6_INDEX => {
                self.compressor.reset();
                self.saturation.reset();
                self.saturation_dc.reset();
                self.limiter.reset();
            }
            _ => {}
        }
    }

    #[inline]
    fn process_filter_module(&mut self, channel: usize, sample: f32) -> f32 {
        self.filters.process_cuts(channel, sample)
    }

    #[inline]
    fn process_eq_module(&mut self, channel: usize, mut sample: f32) -> f32 {
        for band in &mut self.filters.eq {
            sample = band.process(channel, sample);
        }
        sample
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.filters.reset();
        self.compressor.reset();
        self.saturation.reset();
        self.saturation_dc.reset();
        self.limiter.reset();
        self.meters.reset();
        self.input_gain.snap_db(self.params.input_trim_db);
        self.output_gain.snap_db(self.params.output_trim_db);
        let trims = Self::module_trim_db(&self.params);
        for (gain, db) in self.module_trim.iter_mut().zip(trims) {
            gain.snap_db(db);
        }
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.input_gain.set_sample_rate(self.sample_rate);
        self.output_gain.set_sample_rate(self.sample_rate);
        self.compressor.set_sample_rate(self.sample_rate);
        self.saturation_dc.set_sample_rate(self.sample_rate);
        for gain in &mut self.module_trim {
            gain.set_sample_rate(self.sample_rate);
        }
        self.meters = Meters::new(self.sample_rate);
        self.apply_params();
    }

    #[inline]
    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            self.meters.push((left, right), (left, right));
            return (left, right);
        }

        let input = (left, right);
        let input_gain = self.input_gain.next();
        let mut l = left * input_gain;
        let mut r = right * input_gain;

        let slots = [
            self.params.slot1_module,
            self.params.slot2_module,
            self.params.slot3_module,
            self.params.slot4_module,
            self.params.slot5_module,
            self.params.slot6_module,
        ];
        let mut limiter_processed = false;
        for (slot, module) in slots.into_iter().enumerate() {
            // Level entering this stage, for the editor's per-row meter.
            let stage_in = l.abs().max(r.abs());
            match module {
                1 if self.params.filters_enabled => {
                    l = self.process_filter_module(0, l);
                    r = self.process_filter_module(1, r);
                }
                2 if self.params.eq_enabled => {
                    l = self.process_eq_module(0, l);
                    r = self.process_eq_module(1, r);
                }
                3 if self.params.comp_enabled => {
                    (l, r) = self.compressor.process_stereo_linked(l, r);
                }
                4 if self.params.sat_enabled && saturation_active(self.saturation_drive) => {
                    l = self.saturation_dc.process(
                        0,
                        self.saturation.process(
                            0,
                            l,
                            self.saturation_drive,
                            self.saturation_character,
                        ),
                    );
                    r = self.saturation_dc.process(
                        1,
                        self.saturation.process(
                            1,
                            r,
                            self.saturation_drive,
                            self.saturation_character,
                        ),
                    );
                }
                5 if self.params.width_enabled => {
                    (l, r) = stereo_width(l, r, self.width);
                }
                6 if self.params.limiter_enabled => {
                    (l, r) = self.limiter.process(l, r);
                    limiter_processed = true;
                }
                _ => {}
            }
            // Per-module output trim, then the stage tap. A bypassed or empty
            // slot still publishes its levels so the row's meter reads through
            // rather than going dark.
            if let Some(index) = module.checked_sub(1)
                && (index as usize) < RACK_SLOTS
            {
                let trim = self.module_trim[index as usize].next();
                l *= trim;
                r *= trim;
            }
            self.meters.push_stage(slot, stage_in, l.abs().max(r.abs()));
        }

        let output_gain = self.output_gain.next();
        l *= output_gain;
        r *= output_gain;

        if !limiter_processed {
            self.limiter.reset();
        }

        self.meters.push(input, (l, r));
        (l, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn bypassed() -> Params {
        let mut p = default_params();
        p.filters_enabled = false;
        p.eq_enabled = false;
        p.comp_enabled = false;
        p.sat_enabled = false;
        p.width_enabled = false;
        p.limiter_enabled = false;
        p
    }

    fn tone_peak(dsp: &mut Dsp, hz: f32, amplitude: f32) -> f32 {
        let step = 2.0 * std::f32::consts::PI * hz / SR;
        let mut peak = 0.0_f32;
        for n in 0..24_000 {
            let x = (n as f32 * step).sin() * amplitude;
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            if n > 12_000 {
                peak = peak.max(l.abs());
            }
        }
        peak
    }

    #[test]
    fn descriptor_wire_ids_and_defaults_agree() {
        let descriptor = descriptor();
        let values = ipc::ui_values(&default_params());
        assert_eq!(descriptor.id, PLUGIN_ID);
        assert_eq!(descriptor.params.len(), ipc::PARAM_COUNT);
        for (index, parameter) in descriptor.params.iter().enumerate() {
            assert_eq!(parameter.id, ipc::UI_PARAM_IDS[index]);
            assert_eq!(parameter.default_value, values[index]);
        }
    }

    #[test]
    fn default_rack_has_no_loaded_modules() {
        let params = default_params();
        assert!(params.power);
        assert!(!params.filters_enabled);
        assert!(!params.eq_enabled);
        assert!(!params.comp_enabled);
        assert!(!params.sat_enabled);
        assert!(!params.width_enabled);
        assert!(!params.limiter_enabled);
        assert_eq!(
            [
                params.slot1_module,
                params.slot2_module,
                params.slot3_module,
                params.slot4_module,
                params.slot5_module,
                params.slot6_module,
            ],
            [0; 6]
        );
    }

    #[test]
    fn power_and_all_module_bypasses_are_transparent() {
        let mut dsp = Dsp::new(SR);
        let mut off = default_params();
        off.power = false;
        dsp.set_params(off);
        assert_eq!(dsp.process_stereo(1.2, -0.7), (1.2, -0.7));
        dsp.set_params(bypassed());
        dsp.reset();
        for n in 0..1_000 {
            let x = (n as f32 * 0.13).sin() * 0.8;
            assert_eq!(dsp.process_stereo(x, -x), (x, -x));
        }
    }

    #[test]
    fn filters_and_eq_have_audible_effect() {
        let mut p = bypassed();
        p.filters_enabled = true;
        p.slot1_module = 1;
        p.hpf_hz = 300.0;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(p);
        assert!(tone_peak(&mut dsp, 40.0, 0.5) < 0.08);

        let mut p = bypassed();
        p.eq_enabled = true;
        p.slot1_module = 2;
        p.low_gain_db = 12.0;
        dsp = Dsp::new(SR);
        dsp.set_params(p);
        assert!(tone_peak(&mut dsp, 80.0, 0.1) > 0.2);
    }

    #[test]
    fn linked_compressor_reduces_both_channels() {
        let mut p = bypassed();
        p.comp_enabled = true;
        p.slot1_module = 3;
        p.comp_threshold_db = -30.0;
        p.comp_ratio = 10.0;
        p.comp_attack_ms = 0.1;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(p);
        for _ in 0..10_000 {
            let _ = dsp.process_stereo(0.9, 0.1);
        }
        let (l, r) = dsp.process_stereo(0.9, 0.1);
        assert!(l < 0.5 && r < 0.08);
        assert!(dsp.meter_frame().gain_reduction_db > 3.0);
    }

    #[test]
    fn saturation_and_width_change_signal_and_stay_finite() {
        let mut p = bypassed();
        p.sat_enabled = true;
        p.slot1_module = 4;
        p.sat_drive_pct = 80.0;
        p.sat_character_pct = 80.0;
        p.width_enabled = true;
        p.slot2_module = 5;
        p.width_pct = 200.0;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(p);
        let (l, r) = dsp.process_stereo(0.8, -0.8);
        assert!(l.is_finite() && r.is_finite());
        assert!((l - 0.8).abs() > 0.05 || (r + 0.8).abs() > 0.05);
    }

    #[test]
    fn rack_slot_order_changes_the_real_processing_order() {
        let mut params = bypassed();
        params.comp_enabled = true;
        params.comp_threshold_db = -24.0;
        params.comp_ratio = 8.0;
        params.comp_attack_ms = 0.1;
        params.sat_enabled = true;
        params.sat_drive_pct = 75.0;

        let mut comp_then_sat = Dsp::new(SR);
        params.slot1_module = 3;
        params.slot2_module = 4;
        comp_then_sat.set_params(params.clone());

        let mut sat_then_comp = Dsp::new(SR);
        params.slot1_module = 4;
        params.slot2_module = 3;
        sat_then_comp.set_params(params);

        let mut energy_a = 0.0;
        let mut energy_b = 0.0;
        for n in 0..24_000 {
            let input = (n as f32 * 0.17).sin() * 0.9;
            let a = comp_then_sat.process_stereo(input, input).0;
            let b = sat_then_comp.process_stereo(input, input).0;
            if n > 12_000 {
                energy_a += a * a;
                energy_b += b * b;
            }
        }
        assert!((energy_a - energy_b).abs() > 1.0);
    }

    #[test]
    fn limiter_obeys_ceiling_and_has_zero_latency() {
        let mut p = bypassed();
        p.limiter_enabled = true;
        p.slot1_module = 6;
        p.limiter_ceiling_db = -6.0;
        let ceiling = builtin_dsp_core::db_to_linear(-6.0);
        let mut dsp = Dsp::new(SR);
        dsp.set_params(p);
        let (l, r) = dsp.process_stereo(2.0, -1.5);
        assert!(l.abs() <= ceiling + 1.0e-6 && r.abs() <= ceiling + 1.0e-6);
        assert!(dsp.meter_frame().gain_reduction_db > 0.0);
        assert_eq!(dsp.latency_samples(), 0);
    }

    #[test]
    fn module_trim_scales_that_stage_only() {
        let mut params = bypassed();
        params.width_enabled = true;
        params.slot1_module = 5;
        params.width_pct = 100.0; // Unity width: the stage itself is transparent.
        let mut unity = Dsp::new(SR);
        unity.set_params(params.clone());
        unity.reset();

        params.width_trim_db = -6.0;
        let mut trimmed = Dsp::new(SR);
        trimmed.set_params(params);
        trimmed.reset();

        // Let both smoothers settle before comparing.
        for _ in 0..8_000 {
            let _ = unity.process_stereo(0.5, 0.5);
            let _ = trimmed.process_stereo(0.5, 0.5);
        }
        let plain = unity.process_stereo(0.5, 0.5).0;
        let cut = trimmed.process_stereo(0.5, 0.5).0;
        assert!(
            (cut / plain - builtin_dsp_core::db_to_linear(-6.0)).abs() < 0.01,
            "{cut} vs {plain}"
        );
    }

    #[test]
    fn module_trim_follows_the_module_not_the_rack_position() {
        // The trim is keyed by module, so moving Width from slot 1 to slot 2
        // must not hand its trim to whatever now sits in slot 1.
        let mut params = bypassed();
        params.width_enabled = true;
        params.width_trim_db = -12.0;
        params.slot1_module = 5;
        let mut first = Dsp::new(SR);
        first.set_params(params.clone());
        first.reset();

        params.slot1_module = 0;
        params.slot2_module = 5;
        let mut second = Dsp::new(SR);
        second.set_params(params);
        second.reset();

        for _ in 0..8_000 {
            let _ = first.process_stereo(0.5, 0.5);
            let _ = second.process_stereo(0.5, 0.5);
        }
        let a = first.process_stereo(0.5, 0.5).0;
        let b = second.process_stereo(0.5, 0.5).0;
        assert!((a - b).abs() < 1.0e-4, "{a} vs {b}");
    }

    #[test]
    fn legacy_state_without_trims_loads_at_unity() {
        // The trim fields postdate the original state version; a project saved
        // before them must restore silently rather than being rejected.
        let json = r#"{"version":1,"params":{
            "power":true,"inputTrimDb":0.0,"filtersEnabled":false,"hpfHz":30.0,
            "lpfHz":20000.0,"eqEnabled":false,"lowGainDb":0.0,"lowMidFreqHz":400.0,
            "lowMidGainDb":0.0,"highMidFreqHz":2500.0,"highMidGainDb":0.0,
            "highGainDb":0.0,"compEnabled":false,"compThresholdDb":-18.0,
            "compRatio":4.0,"compAttackMs":10.0,"compReleaseMs":120.0,
            "compMakeupDb":0.0,"satEnabled":false,"satDrivePct":0.0,
            "satCharacterPct":50.0,"widthEnabled":false,"widthPct":100.0,
            "outputTrimDb":0.0,"limiterEnabled":false,"limiterCeilingDb":-0.3,
            "limiterReleaseMs":100.0}}"#;
        let state = ipc::MixStationState::from_json(json).expect("legacy state must load");
        assert_eq!(state.params.filters_trim_db, 0.0);
        assert_eq!(state.params.limiter_trim_db, 0.0);
        // Slots keep their legacy defaults so an old rack still has its chain.
        assert_eq!(state.params.slot1_module, 1);
    }

    #[test]
    fn stage_meters_track_each_rack_position() {
        let mut params = bypassed();
        params.width_enabled = true;
        params.slot1_module = 5;
        params.sat_enabled = true;
        params.slot2_module = 4;
        params.sat_drive_pct = 60.0;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        dsp.reset();

        for n in 0..4_000 {
            let x = (n as f32 * std::f32::consts::TAU * 220.0 / SR).sin() * 0.6;
            let _ = dsp.process_stereo(x, x);
        }
        let frame = dsp.meter_frame();
        // The two loaded positions register signal; the empty ones read the
        // level passing through, and nothing is NaN.
        assert!(frame.slot_in_peak[0] > 0.1, "{:?}", frame.slot_in_peak);
        assert!(frame.slot_out_peak[1] > 0.0);
        for value in frame.slot_in_peak.iter().chain(frame.slot_out_peak.iter()) {
            assert!(value.is_finite());
        }
    }

    #[test]
    fn stage_meter_output_reflects_that_module_trim() {
        let mut params = bypassed();
        params.width_enabled = true;
        params.slot1_module = 5;
        params.width_trim_db = -24.0;
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params);
        dsp.reset();
        for _ in 0..8_000 {
            let _ = dsp.process_stereo(0.8, 0.8);
        }
        let frame = dsp.meter_frame();
        assert!(
            frame.slot_out_peak[0] < frame.slot_in_peak[0] * 0.2,
            "in {} out {}",
            frame.slot_in_peak[0],
            frame.slot_out_peak[0]
        );
    }

    #[test]
    fn clip_indicators_expire_after_the_defined_hold() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(bypassed());
        let _ = dsp.process_stereo(1.2, 1.2);
        assert!(dsp.meter_frame().in_clip);
        assert!(dsp.meter_frame().out_clip);
        for _ in 0..SR as usize {
            let _ = dsp.process_stereo(0.0, 0.0);
        }
        assert!(!dsp.meter_frame().in_clip);
        assert!(!dsp.meter_frame().out_clip);
    }

    #[test]
    fn extreme_wire_values_produce_only_finite_output() {
        let mut dsp = Dsp::new(SR);
        for index in 0..ipc::PARAM_COUNT as u32 {
            assert!(dsp.apply_wire_param(index, descriptor().params[index as usize].max));
        }
        for n in 0..20_000 {
            let x = (n as f32 * 0.719).sin() * 2.0;
            let (l, r) = dsp.process_stereo(x, -x * 0.37);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    #[test]
    fn the_module_curves_follow_the_strip() {
        let mut p = default_params();
        p.hpf_hz = 100.0;
        p.lpf_hz = 10_000.0;
        let cuts = filter_response_db(&p, 48_000.0, &[100.0, 1_000.0, 10_000.0]);
        assert!((cuts[0] + 3.0).abs() < 0.3, "{cuts:?}");
        assert!(cuts[1].abs() < 0.3, "{cuts:?}");
        assert!((cuts[2] + 3.0).abs() < 0.5, "{cuts:?}");
        // Parked at their open ends, the cuts are out of the path.
        let parked = Params {
            hpf_hz: 20.0,
            lpf_hz: 20_000.0,
            ..default_params()
        };
        let open = filter_response_db(&parked, 48_000.0, &[20.0, 19_000.0]);
        assert!(open.iter().all(|db| db.abs() < 1.0e-3), "{open:?}");

        p.low_mid_freq_hz = 400.0;
        p.low_mid_gain_db = 6.0;
        let eq = eq_response_db(&p, 48_000.0, &[400.0, 5_000.0]);
        assert!((eq[0] - 6.0).abs() < 0.3, "{eq:?}");
        assert!(eq[1].abs() < 0.3, "{eq:?}");

        p.comp_threshold_db = -20.0;
        p.comp_makeup_db = 2.0;
        assert!((comp_transfer_db(&p, -40.0) - -38.0).abs() < 1.0e-4);
        assert!(comp_transfer_db(&p, 0.0) < -10.0);
        p.limiter_ceiling_db = -1.0;
        assert!(limiter_transfer_db(&p, 6.0) <= -1.0);
        assert!((limiter_transfer_db(&p, -20.0) - -20.0).abs() < 1.0e-4);
    }
}
