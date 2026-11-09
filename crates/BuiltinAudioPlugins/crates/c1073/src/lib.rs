//! C1073 — 3-band channel EQ with preamp drive (Neve 1073-inspired).
//!
//! Phase 3 (harder). Tone stages use the MIT/Apache [`biquad`] crate:
//! high-pass, low shelf, mid bell (selectable center), high shelf, plus
//! mild class-A style tanh saturation.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{smoothing_step, Smoothed};
use builtin_dsp_core::{
    clamp, db_to_linear, make_eq_coefficients, mix, ParamDescriptor, PluginCategory,
    PluginDescriptor, StereoEffect,
};

pub const PLUGIN_ID: &str = "futureboard.c1073";

/// Classic mid-band center frequencies (Hz).
pub const MID_FREQ_CHOICES: [f32; 6] = [360.0, 700.0, 1_600.0, 3_200.0, 4_800.0, 7_200.0];

#[derive(Debug, Clone)]
pub struct Params {
    pub power: bool,
    /// Mic/line preamp drive into the EQ (dB).
    pub preamp_db: f32,
    pub highpass_hz: f32,
    pub highpass_enabled: bool,
    pub low_shelf_db: f32,
    pub low_shelf_hz: f32,
    pub mid_db: f32,
    pub mid_hz: f32,
    pub mid_q: f32,
    pub high_shelf_db: f32,
    pub high_shelf_hz: f32,
    pub output_db: f32,
    pub drive: f32,
    pub mix: f32,
}

pub fn default_params() -> Params {
    Params {
        power: true,
        preamp_db: 0.0,
        highpass_hz: 50.0,
        highpass_enabled: true,
        low_shelf_db: 0.0,
        low_shelf_hz: 220.0,
        mid_db: 0.0,
        mid_hz: 1_600.0,
        mid_q: 0.9,
        high_shelf_db: 0.0,
        high_shelf_hz: 12_000.0,
        output_db: 0.0,
        drive: 12.0,
        mix: 100.0,
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "C1073",
        vendor: "Futureboard Digital Technologies",
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
                id: "preampDb",
                name: "Preamp",
                default_value: 0.0,
                min: -12.0,
                max: 36.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "lowShelfDb",
                name: "Low",
                default_value: 0.0,
                min: -18.0,
                max: 18.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "midDb",
                name: "Mid",
                default_value: 0.0,
                min: -18.0,
                max: 18.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "highShelfDb",
                name: "High",
                default_value: 0.0,
                min: -18.0,
                max: 18.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "drive",
                name: "Drive",
                default_value: 12.0,
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

/// Time constant of every gain, drive, mix and high-pass switch glide, and
/// the length of a whole-state load's crossfade.
const SMOOTH_MS: f32 = 20.0;

/// A biquad that passes the signal untouched: the fallback when a design is
/// refused, so a band is always present and always keeps its state.
const IDENTITY: Coefficients<f32> = Coefficients {
    a1: 0.0,
    a2: 0.0,
    b0: 1.0,
    b1: 0.0,
    b2: 0.0,
};

/// Every tone stage's coefficients for one set of params.
#[derive(Debug, Clone, Copy)]
struct ToneCoefficients {
    hpf: Coefficients<f32>,
    low: Coefficients<f32>,
    mid: Coefficients<f32>,
    high: Coefficients<f32>,
}

impl ToneCoefficients {
    /// A band at 0 dB designs to (numerically) unity, so every band always
    /// runs: switching one in or out as it crossed ±0.01 dB stepped the
    /// signal.
    fn for_params(p: &Params, sample_rate: f32) -> Self {
        let design = |kind, hz, db, q| {
            make_eq_coefficients(kind, hz, db, q, sample_rate).unwrap_or(IDENTITY)
        };
        Self {
            hpf: design("highpass", p.highpass_hz, 0.0, 0.707),
            low: design("lowshelf", p.low_shelf_hz, p.low_shelf_db, 0.7),
            mid: design("bell", p.mid_hz, p.mid_db, p.mid_q),
            high: design("highshelf", p.high_shelf_hz, p.high_shelf_db, 0.7),
        }
    }
}

#[derive(Debug, Clone)]
struct ChannelFilters {
    hpf: DirectForm1<f32>,
    low: DirectForm1<f32>,
    mid: DirectForm1<f32>,
    high: DirectForm1<f32>,
}

impl ChannelFilters {
    fn new(c: &ToneCoefficients) -> Self {
        Self {
            hpf: DirectForm1::<f32>::new(c.hpf),
            low: DirectForm1::<f32>::new(c.low),
            mid: DirectForm1::<f32>::new(c.mid),
            high: DirectForm1::<f32>::new(c.high),
        }
    }

    /// New coefficients under running filters: their state carries on, so
    /// an edit bends the sound instead of restarting every filter mid-wave.
    fn retune(&mut self, c: &ToneCoefficients) {
        self.hpf.update_coefficients(c.hpf);
        self.low.update_coefficients(c.low);
        self.mid.update_coefficients(c.mid);
        self.high.update_coefficients(c.high);
    }

    fn reset(&mut self) {
        self.hpf.reset_state();
        self.low.reset_state();
        self.mid.reset_state();
        self.high.reset_state();
    }

    /// One sample through the chain; `hpf_mix` (0..1) crossfades the
    /// high-pass in, which always runs so switching it on finds it settled.
    #[inline]
    fn process(&mut self, sample: f32, hpf_mix: f32) -> f32 {
        let filtered = self.hpf.run(sample);
        let mut sample = sample + (filtered - sample) * hpf_mix;
        sample = self.low.run(sample);
        sample = self.mid.run(sample);
        self.high.run(sample)
    }
}

/// One complete C1073 sound: filters, glides and power. The DSP keeps two so
/// a whole-state load can crossfade from the old sound to the new one.
#[derive(Debug, Clone)]
struct Chain {
    power: bool,
    left: ChannelFilters,
    right: ChannelFilters,
    preamp_gain: Smoothed,
    output_gain: Smoothed,
    /// Drive amount, 0..1.
    drive: Smoothed,
    /// Wet share, 0..1.
    mix: Smoothed,
    /// High-pass share, 0..1, following `highpass_enabled`.
    hpf_mix: Smoothed,
}

impl Chain {
    fn new(params: &Params, sample_rate: f32) -> Self {
        let tone = ToneCoefficients::for_params(params, sample_rate);
        let mut chain = Self {
            power: params.power,
            left: ChannelFilters::new(&tone),
            right: ChannelFilters::new(&tone),
            preamp_gain: Smoothed::at(1.0),
            output_gain: Smoothed::at(1.0),
            drive: Smoothed::at(0.0),
            mix: Smoothed::at(1.0),
            hpf_mix: Smoothed::at(1.0),
        };
        chain.aim(params, sample_rate);
        chain.settle();
        chain
    }

    /// Retunes the filters in place and hands every glide its target.
    fn aim(&mut self, params: &Params, sample_rate: f32) {
        let tone = ToneCoefficients::for_params(params, sample_rate);
        self.left.retune(&tone);
        self.right.retune(&tone);
        self.power = params.power;
        self.preamp_gain.target = db_to_linear(params.preamp_db);
        self.output_gain.target = db_to_linear(params.output_db);
        self.drive.target = params.drive / 100.0;
        self.mix.target = params.mix / 100.0;
        self.hpf_mix.target = if params.highpass_enabled { 1.0 } else { 0.0 };
    }

    /// Every glide lands on its target, the high-pass crossfade included.
    fn settle(&mut self) {
        for smoothed in [
            &mut self.preamp_gain,
            &mut self.output_gain,
            &mut self.drive,
            &mut self.mix,
            &mut self.hpf_mix,
        ] {
            smoothed.settle();
        }
    }

    fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
        self.settle();
    }

    #[inline]
    fn process(&mut self, left: f32, right: f32, step: f32) -> (f32, f32) {
        if !self.power {
            return (left, right);
        }
        let preamp = glide(&mut self.preamp_gain, step);
        let output = glide(&mut self.output_gain, step);
        let drive = glide(&mut self.drive, step);
        let amount = glide(&mut self.mix, step);
        let hpf_mix = glide(&mut self.hpf_mix, step);
        let mut wet_l = Dsp::drive(left * preamp, drive);
        let mut wet_r = Dsp::drive(right * preamp, drive);
        wet_l = self.left.process(wet_l, hpf_mix) * output;
        wet_r = self.right.process(wet_r, hpf_mix) * output;

        (mix(left, wet_l, amount), mix(right, wet_r, amount))
    }
}

/// Live edits and state loads reach the sound differently:
///
/// - Before the first processed sample (after construction or a reset)
///   every change lands where it is going: the restored state is the one the
///   first sample hears, never a glide in from the defaults (the same rule
///   as Rodhareist's `fresh`).
/// - After that, [`Dsp::set_params`] is a live edit: filters retune in place
///   and every gain glides, so a knob drag neither clicks nor zippers.
/// - [`Dsp::load_params`] (or a [`Dsp::begin_state_load`] /
///   [`Dsp::end_state_load`] bracket around single edits) is a whole-state
///   load, such as a preset: the new state starts settled on a second chain
///   and the output crossfades to it linearly. Each output sample is then a
///   convex mix of two sounds, so it can never be louder than the louder of
///   them, where independent glides of preamp, output and drive could
///   overshoot both.
#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    /// The latest params asked for: what [`Dsp::params`] reports, including a
    /// load still waiting for the running crossfade to finish.
    params: Params,
    /// The sound — during a crossfade, the one fading in.
    chain: Chain,
    /// The sound a crossfade is leaving; idle otherwise.
    outgoing: Chain,
    smooth_step: f32,
    /// Crossfade position from `outgoing` (0) to `chain` (1); 1 when idle.
    fade: f32,
    fade_step: f32,
    /// Nothing has been processed since construction or the last reset, so
    /// any params arriving now are a state being applied and land at once.
    fresh: bool,
    /// Inside a [`Dsp::begin_state_load`] bracket: edits are only recorded.
    loading: bool,
    /// A whole-state load arrived mid-crossfade; it starts when this one
    /// ends, with the latest params.
    queued_load: bool,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let params = default_params();
        let chain = Chain::new(&params, sample_rate);
        Self {
            sample_rate,
            params,
            outgoing: chain.clone(),
            chain,
            smooth_step: smoothing_step(SMOOTH_MS, sample_rate),
            fade: 1.0,
            fade_step: fade_step(sample_rate),
            fresh: true,
            loading: false,
            queued_load: false,
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter as a live edit. Allocation-free and safe
    /// between blocks: the filters are retuned in place and every gain
    /// glides to its new value, so a knob drag (the mobile host sends each
    /// step here) neither clicks nor zippers. Before the first processed
    /// sample the values land instead (see [`Dsp`]).
    pub fn set_params(&mut self, params: Params) {
        self.params = sanitize(params);
        if self.loading || self.queued_load {
            // The load in progress picks these up when it applies.
            return;
        }
        if self.fresh {
            self.land();
        } else {
            self.chain.aim(&self.params, self.sample_rate);
        }
    }

    /// Replace every parameter as one whole-state load (preset, restored
    /// state): lands at once before the first processed sample, crossfades
    /// from the running sound after it. Allocation-free.
    pub fn load_params(&mut self, params: Params) {
        self.begin_state_load();
        self.set_params(params);
        self.end_state_load();
    }

    /// Opens a whole-state load for a host that applies a state one
    /// parameter at a time: [`Dsp::set_params`] calls until
    /// [`Dsp::end_state_load`] only record, and the end applies them as one
    /// load. Audio processed meanwhile keeps the previous sound.
    pub fn begin_state_load(&mut self) {
        self.loading = true;
    }

    /// Closes a [`Dsp::begin_state_load`] bracket and applies the result as
    /// one whole-state load.
    pub fn end_state_load(&mut self) {
        if !std::mem::take(&mut self.loading) {
            return;
        }
        if self.fresh {
            self.land();
        } else if self.fade < 1.0 {
            self.queued_load = true;
        } else {
            self.start_crossfade();
        }
    }

    /// The current params, with nothing gliding: what the first sample
    /// hears. The filter state is untouched — empty before any audio — and
    /// only the coefficients change.
    fn land(&mut self) {
        self.chain.aim(&self.params, self.sample_rate);
        self.chain.settle();
        self.fade = 1.0;
        self.queued_load = false;
    }

    /// Keeps the running sound as the outgoing one and starts the current
    /// params settled on the other chain, from empty filters as a restored
    /// instance would. Carrying the old filter state over is wrong here: it
    /// was built at the old preamp level, and a load that drops the preamp
    /// 24 dB rang out of it 1.9 dB above both settled sounds.
    fn start_crossfade(&mut self) {
        std::mem::swap(&mut self.outgoing, &mut self.chain);
        self.chain.aim(&self.params, self.sample_rate);
        self.chain.reset();
        self.fade = 0.0;
        self.queued_load = false;
    }

    #[inline]
    fn drive(sample: f32, amount: f32) -> f32 {
        if amount <= 0.0 {
            return sample;
        }
        // Mild asymmetric flavor for "iron" color without heavy aliasing.
        let drive = 1.0 + amount * 5.0;
        let shaped = (sample * drive).tanh();
        let even = shaped + 0.03 * amount * shaped * shaped;
        even / drive.tanh().max(0.001)
    }
}

fn sanitize(params: Params) -> Params {
    Params {
        power: params.power,
        preamp_db: clamp(params.preamp_db, -12.0, 36.0),
        highpass_hz: clamp(params.highpass_hz, 20.0, 300.0),
        highpass_enabled: params.highpass_enabled,
        low_shelf_db: clamp(params.low_shelf_db, -18.0, 18.0),
        low_shelf_hz: clamp(params.low_shelf_hz, 50.0, 400.0),
        mid_db: clamp(params.mid_db, -18.0, 18.0),
        mid_hz: nearest_mid_freq(params.mid_hz),
        mid_q: clamp(params.mid_q, 0.3, 4.0),
        high_shelf_db: clamp(params.high_shelf_db, -18.0, 18.0),
        high_shelf_hz: clamp(params.high_shelf_hz, 4_000.0, 16_000.0),
        output_db: clamp(params.output_db, -24.0, 12.0),
        drive: clamp(params.drive, 0.0, 100.0),
        mix: clamp(params.mix, 0.0, 100.0),
    }
}

/// Per-sample advance of a [`SMOOTH_MS`] linear crossfade.
fn fade_step(sample_rate: f32) -> f32 {
    1.0 / (SMOOTH_MS * 0.001 * sample_rate).max(1.0)
}

/// One sample of `smoothed`'s glide, landing exactly on the target once
/// within reach: a drive glide that only crept toward 0 would never reach
/// the shaper's exact bypass.
#[inline]
fn glide(smoothed: &mut Smoothed, step: f32) -> f32 {
    let value = smoothed.next(step);
    if (smoothed.target - value).abs() < 1.0e-5 {
        smoothed.settle();
    }
    smoothed.value
}

fn nearest_mid_freq(freq: f32) -> f32 {
    let mut best = MID_FREQ_CHOICES[0];
    let mut best_err = (freq - best).abs();
    for &candidate in &MID_FREQ_CHOICES[1..] {
        let err = (freq - candidate).abs();
        if err < best_err {
            best = candidate;
            best_err = err;
        }
    }
    best
}

impl StereoEffect for Dsp {
    /// Silences the filters and lands every glide; the next params before
    /// any audio land too, like a fresh instance's.
    fn reset(&mut self) {
        self.chain.reset();
        self.outgoing.reset();
        if self.queued_load {
            self.land();
        }
        self.fade = 1.0;
        self.fresh = true;
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.smooth_step = smoothing_step(SMOOTH_MS, self.sample_rate);
        self.fade_step = fade_step(self.sample_rate);
        // The outgoing chain is tuned for the old rate: a load in flight
        // lands on the new sound rather than crossfading from a mistuned one.
        let loading = self.fade < 1.0 || self.queued_load;
        self.fade = 1.0;
        self.queued_load = false;
        self.chain.aim(&self.params, self.sample_rate);
        if self.fresh || loading {
            self.chain.settle();
        }
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        let left = if left.is_finite() { left } else { 0.0 };
        let right = if right.is_finite() { right } else { 0.0 };
        let step = self.smooth_step;
        if self.fade >= 1.0 {
            // A chain that is off has heard nothing: the instance stays
            // fresh until audio actually runs through it.
            self.fresh &= !self.chain.power;
            return self.chain.process(left, right, step);
        }

        self.fresh = false;
        self.fade = (self.fade + self.fade_step).min(1.0);
        let weight = self.fade;
        let (old_l, old_r) = self.outgoing.process(left, right, step);
        let (new_l, new_r) = self.chain.process(left, right, step);
        if self.fade >= 1.0 && self.queued_load {
            self.start_crossfade();
        }
        (
            old_l + (new_l - old_l) * weight,
            old_r + (new_r - old_r) * weight,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_non_finite_sample_does_not_stick() {
        let mut dsp = Dsp::new(48_000.0);
        let before = signal_energy(&mut dsp);
        assert!(before > 1.0e-4, "fixture produced no audio ({before})");
        let _ = dsp.process_stereo(f32::NAN, 0.2);
        let _ = dsp.process_stereo(f32::NEG_INFINITY, f32::NAN);
        let after = signal_energy(&mut dsp);
        assert!(
            after > before * 0.25,
            "level did not recover: before {before} after {after}"
        );
    }

    fn signal_energy(dsp: &mut Dsp) -> f32 {
        let mut acc = 0.0f32;
        for n in 0..4_000 {
            let x = (n as f32 * 0.17).sin() * 0.25;
            let (l, r) = dsp.process_stereo(x, x * 0.8);
            assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
            acc += l * l + r * r;
        }
        acc
    }

    #[test]
    fn snaps_mid_frequency() {
        assert_eq!(nearest_mid_freq(1_500.0), 1_600.0);
        assert_eq!(nearest_mid_freq(7_000.0), 7_200.0);
    }

    #[test]
    fn eq_boost_changes_signal() {
        let mut dsp = Dsp::new(48_000.0);
        let mut params = default_params();
        params.mid_db = 12.0;
        params.mid_hz = 1_600.0;
        params.drive = 0.0;
        params.preamp_db = 0.0;
        params.mix = 100.0;
        dsp.set_params(params);

        // Drive a mid-frequency sine-ish impulse train and ensure finite output.
        let mut peak = 0.0f32;
        for n in 0..2_000 {
            let phase = (n as f32) * 1_600.0 * std::f32::consts::TAU / 48_000.0;
            let x = phase.sin() * 0.25;
            let (l, _) = dsp.process_stereo(x, x);
            peak = peak.max(l.abs());
        }
        assert!(peak.is_finite());
        assert!(peak > 0.0);
    }

    /// Dragging any knob on a running 60 Hz sine — the mobile host sends
    /// every step through `set_params` — bends the signal; it never resets a
    /// filter or steps the level. A step shows as a kink (second difference)
    /// far above the sine's own.
    #[test]
    fn dragging_a_knob_neither_clicks_nor_zippers() {
        type Edit = fn(&mut Params, f32);
        let edits: [(&str, Edit); 6] = [
            ("output", |p, t| p.output_db = -6.0 * t),
            ("preamp", |p, t| p.preamp_db = 6.0 * t),
            ("drive", |p, t| p.drive = 12.0 + 40.0 * t),
            ("low", |p, t| p.low_shelf_db = 3.0 * t),
            ("mid", |p, t| p.mid_db = -0.02 + 0.04 * t),
            ("hpf", |p, t| p.highpass_enabled = t < 0.5),
        ];
        for (name, edit) in edits {
            let mut dsp = Dsp::new(48_000.0);
            let base = dsp.params().clone();
            let mut history = [0.0f32; 2];
            let kink = |y: f32, history: &mut [f32; 2]| {
                let k = (y - 2.0 * history[1] + history[0]).abs();
                *history = [history[1], y];
                k
            };
            let sine = |n: usize| (n as f32 * std::f32::consts::TAU * 60.0 / 48_000.0).sin() * 0.5;
            let mut steady = 0.0f32;
            for n in 0..24_000 {
                let (l, _) = dsp.process_stereo(sine(n), sine(n));
                let k = kink(l, &mut history);
                if n > 12_000 {
                    steady = steady.max(k);
                }
            }
            // 64 knob steps, one per 64-sample block.
            let mut worst = 0.0f32;
            for n in 24_000..(24_000 + 64 * 64) {
                if n % 64 == 0 {
                    let mut params = base.clone();
                    edit(&mut params, (n - 24_000) as f32 / (64.0 * 64.0));
                    dsp.set_params(params);
                }
                let (l, _) = dsp.process_stereo(sine(n), sine(n));
                worst = worst.max(kink(l, &mut history));
            }
            assert!(
                worst < steady * 4.0,
                "{name}: the drag stepped the signal ({worst} against {steady})"
            );
        }
    }

    const SR: f32 = 48_000.0;
    /// 100 ms: the window a restored or loaded state must not burst in.
    const WINDOW: usize = 4_800;
    /// Allowed excess of a peak over the settled one: 1 dB. Gains land (or
    /// crossfade convexly), so what is left is the filters' own transient —
    /// the HPF and shelves starting on a signal from empty state, or a
    /// retune carrying the old state — which the 100 Hz part of the program
    /// overshoots by a fraction of a dB. The bug was +36 dB (+27 dB in the
    /// issue's measurement); a glide from the old values shows well above
    /// this.
    const MARGIN: f32 = 1.122;

    /// 100 Hz + 1 kHz: through the high-pass and both shelves' transitions,
    /// and periodic every 10 ms, so any 10 ms window holds its peak.
    fn program(n: usize) -> f32 {
        let t = n as f32 / SR;
        0.25 * (std::f32::consts::TAU * 100.0 * t).sin()
            + 0.25 * (std::f32::consts::TAU * 1_000.0 * t).sin()
    }

    /// Peak of both outputs over `range` of the program.
    fn run_peak(dsp: &mut Dsp, range: std::ops::Range<usize>) -> f32 {
        range
            .map(|n| {
                let (l, r) = dsp.process_stereo(program(n), program(n));
                l.abs().max(r.abs())
            })
            .fold(0.0, f32::max)
    }

    /// The peak `params` settle to: one second in, over the next half.
    fn settled_peak(params: &Params) -> f32 {
        let mut dsp = Dsp::new(SR);
        dsp.set_params(params.clone());
        run_peak(&mut dsp, 0..SR as usize);
        run_peak(&mut dsp, SR as usize..(SR * 1.5) as usize)
    }

    /// An extremely low-output setting: every gain at its bottom.
    fn quiet() -> Params {
        Params {
            preamp_db: -12.0,
            low_shelf_db: -18.0,
            mid_db: -18.0,
            high_shelf_db: -18.0,
            output_db: -24.0,
            drive: 0.0,
            ..default_params()
        }
    }

    /// Opposite extremes whose product is the same level: gliding preamp
    /// and output separately from one to the other overshoots both.
    fn hot_in_low_out() -> Params {
        Params {
            preamp_db: 24.0,
            output_db: -24.0,
            drive: 0.0,
            ..default_params()
        }
    }

    fn unity() -> Params {
        Params {
            drive: 0.0,
            ..default_params()
        }
    }

    /// The mobile host's restore: one `set_params` per key, each a copy of
    /// the current params with one field changed.
    fn replay_per_key(dsp: &mut Dsp, target: &Params) {
        type Copy = fn(&mut Params, &Params);
        let fields: [Copy; 14] = [
            |p, t| p.power = t.power,
            |p, t| p.preamp_db = t.preamp_db,
            |p, t| p.highpass_hz = t.highpass_hz,
            |p, t| p.highpass_enabled = t.highpass_enabled,
            |p, t| p.low_shelf_db = t.low_shelf_db,
            |p, t| p.low_shelf_hz = t.low_shelf_hz,
            |p, t| p.mid_db = t.mid_db,
            |p, t| p.mid_hz = t.mid_hz,
            |p, t| p.mid_q = t.mid_q,
            |p, t| p.high_shelf_db = t.high_shelf_db,
            |p, t| p.high_shelf_hz = t.high_shelf_hz,
            |p, t| p.output_db = t.output_db,
            |p, t| p.drive = t.drive,
            |p, t| p.mix = t.mix,
        ];
        for copy in fields {
            let mut p = dsp.params().clone();
            copy(&mut p, target);
            dsp.set_params(p);
        }
    }

    /// Project open, insert, undo: a new instance (or a reset one) handed
    /// its state before any audio starts on that state — per-key or whole.
    #[test]
    fn a_restored_quiet_state_starts_on_its_settings() {
        let settled = settled_peak(&quiet());
        type Restore = fn(&mut Dsp);
        let paths: [(&str, Restore); 4] = [
            ("per-key replay", |d| replay_per_key(d, &quiet())),
            ("whole-state load", |d| d.load_params(quiet())),
            ("bracketed per-key replay", |d| {
                d.begin_state_load();
                replay_per_key(d, &quiet());
                d.end_state_load();
            }),
            ("replay after a reset", |d| {
                run_peak(d, 0..WINDOW);
                d.reset();
                replay_per_key(d, &quiet());
            }),
        ];
        for (name, restore) in paths {
            let mut dsp = Dsp::new(SR);
            restore(&mut dsp);
            let start = if name == "replay after a reset" {
                WINDOW
            } else {
                0
            };
            let peak = run_peak(&mut dsp, start..start + WINDOW);
            assert!(
                peak <= settled * MARGIN,
                "{name}: the first 100 ms peaked at {peak}, settled is {settled} \
                 ({:+.1} dB)",
                20.0 * (peak / settled).log10()
            );
        }
    }

    /// A preset loaded into an instance already playing crossfades: never
    /// louder than the louder of the two settled sounds, and ends on the new
    /// one.
    #[test]
    fn a_preset_loaded_while_playing_never_bursts() {
        let pairs = [
            ("default -> quiet", default_params(), quiet()),
            ("quiet -> default", quiet(), default_params()),
            ("unity -> hot in, low out", unity(), hot_in_low_out()),
            ("hot in, low out -> unity", hot_in_low_out(), unity()),
        ];
        for (name, from, to) in pairs {
            let ceiling = settled_peak(&from).max(settled_peak(&to));
            let target = settled_peak(&to);
            for bracketed in [false, true] {
                let mut dsp = Dsp::new(SR);
                dsp.set_params(from.clone());
                run_peak(&mut dsp, 0..SR as usize / 2);
                if bracketed {
                    dsp.begin_state_load();
                    replay_per_key(&mut dsp, &to);
                    dsp.end_state_load();
                } else {
                    dsp.load_params(to.clone());
                }
                let start = SR as usize / 2;
                let peak = run_peak(&mut dsp, start..start + WINDOW);
                assert!(
                    peak <= ceiling * MARGIN,
                    "{name} (bracketed: {bracketed}): peaked at {peak} against \
                     {ceiling} ({:+.1} dB)",
                    20.0 * (peak / ceiling).log10()
                );
                let after = run_peak(&mut dsp, start + WINDOW..start + 2 * WINDOW);
                assert!(
                    (after / target - 1.0).abs() < 0.01,
                    "{name}: did not end on the new state ({after} against {target})"
                );
            }
        }
    }

    /// Paging through presets faster than a crossfade: each new load waits
    /// for the running crossfade, none overshoots, the last one wins.
    #[test]
    fn rapid_preset_loads_queue_without_bursting() {
        let states = [unity(), hot_in_low_out(), quiet(), default_params()];
        let ceiling = states.iter().map(settled_peak).fold(0.0, f32::max);
        let mut dsp = Dsp::new(SR);
        dsp.set_params(states[0].clone());
        let mut n = SR as usize / 2;
        run_peak(&mut dsp, 0..n);
        let mut peak = 0.0f32;
        for state in &states[1..] {
            dsp.load_params(state.clone());
            assert_eq!(dsp.params().output_db, state.output_db);
            peak = peak.max(run_peak(&mut dsp, n..n + 240));
            n += 240;
        }
        peak = peak.max(run_peak(&mut dsp, n..n + WINDOW));
        assert!(
            peak <= ceiling * MARGIN,
            "peaked at {peak} against {ceiling}"
        );
        let target = settled_peak(&default_params());
        let after = run_peak(&mut dsp, n + WINDOW..n + 2 * WINDOW);
        assert!(
            (after / target - 1.0).abs() < 0.01,
            "did not end on the last preset ({after} against {target})"
        );
    }

    /// Once audio runs, a restored instance's knobs are live again: an
    /// Output move glides (no snap, no zipper).
    #[test]
    fn a_live_change_after_a_restore_still_glides() {
        let restored = Params {
            output_db: -12.0,
            highpass_enabled: false,
            drive: 0.0,
            ..default_params()
        };
        // DC through flat shelves: the output is the gain itself.
        let mut dsp = Dsp::new(SR);
        replay_per_key(&mut dsp, &restored);
        let mut level = 0.0;
        for _ in 0..SR as usize / 2 {
            level = dsp.process_stereo(0.1, 0.1).0;
        }
        let quiet_level = level;
        assert!((quiet_level / (0.1 * db_to_linear(-12.0)) - 1.0).abs() < 0.01);
        dsp.set_params(Params {
            output_db: 0.0,
            ..restored.clone()
        });
        for _ in 0..48 {
            level = dsp.process_stereo(0.1, 0.1).0;
        }
        assert!(
            level < quiet_level + (0.1 - quiet_level) * 0.1,
            "1 ms after the edit the output already reached {level}: it snapped"
        );
        for _ in 0..SR as usize / 2 {
            level = dsp.process_stereo(0.1, 0.1).0;
        }
        assert!((level / 0.1 - 1.0).abs() < 0.01, "glide ended at {level}");

        // And on a 60 Hz sine, an Output drag after the restore stays as
        // smooth as the sine itself.
        let mut dsp = Dsp::new(SR);
        replay_per_key(&mut dsp, &restored);
        let sine = |n: usize| (n as f32 * std::f32::consts::TAU * 60.0 / SR).sin() * 0.5;
        let mut history = [0.0f32; 2];
        let mut kink = |y: f32| {
            let k = (y - 2.0 * history[1] + history[0]).abs();
            history = [history[1], y];
            k
        };
        let mut steady = 0.0f32;
        for n in 0..24_000 {
            let k = kink(dsp.process_stereo(sine(n), sine(n)).0);
            if n > 12_000 {
                steady = steady.max(k);
            }
        }
        let mut worst = 0.0f32;
        for n in 24_000..(24_000 + 64 * 64) {
            if n % 64 == 0 {
                let t = (n - 24_000) as f32 / (64.0 * 64.0);
                dsp.set_params(Params {
                    output_db: -12.0 + 12.0 * t,
                    ..restored.clone()
                });
            }
            worst = worst.max(kink(dsp.process_stereo(sine(n), sine(n)).0));
        }
        assert!(
            worst < steady * 4.0,
            "the drag stepped the signal ({worst} against {steady})"
        );
    }
}
