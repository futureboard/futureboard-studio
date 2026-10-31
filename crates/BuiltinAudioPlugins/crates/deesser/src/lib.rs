//! De-Esser — split-band and wideband sibilance control.
//!
//! ```text
//!          ┌─ band-pass ─ peak envelope ─ gain computer ─┐ (detector, undelayed)
//! in ─ M/S ┤                                             ▼
//!          └─ lookahead ─┬─ split: x − (1 − g)·bp(x) ────┬─ mix ─ M/S ─ output ─ out
//!                        └─ wide:  x · g ────────────────┘
//! ```
//!
//! The detector hears a band-pass around the frequency, as wide as the width
//! in octaves, on the linked stereo pair, the mid or the side. Its peak level
//! over the threshold, at a fixed 5:1, sets the cut — never more than the
//! range. Split mode takes the cut out of that same band only (the band-pass
//! subtracted back from the signal is a dip that is transparent elsewhere);
//! wideband turns the whole signal down. Listen plays what the detector
//! hears.
//!
//! # Realtime shape
//!
//! Everything is sized in [`Dsp::new`]: the lookahead rings hold the longest
//! lookahead at the highest supported rate. The audio path indexes and
//! multiplies; a new lookahead, mode or stereo mode dips the output for a few
//! milliseconds and switches at the bottom of the dip rather than clicking.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{DelayRing, Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    flush_denormal, linear_to_db, make_eq_coefficients, time_constant,
};
use serde::{Deserialize, Serialize};

pub mod ipc;
pub mod presets;
pub mod ui;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use presets::{FactoryPreset, factory_presets};

pub const PLUGIN_ID: &str = "futureboard.deesser";

/// The cut's slope over the threshold: a fixed 5:1.
pub const RATIO: f32 = 5.0;
/// The longest lookahead, and the highest rate its ring is sized for.
pub const MAX_LOOKAHEAD_MS: f32 = 10.0;
const MAX_SAMPLE_RATE: f32 = 384_000.0;
/// The detector's attack.
const ATTACK_MS: f32 = 0.5;
/// The applied cut's de-zipper.
const GAIN_SMOOTH_MS: f32 = 1.0;
const CLIP_THRESHOLD: f32 = 1.0;
const RMS_WINDOW_SECONDS: f32 = 0.300;
const PEAK_FALL_SECONDS: f32 = 0.400;
/// Mix and output smoothing.
const SMOOTH_MS: f32 = 20.0;
/// The power crossfade.
const FADE_MS: f32 = 10.0;
/// Each half of the dip a lookahead, mode or stereo-mode change rides.
const SWITCH_MS: f32 = 4.0;
/// How long the held peak cut stays before it falls, and how fast it falls.
const PEAK_HOLD_SECONDS: f32 = 1.5;
const PEAK_HOLD_FALL_DB_PER_SECOND: f32 = 12.0;
/// Host samples between filter retunes while the band moves.
const CONTROL_TICK: u32 = 32;
/// Share of the way a moving frequency or width goes per control tick.
const CONTROL_FOLLOW: f32 = 0.2;
/// The detector level's floor, in dB.
const FLOOR_DB: f32 = -120.0;

/// How the cut is taken. Wire order is the persisted contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Only the detected band is turned down.
    Split,
    /// The whole signal is turned down.
    Wide,
}

impl Mode {
    pub const fn to_wire(self) -> f32 {
        match self {
            Self::Split => 0.0,
            Self::Wide => 1.0,
        }
    }

    pub fn from_wire(value: f32) -> Self {
        if value.round() as i32 == 1 {
            Self::Wide
        } else {
            Self::Split
        }
    }
}

/// What part of the stereo image is detected and processed; the rest passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StereoMode {
    Stereo,
    Mid,
    Side,
}

impl StereoMode {
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Params {
    pub power: bool,
    pub mode: Mode,
    pub stereo_mode: StereoMode,
    /// The detected band's centre.
    pub freq_hz: f32,
    /// The detected band's width, in octaves.
    pub width_oct: f32,
    pub threshold_db: f32,
    /// The most the cut takes off, dB.
    pub range_db: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    /// Wet share, percent.
    pub mix: f32,
    pub output_db: f32,
    /// Play the detector's band instead of the processed signal.
    pub listen: bool,
}

impl Default for Params {
    fn default() -> Self {
        default_params()
    }
}

pub fn default_params() -> Params {
    Params {
        power: true,
        mode: Mode::Split,
        stereo_mode: StereoMode::Stereo,
        freq_hz: 7_500.0,
        width_oct: 1.2,
        threshold_db: -30.0,
        range_db: 10.0,
        release_ms: 60.0,
        lookahead_ms: 2.0,
        mix: 100.0,
        output_db: 0.0,
        listen: false,
    }
}

/// The band-pass Q for a width of `octaves`.
pub fn width_q(octaves: f32) -> f32 {
    let n = clamp(octaves, 0.05, 6.0);
    let k = 2f32.powf(n);
    k.sqrt() / (k - 1.0)
}

/// The detector's band-pass coefficients: constant 0 dB peak at `freq_hz`.
pub fn band_coefficients(freq_hz: f32, width_oct: f32, sample_rate: f32) -> Coefficients<f32> {
    // `biquad`'s band-pass keeps its skirts at 0 dB and peaks at Q; scaled by
    // 1/Q it peaks at 0 dB, so `x − bp(x)` nulls the centre exactly.
    let q = clamp(width_q(width_oct), 0.1, 12.0);
    match make_eq_coefficients("bandpass", freq_hz, 0.0, q, sample_rate) {
        Some(c) => Coefficients {
            b0: c.b0 / q,
            b1: c.b1 / q,
            b2: c.b2 / q,
            ..c
        },
        None => Coefficients {
            a1: 0.0,
            a2: 0.0,
            b0: 0.0,
            b1: 0.0,
            b2: 0.0,
        },
    }
}

/// The cut, in positive dB, for a detector level of `detector_db`.
pub fn cut_db(params: &Params, detector_db: f32) -> f32 {
    clamp(
        (detector_db - params.threshold_db) * (1.0 - 1.0 / RATIO),
        0.0,
        params.range_db.max(0.0),
    )
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
    param("mode", "Mode", 0.0, 0.0, 1.0, "enum"),
    param("stereoMode", "Process", 0.0, 0.0, 2.0, "enum"),
    param("freqHz", "Frequency", 7_500.0, 2_000.0, 16_000.0, "Hz"),
    param("widthOct", "Width", 1.2, 0.3, 3.0, "oct"),
    param("thresholdDb", "Threshold", -30.0, -60.0, 0.0, "dB"),
    param("rangeDb", "Range", 10.0, 0.0, 24.0, "dB"),
    param("releaseMs", "Release", 60.0, 5.0, 300.0, "ms"),
    param("lookaheadMs", "Lookahead", 2.0, 0.0, 10.0, "ms"),
    param("mix", "Mix", 100.0, 0.0, 100.0, "%"),
    param("outputDb", "Output", 0.0, -12.0, 12.0, "dB"),
    param("listen", "Listen", 0.0, 0.0, 1.0, "bool"),
];

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "De-Esser",
        vendor: "Futureboard",
        category: PluginCategory::Effect,
        version: env!("CARGO_PKG_VERSION"),
        params: PARAMS,
    }
}

/// Telemetry the editor reads from the host's pad-level block.
pub mod telemetry {
    /// Slots in the block (the host's `BUILTIN_PAD_SLOTS`).
    pub const SLOTS: usize = 64;
    /// The cut applied now, positive dB (0 when bypassed).
    pub const GR_DB: usize = 0;
    /// The largest cut over the last 1.5 s, then falling.
    pub const GR_PEAK_DB: usize = 1;
    /// The detector band's level, dBFS.
    pub const DETECTOR_DB: usize = 2;
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gain_reduction_db: f32,
    /// The detector's level (linear peak): what the host's meter frame
    /// carries in slot 0's input peak, so the editor's history records it.
    pub detector_peak: f32,
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
            flush_denormal(self.in_peak * self.peak_coeff)
        };
        self.out_peak = if out_abs > self.out_peak {
            out_abs
        } else {
            flush_denormal(self.out_peak * self.peak_coeff)
        };
        let input_ms = (in_l * in_l + in_r * in_r) * 0.5;
        let output_ms = (out_l * out_l + out_r * out_r) * 0.5;
        self.in_ms =
            flush_denormal(self.rms_coeff * self.in_ms + (1.0 - self.rms_coeff) * input_ms);
        self.out_ms =
            flush_denormal(self.rms_coeff * self.out_ms + (1.0 - self.rms_coeff) * output_ms);
        if in_abs >= CLIP_THRESHOLD {
            self.in_clip = true;
        }
        if out_abs >= CLIP_THRESHOLD {
            self.out_clip = true;
        }
    }
}

/// Eased 0…1: zero slope at both ends.
#[inline]
fn ease(pos: f32) -> f32 {
    pos * pos * (3.0 - 2.0 * pos)
}

fn ramp_step(ms: f32, sample_rate: f32) -> f32 {
    1.0 / (ms * 0.001 * sample_rate.max(1.0)).max(1.0)
}

/// A 0…1 crossfade position moving linearly toward its target, read eased.
#[derive(Debug, Clone, Copy)]
struct Fade {
    pos: f32,
    target: f32,
    step: f32,
}

impl Fade {
    fn new(on: bool, sample_rate: f32) -> Self {
        let at = if on { 1.0 } else { 0.0 };
        Self {
            pos: at,
            target: at,
            step: ramp_step(FADE_MS, sample_rate),
        }
    }

    fn set(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
    }

    fn settle(&mut self) {
        self.pos = self.target;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        if self.pos < self.target {
            self.pos = (self.pos + self.step).min(self.target);
        } else if self.pos > self.target {
            self.pos = (self.pos - self.step).max(self.target);
        }
        ease(self.pos)
    }
}

/// `a` at `t` 0, `b` at `t` 1 (both exactly).
#[inline]
fn blend(a: f32, b: f32, t: f32) -> f32 {
    if t <= 0.0 {
        a
    } else if t >= 1.0 {
        b
    } else {
        a + (b - a) * t
    }
}

/// One smoothed value that lands exactly once close.
#[inline]
fn glide(value: &mut Smoothed, step: f32) -> f32 {
    if value.value != value.target {
        value.next(step);
        if (value.target - value.value).abs() <= 1.0e-6 * (1.0 + value.target.abs()) {
            value.settle();
        }
    }
    value.value
}

/// What the dip switches between: the settings a click would come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    lookahead: usize,
    mode: Mode,
    stereo_mode: StereoMode,
    listen: bool,
}

fn lookahead_samples(ms: f32, sample_rate: f32) -> usize {
    let max = (MAX_LOOKAHEAD_MS * 0.001 * sample_rate).round() as usize;
    ((clamp(ms, 0.0, MAX_LOOKAHEAD_MS) * 0.001 * sample_rate).round() as usize).min(max)
}

impl Layout {
    fn of(params: &Params, sample_rate: f32) -> Self {
        Self {
            lookahead: lookahead_samples(params.lookahead_ms, sample_rate),
            mode: params.mode,
            stereo_mode: params.stereo_mode,
            listen: params.listen,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    params: Params,
    /// Audio has run since construction or a reset; until it has, edits land
    /// at once.
    started: bool,
    sample_rate: f32,
    /// The detector's band-pass on path A (left or mid) and B (right or
    /// side), on the undelayed input.
    det_a: DirectForm1<f32>,
    det_b: DirectForm1<f32>,
    /// The same band on the delayed signal, for the split cut and listen.
    band_a: DirectForm1<f32>,
    band_b: DirectForm1<f32>,
    /// The band the filters are tuned to now, gliding toward the params.
    tuned_hz: f32,
    tuned_oct: f32,
    tick: u32,
    /// The lookahead: the input, delayed.
    ring_l: DelayRing,
    ring_r: DelayRing,
    active: Layout,
    switch_env: f32,
    switch_step: f32,
    /// The detector's peak envelope (linear) and its coefficients.
    envelope: f32,
    attack: f32,
    release: f32,
    /// The cut applied now, positive dB.
    cut: f32,
    cut_step: f32,
    /// The held peak cut and how long it still holds.
    cut_peak: f32,
    peak_hold: u32,
    peak_hold_len: u32,
    peak_fall: f32,
    mix: Smoothed,
    output: Smoothed,
    smooth_step: f32,
    power: Fade,
    meters: Meters,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let params = default_params();
        let capacity = (MAX_LOOKAHEAD_MS * 0.001 * MAX_SAMPLE_RATE).ceil() as usize + 8;
        let band = band_coefficients(params.freq_hz, params.width_oct, sr);
        let mut dsp = Self {
            started: false,
            sample_rate: sr,
            det_a: DirectForm1::<f32>::new(band),
            det_b: DirectForm1::<f32>::new(band),
            band_a: DirectForm1::<f32>::new(band),
            band_b: DirectForm1::<f32>::new(band),
            tuned_hz: params.freq_hz,
            tuned_oct: params.width_oct,
            tick: 0,
            ring_l: DelayRing::new(capacity),
            ring_r: DelayRing::new(capacity),
            active: Layout::of(&params, sr),
            switch_env: 1.0,
            switch_step: ramp_step(SWITCH_MS, sr),
            envelope: 0.0,
            attack: time_constant(sr, ATTACK_MS * 0.001),
            release: time_constant(sr, params.release_ms * 0.001),
            cut: 0.0,
            cut_step: smoothing_step(GAIN_SMOOTH_MS, sr),
            cut_peak: 0.0,
            peak_hold: 0,
            peak_hold_len: (PEAK_HOLD_SECONDS * sr) as u32,
            peak_fall: PEAK_HOLD_FALL_DB_PER_SECOND / sr,
            mix: Smoothed::at(1.0),
            output: Smoothed::at(1.0),
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            power: Fade::new(true, sr),
            meters: Meters::new(sr),
            params,
        };
        dsp.apply_params();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter at once — a state load. Lands without a
    /// glide, a fade or a dip.
    pub fn set_params(&mut self, params: Params) {
        self.params = params;
        ipc::sanitize_params(&mut self.params);
        self.apply_params();
        self.settle();
    }

    pub fn apply_wire_param(&mut self, wire_index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params, wire_index, value) {
            return false;
        }
        self.apply_params();
        if !self.started {
            self.settle();
        }
        true
    }

    pub fn apply_ui_param(&mut self, id: &str, value: f32) -> bool {
        match ipc::ui_param_index(id) {
            Some(index) => self.apply_wire_param(index, value),
            None => false,
        }
    }

    /// The lookahead, at the host rate: what the host compensates. The dry
    /// and bypassed signal run at it too.
    pub fn latency_samples(&self) -> usize {
        lookahead_samples(self.params.lookahead_ms, self.sample_rate)
    }

    /// The cut applied now, positive dB; 0 when bypassed.
    pub fn gain_reduction_db(&self) -> f32 {
        if self.params.power && !self.params.listen {
            self.cut * self.mix.value
        } else {
            0.0
        }
    }

    pub fn detector_db(&self) -> f32 {
        linear_to_db(self.envelope).max(FLOOR_DB)
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            in_peak: self.meters.in_peak,
            in_rms: self.meters.in_ms.max(0.0).sqrt(),
            out_peak: self.meters.out_peak,
            out_rms: self.meters.out_ms.max(0.0).sqrt(),
            gain_reduction_db: self.gain_reduction_db(),
            detector_peak: self.envelope,
            in_clip: self.meters.in_clip,
            out_clip: self.meters.out_clip,
        }
    }

    pub fn clear_clip(&mut self) {
        self.meters.in_clip = false;
        self.meters.out_clip = false;
    }

    /// The editor's telemetry block ([`telemetry`]).
    pub fn telemetry(&self) -> [f32; telemetry::SLOTS] {
        let mut block = [0.0; telemetry::SLOTS];
        block[telemetry::GR_DB] = self.gain_reduction_db();
        block[telemetry::GR_PEAK_DB] = if self.params.power && !self.params.listen {
            self.cut_peak
        } else {
            0.0
        };
        block[telemetry::DETECTOR_DB] = self.detector_db();
        block
    }

    fn apply_params(&mut self) {
        let p = &self.params;
        self.mix.target = clamp(p.mix, 0.0, 100.0) / 100.0;
        self.output.target = db_to_linear(p.output_db);
        self.release = time_constant(self.sample_rate, p.release_ms.max(1.0) * 0.001);
        self.power.set(p.power);
    }

    /// Land everything on its target: values, power, layout, filters.
    fn settle(&mut self) {
        self.mix.settle();
        self.output.settle();
        self.power.settle();
        self.tuned_hz = self.params.freq_hz;
        self.tuned_oct = self.params.width_oct;
        self.retune();
        self.active = Layout::of(&self.params, self.sample_rate);
        self.switch_env = 1.0;
    }

    fn retune(&mut self) {
        let band = band_coefficients(self.tuned_hz, self.tuned_oct, self.sample_rate);
        for filter in [
            &mut self.det_a,
            &mut self.det_b,
            &mut self.band_a,
            &mut self.band_b,
        ] {
            filter.update_coefficients(band);
        }
    }

    /// Moves the band a step toward its target, retuning when it moved.
    fn control_tick(&mut self) {
        let mut moved = false;
        let target = self.params.freq_hz;
        if self.tuned_hz != target {
            let next = self.tuned_hz * (target / self.tuned_hz).powf(CONTROL_FOLLOW);
            self.tuned_hz = if (next / target - 1.0).abs() < 1.0e-3 {
                target
            } else {
                next
            };
            moved = true;
        }
        let target = self.params.width_oct;
        if self.tuned_oct != target {
            let next = self.tuned_oct + (target - self.tuned_oct) * CONTROL_FOLLOW;
            self.tuned_oct = if (target - next).abs() < 1.0e-3 {
                target
            } else {
                next
            };
            moved = true;
        }
        if moved {
            self.retune();
        }
    }

    fn adopt_layout(&mut self) {
        let next = Layout::of(&self.params, self.sample_rate);
        if next.stereo_mode != self.active.stereo_mode {
            self.det_a.reset_state();
            self.det_b.reset_state();
            self.band_a.reset_state();
            self.band_b.reset_state();
            self.envelope = 0.0;
        }
        self.active = next;
    }
}

impl StereoEffect for Dsp {
    fn reset(&mut self) {
        self.started = false;
        self.det_a.reset_state();
        self.det_b.reset_state();
        self.band_a.reset_state();
        self.band_b.reset_state();
        self.ring_l.clear();
        self.ring_r.clear();
        self.envelope = 0.0;
        self.cut = 0.0;
        self.cut_peak = 0.0;
        self.peak_hold = 0;
        self.meters.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        self.meters = Meters::new(sr);
        self.smooth_step = smoothing_step(SMOOTH_MS, sr);
        self.power.step = ramp_step(FADE_MS, sr);
        self.switch_step = ramp_step(SWITCH_MS, sr);
        self.attack = time_constant(sr, ATTACK_MS * 0.001);
        self.cut_step = smoothing_step(GAIN_SMOOTH_MS, sr);
        self.peak_hold_len = (PEAK_HOLD_SECONDS * sr) as u32;
        self.peak_fall = PEAK_HOLD_FALL_DB_PER_SECOND / sr;
        self.apply_params();
        self.settle();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.started = true;

        // A new lookahead, mode, stereo mode or listen: dip, switch at the
        // bottom, come back up.
        if Layout::of(&self.params, self.sample_rate) != self.active {
            self.switch_env = (self.switch_env - self.switch_step).max(0.0);
            if self.switch_env <= 0.0 {
                self.adopt_layout();
            }
        } else if self.switch_env < 1.0 {
            self.switch_env = (self.switch_env + self.switch_step).min(1.0);
        }

        self.tick += 1;
        if self.tick >= CONTROL_TICK {
            self.tick = 0;
            self.control_tick();
        }

        let mix = glide(&mut self.mix, self.smooth_step);
        let output = glide(&mut self.output, self.smooth_step);
        let mode = self.active.stereo_mode;

        // Detector, on the undelayed input.
        let level = match mode {
            StereoMode::Stereo => self.det_a.run(left).abs().max(self.det_b.run(right).abs()),
            StereoMode::Mid => self.det_a.run((left + right) * 0.5).abs(),
            StereoMode::Side => self.det_b.run((left - right) * 0.5).abs(),
        };
        let coeff = if level > self.envelope {
            self.attack
        } else {
            self.release
        };
        self.envelope = flush_denormal(coeff * self.envelope + (1.0 - coeff) * level);
        let target = cut_db(&self.params, linear_to_db(self.envelope));
        self.cut = flush_denormal(self.cut + (target - self.cut) * self.cut_step);
        if self.cut >= self.cut_peak {
            self.cut_peak = self.cut;
            self.peak_hold = self.peak_hold_len;
        } else if self.peak_hold > 0 {
            self.peak_hold -= 1;
        } else {
            self.cut_peak = (self.cut_peak - self.peak_fall).max(self.cut);
        }
        let g = db_to_linear(-self.cut);

        // The audio, at the lookahead.
        self.ring_l.push(left);
        self.ring_r.push(right);
        let delay = self.active.lookahead;
        let (dl, dr) = if delay == 0 {
            (left, right)
        } else {
            (self.ring_l.at(delay + 1), self.ring_r.at(delay + 1))
        };
        let (a, b) = match mode {
            StereoMode::Stereo => (dl, dr),
            StereoMode::Mid | StereoMode::Side => ((dl + dr) * 0.5, (dl - dr) * 0.5),
        };
        let (on_a, on_b) = match mode {
            StereoMode::Stereo => (true, true),
            StereoMode::Mid => (true, false),
            StereoMode::Side => (false, true),
        };
        let band_a = if on_a { self.band_a.run(a) } else { 0.0 };
        let band_b = if on_b { self.band_b.run(b) } else { 0.0 };
        let listen = self.active.listen;
        let wide = self.active.mode == Mode::Wide;
        let process = |x: f32, band: f32, on: bool| -> f32 {
            if !on {
                if listen { 0.0 } else { x }
            } else if listen {
                band
            } else {
                let wet = if wide { x * g } else { x - (1.0 - g) * band };
                mix * wet + (1.0 - mix) * x
            }
        };
        let pa = process(a, band_a, on_a);
        let pb = process(b, band_b, on_b);
        let (pl, pr) = match mode {
            StereoMode::Stereo => (pa, pb),
            StereoMode::Mid | StereoMode::Side => (pa + pb, pa - pb),
        };

        let on = self.power.next();
        let envelope = ease(self.switch_env);
        let mut out_l = blend(dl, pl * output, on);
        let mut out_r = blend(dr, pr * output, on);
        if envelope < 1.0 {
            out_l *= envelope;
            out_r *= envelope;
        }
        self.meters.push_stereo(left, right, out_l, out_r);
        (out_l, out_r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> f32 {
        amp * (std::f32::consts::TAU * freq * n as f32 / SR).sin()
    }

    /// The output's peak over the last half of `n` samples of `input`.
    fn run(dsp: &mut Dsp, n: usize, input: impl Fn(usize) -> f32) -> f32 {
        let mut peak = 0.0f32;
        for i in 0..n {
            let x = input(i);
            let (l, r) = dsp.process_stereo(x, x);
            assert!(l.is_finite() && r.is_finite());
            if i > n / 2 {
                peak = peak.max(l.abs());
            }
        }
        peak
    }

    #[test]
    fn the_band_pass_peaks_at_unity_on_its_centre() {
        let band = band_coefficients(7_500.0, 1.2, SR);
        let at = builtin_dsp_core::biquad_response_db(&band, 7_500.0, SR);
        assert!(at.abs() < 0.05, "{at}");
        assert!(builtin_dsp_core::biquad_response_db(&band, 300.0, SR) < -20.0);
        // About 1.2 octaves wide at −3 dB (the bilinear warp near the top
        // bends the upper edge a little).
        let below = 7_500.0 * 2f32.powf(-0.6);
        let at_edge = builtin_dsp_core::biquad_response_db(&band, below, SR);
        assert!((at_edge + 3.0).abs() < 1.0, "{at_edge}");
    }

    #[test]
    fn the_cut_follows_the_threshold_and_stops_at_the_range() {
        let p = default_params();
        assert_eq!(cut_db(&p, -40.0), 0.0);
        assert!((cut_db(&p, -25.0) - 4.0).abs() < 1.0e-4);
        assert_eq!(cut_db(&p, 0.0), 10.0);
    }

    #[test]
    fn latency_is_the_lookahead() {
        let mut dsp = Dsp::new(SR);
        assert_eq!(dsp.latency_samples(), 96);
        assert!(dsp.apply_ui_param("lookaheadMs", 0.0));
        assert_eq!(dsp.latency_samples(), 0);
        assert!(dsp.apply_ui_param("lookaheadMs", 50.0));
        assert_eq!(dsp.latency_samples(), 480);
        // An impulse comes out exactly that late.
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            threshold_db: 0.0,
            ..default_params()
        });
        let delay = dsp.latency_samples();
        let mut first = None;
        for i in 0..400 {
            let (l, _) = dsp.process_stereo(if i == 0 { 1.0 } else { 0.0 }, 0.0);
            if first.is_none() && l.abs() > 0.5 {
                first = Some(i);
            }
        }
        assert_eq!(first, Some(delay));
    }

    #[test]
    fn split_cuts_sibilance_and_leaves_the_low_end_alone() {
        let mut dsp = Dsp::new(SR);
        let loud = run(&mut dsp, 24_000, |i| sine(7_500.0, 0.5, i));
        let cut = dsp.gain_reduction_db();
        assert!((cut - 10.0).abs() < 0.2, "the range caps the cut: {cut}");
        assert!(
            (linear_to_db(loud) - (linear_to_db(0.5) - 10.0)).abs() < 1.0,
            "{}",
            linear_to_db(loud)
        );
        let mut dsp = Dsp::new(SR);
        let low = run(&mut dsp, 24_000, |i| sine(300.0, 0.5, i));
        assert!(dsp.gain_reduction_db() < 0.01);
        assert!((low - 0.5).abs() < 0.01, "{low}");
    }

    #[test]
    fn split_leaves_a_low_tone_alone_while_it_cuts_the_band() {
        let mut dsp = Dsp::new(SR);
        // Low tone plus sibilance: the low tone passes at its level.
        let mut low_out = 0.0f64;
        let mut low_in = 0.0f64;
        let n = 48_000;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let x = sine(300.0, 0.3, i) + sine(8_000.0, 0.3, i);
            out.push(dsp.process_stereo(x, x).0);
        }
        let delay = dsp.latency_samples();
        for i in n / 2..n {
            let ph = std::f64::consts::TAU * 300.0 * (i - delay) as f64 / f64::from(SR);
            low_out += f64::from(out[i]) * ph.sin();
            low_in += 0.3 * ph.sin() * ph.sin();
        }
        assert!(dsp.gain_reduction_db() > 5.0);
        assert!(
            (low_out / low_in - 1.0).abs() < 0.05,
            "{}",
            low_out / low_in
        );
    }

    #[test]
    fn wide_turns_everything_down() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            mode: Mode::Wide,
            ..default_params()
        });
        let n = 48_000;
        let mut peak_low = 0.0f32;
        for i in 0..n {
            let x = sine(300.0, 0.3, i) + sine(8_000.0, 0.3, i);
            let (l, _) = dsp.process_stereo(x, x);
            if i > n / 2 {
                // The low tone's share, through a crude comparison: the whole
                // output is lower than the input's low tone alone.
                peak_low = peak_low.max(l.abs());
            }
        }
        assert!(dsp.gain_reduction_db() > 5.0);
        assert!(peak_low < 0.45, "{peak_low}");
    }

    #[test]
    fn listen_plays_the_band() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            listen: true,
            ..default_params()
        });
        let low = run(&mut dsp, 24_000, |i| sine(300.0, 0.5, i));
        assert!(low < 0.02, "{low}");
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            listen: true,
            ..default_params()
        });
        let high = run(&mut dsp, 24_000, |i| sine(7_500.0, 0.5, i));
        assert!((high - 0.5).abs() < 0.02, "{high}");
    }

    #[test]
    fn power_off_is_transparent_at_the_same_latency() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            power: false,
            ..default_params()
        });
        let delay = dsp.latency_samples();
        let input: Vec<f32> = (0..4_800).map(|i| sine(7_500.0, 0.8, i)).collect();
        for (i, &x) in input.iter().enumerate() {
            let (l, r) = dsp.process_stereo(x, -x);
            let expected = if i >= delay { input[i - delay] } else { 0.0 };
            assert_eq!(l, expected);
            assert_eq!(r, -expected);
        }
        assert_eq!(dsp.gain_reduction_db(), 0.0);
    }

    #[test]
    fn mid_mode_leaves_the_side_alone() {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(Params {
            stereo_mode: StereoMode::Mid,
            ..default_params()
        });
        // Pure side sibilance: the mid detector hears nothing.
        let mut peak = 0.0f32;
        for i in 0..24_000 {
            let x = sine(7_500.0, 0.5, i);
            let (l, _) = dsp.process_stereo(x, -x);
            if i > 12_000 {
                peak = peak.max(l.abs());
            }
        }
        assert!(dsp.gain_reduction_db() < 0.01);
        assert!((peak - 0.5).abs() < 0.01);
    }

    #[test]
    fn edits_mid_stream_stay_finite_and_telemetry_reports_the_cut() {
        let mut dsp = Dsp::new(SR);
        let mut seed = 1u32;
        for i in 0..96_000 {
            if i % 9_000 == 0 {
                let k = (i / 9_000) as f32;
                dsp.apply_ui_param("freqHz", 2_000.0 + k * 1_300.0);
                dsp.apply_ui_param("mode", k % 2.0);
                dsp.apply_ui_param("stereoMode", k % 3.0);
                dsp.apply_ui_param("lookaheadMs", k);
            }
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
            let (l, r) = dsp.process_stereo(noise, noise * 0.7);
            assert!(l.is_finite() && r.is_finite());
            assert!(l.abs() < 2.0 && r.abs() < 2.0);
        }
        let block = dsp.telemetry();
        assert!(block[telemetry::GR_PEAK_DB] >= block[telemetry::GR_DB]);
        assert!(block[telemetry::DETECTOR_DB] > -60.0);
        let frame = dsp.meter_frame();
        assert!(frame.detector_peak > 0.0 && frame.in_peak > 0.0);
    }
}
