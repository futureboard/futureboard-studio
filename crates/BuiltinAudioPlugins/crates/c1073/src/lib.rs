//! C1073 — 3-band channel EQ with preamp drive (Neve 1073-inspired).
//!
//! Phase 3 (harder). Tone stages use the MIT/Apache [`biquad`] crate:
//! high-pass, low shelf, mid bell (selectable center), high shelf, plus
//! mild class-A style tanh saturation.

use biquad::{Biquad, Coefficients, DirectForm1};
use builtin_dsp_core::delay::{Smoothed, smoothing_step};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    make_eq_coefficients, mix,
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

/// Time constant of every gain, drive, mix and high-pass switch glide.
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

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    left: ChannelFilters,
    right: ChannelFilters,
    smooth_step: f32,
    preamp_gain: Smoothed,
    output_gain: Smoothed,
    /// Drive amount, 0..1.
    drive: Smoothed,
    /// Wet share, 0..1.
    mix: Smoothed,
    /// High-pass share, 0..1, following `highpass_enabled`.
    hpf_mix: Smoothed,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let params = default_params();
        let tone = ToneCoefficients::for_params(&params, sample_rate);
        let mut dsp = Self {
            sample_rate,
            params,
            left: ChannelFilters::new(&tone),
            right: ChannelFilters::new(&tone),
            smooth_step: smoothing_step(SMOOTH_MS, sample_rate),
            preamp_gain: Smoothed::at(1.0),
            output_gain: Smoothed::at(1.0),
            drive: Smoothed::at(0.0),
            mix: Smoothed::at(1.0),
            hpf_mix: Smoothed::at(1.0),
        };
        dsp.rebuild();
        dsp.settle();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Replace every parameter. Allocation-free and safe between blocks:
    /// the filters are retuned in place and every gain glides to its new
    /// value, so a knob drag (the mobile host sends each step here) neither
    /// clicks nor zippers.
    pub fn set_params(&mut self, params: Params) {
        self.params = Params {
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
        };
        self.rebuild();
    }

    /// Retunes the filters in place and hands every glide its target.
    fn rebuild(&mut self) {
        let tone = ToneCoefficients::for_params(&self.params, self.sample_rate);
        self.left.retune(&tone);
        self.right.retune(&tone);
        self.smooth_step = smoothing_step(SMOOTH_MS, self.sample_rate);
        self.preamp_gain.target = db_to_linear(self.params.preamp_db);
        self.output_gain.target = db_to_linear(self.params.output_db);
        self.drive.target = self.params.drive / 100.0;
        self.mix.target = self.params.mix / 100.0;
        self.hpf_mix.target = if self.params.highpass_enabled {
            1.0
        } else {
            0.0
        };
    }

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
    fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
        self.settle();
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rebuild();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            return (left, right);
        }

        let step = self.smooth_step;
        let preamp = glide(&mut self.preamp_gain, step);
        let output = glide(&mut self.output_gain, step);
        let drive = glide(&mut self.drive, step);
        let amount = glide(&mut self.mix, step);
        let hpf_mix = glide(&mut self.hpf_mix, step);
        let mut wet_l = Self::drive(left * preamp, drive);
        let mut wet_r = Self::drive(right * preamp, drive);
        wet_l = self.left.process(wet_l, hpf_mix) * output;
        wet_r = self.right.process(wet_r, hpf_mix) * output;

        (mix(left, wet_l, amount), mix(right, wet_r, amount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
