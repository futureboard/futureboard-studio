//! WhiteSharp — realtime pitch correction (Auto mode) DSP processor.
//! Borrowed from `Futureboard-release/crates/BuiltinAudioPlugins/crates/whitesharp/src/lib.rs`.

use serde::{Deserialize, Serialize};
use crate::detect::{self, Detector, Estimate};
use crate::dsp_core::{Smoothed, StereoEffect, clamp, db_to_linear, smoothing_step};
use crate::scale::{ALL_NOTES, Scale, nearest_target, pitch_class, scale_mask};
use crate::shifter::{Control, MAX_FORMANT, Shifter};
use crate::splice::{self, Splicer};

pub const MAX_RETUNE_MS: f32 = 400.0;
const HUMANIZE_MS: f32 = 350.0;
const HUMANIZE_HOLD_MS: f32 = 400.0;
const VIBRATO_SPLIT_HZ: f32 = 3.0;
const RELEASE_MS: f32 = 150.0;
const SMOOTH_MS: f32 = 10.0;
const MODE_FADE_MS: f32 = 20.0;
const TYPE_FADE_MS: f32 = 10.0;
const LOWEST_HZ: f32 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputType {
    Soprano,
    AltoTenor,
    LowMale,
    Instrument,
    BassInstrument,
}

impl InputType {
    pub const ALL: [Self; 5] = [
        Self::Soprano,
        Self::AltoTenor,
        Self::LowMale,
        Self::Instrument,
        Self::BassInstrument,
    ];

    pub const fn range_hz(self) -> (f32, f32) {
        match self {
            Self::Soprano => (150.0, 1_100.0),
            Self::AltoTenor => (90.0, 750.0),
            Self::LowMale => (60.0, 450.0),
            Self::Instrument => (50.0, 1_600.0),
            Self::BassInstrument => (30.0, 450.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum VibratoShape {
    #[default]
    None,
    Sine,
    Square,
    Sawtooth,
}

impl VibratoShape {
    #[inline]
    pub fn at(self, phase: f32) -> f32 {
        let sine = (std::f32::consts::TAU * phase).sin();
        match self {
            Self::None => 0.0,
            Self::Sine => sine,
            Self::Square => (5.0 * sine).tanh() / 5.0f32.tanh(),
            Self::Sawtooth => 2.0 * phase.fract() - 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LatencyMode {
    #[default]
    Quality,
    Live,
}

pub fn latency_samples_for(sample_rate: f32, input_type: InputType, mode: LatencyMode) -> usize {
    match mode {
        LatencyMode::Live => splice::LATENCY,
        LatencyMode::Quality => {
            let sr = sample_rate.max(1.0);
            let (lowest, _) = input_type.range_hz();
            let (centre_lag, hop) = detect::timing(sr, lowest);
            Shifter::latency_for_input(sr, sr / LOWEST_HZ, sr / lowest, centre_lag + hop) as usize
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub power: bool,
    pub key: u8,
    pub scale: Scale,
    pub input_type: InputType,
    pub retune_ms: f32,
    pub humanize: f32,
    pub flex_tune: f32,
    pub vibrato_db: f32,
    pub formant: bool,
    pub throat: f32,
    pub transpose: f32,
    pub detune: f32,
    pub tracking: f32,
    pub remove_mask: u16,
    pub bypass_mask: u16,
    pub mix: f32,
    pub output_db: f32,
    pub classic: bool,
    pub vibrato_shape: VibratoShape,
    pub vibrato_rate_hz: f32,
    pub vibrato_delay_ms: f32,
    pub vibrato_onset_ms: f32,
    pub vibrato_pitch: f32,
    pub vibrato_amp: f32,
    pub vibrato_variation: f32,
    pub latency: LatencyMode,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        key: 9, // A
        scale: Scale::Minor,
        input_type: InputType::AltoTenor,
        retune_ms: 20.0,
        humanize: 0.0,
        flex_tune: 0.0,
        vibrato_db: 0.0,
        formant: true,
        throat: 100.0,
        transpose: 0.0,
        detune: 0.0,
        tracking: 50.0,
        remove_mask: 0,
        bypass_mask: 0,
        mix: 100.0,
        output_db: 0.0,
        classic: false,
        vibrato_shape: VibratoShape::None,
        vibrato_rate_hz: 5.5,
        vibrato_delay_ms: 500.0,
        vibrato_onset_ms: 300.0,
        vibrato_pitch: 35.0,
        vibrato_amp: 0.0,
        vibrato_variation: 0.0,
        latency: LatencyMode::Quality,
    }
}

impl Params {
    pub fn allowed_notes(&self) -> u16 {
        scale_mask(self.key, self.scale) & !self.remove_mask & ALL_NOTES
    }
}

#[derive(Debug, Clone, Copy)]
struct Tuning {
    allowed: u16,
    bypass: u16,
    threshold: f32,
    reference_hz: f32,
    retune_ms: f32,
    humanize_ms: f32,
    flex: Option<(f32, f32)>,
    vibrato_extra: f32,
    transpose_cents: f32,
    preserve_formants: bool,
    throat: f32,
    mix: f32,
    output_gain: f32,
    classic: bool,
    vibrato: VibratoShape,
    vibrato_rate: f32,
    vibrato_delay: f32,
    vibrato_onset: f32,
    vibrato_pitch: f32,
    vibrato_amp: f32,
    vibrato_variation: f32,
    live: bool,
}

impl Tuning {
    fn resolve(p: &Params) -> Self {
        let flex = clamp(p.flex_tune / 100.0, 0.0, 1.0);
        let inner = 15.0 + (1.0 - flex) * 585.0;
        Self {
            allowed: p.allowed_notes(),
            bypass: p.bypass_mask & ALL_NOTES,
            threshold: 0.06 + 0.24 * clamp(p.tracking / 100.0, 0.0, 1.0),
            reference_hz: 440.0 * 2f32.powf(clamp(p.detune, -100.0, 100.0) / 1_200.0),
            retune_ms: clamp(p.retune_ms, 0.0, MAX_RETUNE_MS),
            humanize_ms: clamp(p.humanize / 100.0, 0.0, 1.0) * HUMANIZE_MS,
            flex: (flex > 0.0).then_some((inner, inner + 25.0)),
            vibrato_extra: db_to_linear(clamp(p.vibrato_db, -12.0, 12.0)) - 1.0,
            transpose_cents: clamp(p.transpose.round(), -24.0, 24.0) * 100.0,
            preserve_formants: p.formant,
            throat: clamp(p.throat, 70.0, 140.0) / 100.0,
            mix: clamp(p.mix / 100.0, 0.0, 1.0),
            output_gain: db_to_linear(clamp(p.output_db, -24.0, 12.0)),
            classic: p.classic,
            vibrato: p.vibrato_shape,
            vibrato_rate: clamp(p.vibrato_rate_hz, 0.1, 10.0),
            vibrato_delay: clamp(p.vibrato_delay_ms, 0.0, 2_000.0) * 0.001,
            vibrato_onset: clamp(p.vibrato_onset_ms, 0.0, 2_000.0) * 0.001,
            vibrato_pitch: clamp(p.vibrato_pitch, 0.0, 100.0),
            vibrato_amp: clamp(p.vibrato_amp / 100.0, 0.0, 1.0),
            vibrato_variation: clamp(p.vibrato_variation / 100.0, 0.0, 1.0),
            live: p.latency == LatencyMode::Live,
        }
    }
}

#[inline]
pub fn midi_of(hz: f32, reference_hz: f32) -> f32 {
    69.0 + 12.0 * (hz / reference_hz).log2()
}

#[derive(Debug, Clone)]
pub struct WhiteSharpDsp {
    sample_rate: f32,
    params: Params,
    tuning: Tuning,
    detector: Detector,
    shifter: Shifter,
    retired: Shifter,
    type_fade: f32,
    type_step: f32,
    splicer: Splicer,
    mode_mix: f32,
    mode_step: f32,
    quality_running: bool,
    live_running: bool,
    now: u64,
    hop_seconds: f32,
    target: Option<i32>,
    held: f32,
    silent: f32,
    correction: f32,
    slow_pitch: Option<f32>,
    vibrato_step: f32,
    vibrato_phase: f32,
    vibrato_rate_factor: f32,
    vibrato_depth_factor: f32,
    seed: u32,
    smooth_step: f32,
    active: Smoothed,
    mix: Smoothed,
    output_gain: Smoothed,
    last_input_midi: Option<f32>,
    last_output_midi: Option<f32>,
}

impl WhiteSharpDsp {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let detector = Detector::new(sr, LOWEST_HZ);
        let shifter = Shifter::new(sr, sr / LOWEST_HZ);
        let retired = Shifter::new(sr, sr / LOWEST_HZ);
        let splicer = Splicer::new(sr, sr / LOWEST_HZ);
        let hop_seconds = detector.hop_samples() as f32 / sr;
        let params = default_params();
        let tuning = Tuning::resolve(&params);
        let mut dsp = Self {
            sample_rate: sr,
            tuning,
            detector,
            shifter,
            retired,
            type_fade: 1.0,
            type_step: 1.0 / (TYPE_FADE_MS * 0.001 * sr).max(1.0),
            splicer,
            mode_mix: if tuning.live { 1.0 } else { 0.0 },
            mode_step: 1.0 / (MODE_FADE_MS * 0.001 * sr).max(1.0),
            quality_running: true,
            live_running: true,
            now: 0,
            hop_seconds,
            target: None,
            held: 0.0,
            silent: 0.0,
            correction: 0.0,
            slow_pitch: None,
            vibrato_step: 1.0 - (-std::f32::consts::TAU * VIBRATO_SPLIT_HZ * hop_seconds).exp(),
            vibrato_phase: 0.0,
            vibrato_rate_factor: 1.0,
            vibrato_depth_factor: 1.0,
            seed: 0x2545_F491,
            smooth_step: smoothing_step(SMOOTH_MS, sr),
            active: Smoothed::at(1.0),
            mix: Smoothed::at(tuning.mix),
            output_gain: Smoothed::at(tuning.output_gain),
            params,
            last_input_midi: None,
            last_output_midi: None,
        };
        dsp.apply_input_type();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        let input_changed = params.input_type != self.params.input_type;
        self.params = params;
        self.retune();
        if input_changed {
            self.change_input_type();
        }
    }

    pub fn latency_samples(&self) -> usize {
        match self.params.latency {
            LatencyMode::Quality => self.shifter.latency() as usize,
            LatencyMode::Live => self.splicer.latency(),
        }
    }

    pub fn last_reading(&self) -> (Option<f32>, Option<f32>, Option<i32>) {
        (self.last_input_midi, self.last_output_midi, self.target)
    }

    fn retune(&mut self) {
        self.tuning = Tuning::resolve(&self.params);
        self.mix.target = self.tuning.mix;
        self.output_gain.target = self.tuning.output_gain;
        self.active.target = if self.params.power { 1.0 } else { 0.0 };
        if self.now == 0 {
            self.mix.settle();
            self.output_gain.settle();
            self.active.settle();
            self.settle_mode();
        }
    }

    fn settle_mode(&mut self) {
        self.mode_mix = if self.tuning.live { 1.0 } else { 0.0 };
    }

    fn change_input_type(&mut self) {
        let audible = self.now > 0 && self.quality_running && self.mode_mix < 1.0;
        if audible {
            if self.type_fade < 0.5 {
                std::mem::swap(&mut self.shifter, &mut self.retired);
            }
            std::mem::swap(&mut self.shifter, &mut self.retired);
            self.type_fade = 0.0;
        } else {
            self.type_fade = 1.0;
        }
        self.apply_input_type();
    }

    fn apply_input_type(&mut self) {
        let (lowest, highest) = self.params.input_type.range_hz();
        self.detector.set_range(lowest, highest);
        self.shifter.set_longest_period(
            self.sample_rate / lowest,
            self.detector.centre_lag() + self.detector.hop_samples(),
        );
        self.target = None;
        self.correction = 0.0;
        self.slow_pitch = None;
    }

    fn on_estimate(&mut self, estimate: Estimate) {
        let t = self.tuning;
        let centre = self.now.saturating_sub(self.detector.centre_lag() as u64);
        let hop = self.hop_seconds;
        let Some(period) = estimate.period else {
            self.silent += hop;
            if self.silent * 1_000.0 >= RELEASE_MS {
                self.target = None;
                self.correction = 0.0;
            }
            self.slow_pitch = None;
            self.vibrato_phase = 0.0;
            let control = Control {
                centre,
                period: None,
                ratio: 1.0,
                formant: 1.0,
                gain: 1.0,
            };
            self.shifter.push_control(control);
            self.retired.push_control(control);
            self.splicer.set_control(None, 1.0, 1.0);
            self.last_input_midi = None;
            self.last_output_midi = None;
            return;
        };
        self.silent = 0.0;

        let midi = midi_of(self.sample_rate / period, t.reference_hz);
        let held_note = if t.classic { None } else { self.target };
        let target = nearest_target(midi, t.allowed, held_note);
        let new_note = target != self.target;
        if new_note {
            self.held = 0.0;
        }
        self.target = target;
        self.held += hop;

        let slow = match self.slow_pitch {
            Some(slow) if !new_note => slow + (midi - slow) * self.vibrato_step,
            _ => midi,
        };
        self.slow_pitch = Some(slow);
        let vibrato = clamp(midi - slow, -2.0, 2.0);

        let corrected = target.filter(|note| t.bypass & (1 << pitch_class(*note)) == 0);
        let desired = match corrected {
            Some(note) => {
                let cents = (note as f32 - midi) * 100.0;
                let weight = match t.flex.filter(|_| !t.classic) {
                    Some((inner, outer)) => {
                        clamp((outer - cents.abs()) / (outer - inner), 0.0, 1.0)
                    }
                    None => 1.0,
                };
                cents * weight
            }
            None => 0.0,
        };
        if t.classic {
            let step = if t.retune_ms <= 0.0 {
                f32::INFINITY
            } else {
                100.0 * hop * 1_000.0 / t.retune_ms
            };
            self.correction += clamp(desired - self.correction, -step, step);
        } else {
            let hold = clamp(self.held * 1_000.0 / HUMANIZE_HOLD_MS, 0.0, 1.0);
            let tau_ms = t.retune_ms + t.humanize_ms * hold;
            let pull = if tau_ms <= 0.0 {
                1.0
            } else {
                1.0 - (-hop * 1_000.0 / tau_ms).exp()
            };
            self.correction += (desired - self.correction) * pull;
        }

        let (created_cents, gain) = if corrected.is_some() {
            self.created_vibrato()
        } else {
            self.vibrato_phase = 0.0;
            (0.0, 1.0)
        };
        let cents =
            self.correction + vibrato * 100.0 * t.vibrato_extra + created_cents + t.transpose_cents;
        let ratio = 2f32.powf(cents / 1_200.0);
        let formant = if t.preserve_formants { 1.0 } else { ratio } / t.throat;
        let control = Control {
            centre,
            period: Some(period),
            ratio,
            formant: clamp(formant, 1.0 / MAX_FORMANT, MAX_FORMANT),
            gain,
        };
        self.shifter.push_control(control);
        self.retired.push_control(control);
        self.splicer.set_control(Some(period), ratio, gain);
        self.last_input_midi = Some(midi);
        self.last_output_midi = Some(midi + cents / 100.0);
    }

    #[inline]
    fn play_paths(&mut self, left: f32, right: f32) -> ([f32; 2], [f32; 2]) {
        let target = if self.tuning.live { 1.0 } else { 0.0 };
        if self.mode_mix != target {
            self.mode_mix = if self.mode_mix < target {
                (self.mode_mix + self.mode_step).min(target)
            } else {
                (self.mode_mix - self.mode_step).max(target)
            };
        }
        let m = self.mode_mix;
        let quality = if m < 1.0 {
            if !self.quality_running {
                self.shifter.resume();
                self.quality_running = true;
                self.type_fade = 1.0;
            }
            let new = self.shifter.process(left, right);
            if self.type_fade < 1.0 {
                self.type_fade = (self.type_fade + self.type_step).min(1.0);
                let old = self.retired.process(left, right);
                let x = self.type_fade;
                let angle = std::f32::consts::FRAC_PI_2 * x * x * (3.0 - 2.0 * x);
                let (g_old, g_new) = (angle.cos(), angle.sin());
                let blend = |a: [f32; 2], b: [f32; 2]| {
                    [a[0] * g_old + b[0] * g_new, a[1] * g_old + b[1] * g_new]
                };
                Some((blend(old.0, new.0), blend(old.1, new.1)))
            } else {
                self.retired.skip(left, right);
                Some(new)
            }
        } else {
            self.shifter.skip(left, right);
            self.retired.skip(left, right);
            self.quality_running = false;
            None
        };
        let live = if m > 0.0 {
            if !self.live_running {
                self.splicer.restart();
                self.live_running = true;
            }
            Some(self.splicer.process(left, right))
        } else {
            self.splicer.skip(left, right);
            self.live_running = false;
            None
        };
        match (quality, live) {
            (Some(q), Some(z)) => {
                let angle = std::f32::consts::FRAC_PI_2 * m;
                let (gq, gz) = (angle.cos(), angle.sin());
                let blend =
                    |a: [f32; 2], b: [f32; 2]| [a[0] * gq + b[0] * gz, a[1] * gq + b[1] * gz];
                (blend(q.0, z.0), blend(q.1, z.1))
            }
            (Some(path), None) | (None, Some(path)) => path,
            (None, None) => ([0.0; 2], [0.0; 2]),
        }
    }

    fn created_vibrato(&mut self) -> (f32, f32) {
        let t = self.tuning;
        if t.vibrato == VibratoShape::None {
            return (0.0, 1.0);
        }
        let since = self.held - t.vibrato_delay;
        if since <= 0.0 {
            self.vibrato_phase = 0.0;
            return (0.0, 1.0);
        }
        let envelope = if t.vibrato_onset <= 0.0 {
            1.0
        } else {
            clamp(since / t.vibrato_onset, 0.0, 1.0)
        };
        self.vibrato_phase += t.vibrato_rate * self.vibrato_rate_factor * self.hop_seconds;
        if self.vibrato_phase >= 1.0 {
            self.vibrato_phase -= self.vibrato_phase.floor();
            let mut wander = || {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 17;
                self.seed ^= self.seed << 5;
                (self.seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
            };
            let (rate, depth) = (wander(), wander());
            self.vibrato_rate_factor = 1.0 + 0.3 * t.vibrato_variation * rate;
            self.vibrato_depth_factor = (1.0 + 0.5 * t.vibrato_variation * depth).max(0.0);
        }
        let wave = t.vibrato.at(self.vibrato_phase) * envelope * self.vibrato_depth_factor;
        (wave * t.vibrato_pitch, 1.0 + 0.5 * t.vibrato_amp * wave)
    }
}

impl StereoEffect for WhiteSharpDsp {
    fn reset(&mut self) {
        self.detector.reset();
        self.shifter.reset();
        self.retired.reset();
        self.type_fade = 1.0;
        self.splicer.reset();
        self.settle_mode();
        self.quality_running = true;
        self.live_running = true;
        self.target = None;
        self.correction = 0.0;
        self.slow_pitch = None;
        self.held = 0.0;
        self.silent = 0.0;
        self.vibrato_phase = 0.0;
        self.last_input_midi = None;
        self.last_output_midi = None;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        if (sr - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        let params = self.params.clone();
        *self = Self::new(sr);
        self.set_params(params);
        self.apply_input_type();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if let Some(estimate) = self
            .detector
            .push((left + right) * 0.5, self.tuning.threshold)
        {
            self.on_estimate(estimate);
        }
        let (wet, dry) = self.play_paths(left, right);
        self.now += 1;

        let step = self.smooth_step;
        let active = self.active.next(step);
        let mix = self.mix.next(step);
        let gain = self.output_gain.next(step);
        let out = |side: usize| {
            let processed = (dry[side] + (wet[side] - dry[side]) * mix) * gain;
            dry[side] + (processed - dry[side]) * active
        };
        (out(0), out(1))
    }
}
