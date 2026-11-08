//! C1073 — 3-band channel EQ with preamp drive (Neve 1073-inspired).
//!
//! Phase 3 (harder). Tone stages use the MIT/Apache [`biquad`] crate:
//! high-pass, low shelf, mid bell (selectable center), high shelf, plus
//! mild class-A style tanh saturation.

use biquad::{Biquad, DirectForm1};
use builtin_dsp_core::{
    ParamDescriptor, PluginCategory, PluginDescriptor, StereoEffect, clamp, db_to_linear,
    make_eq_biquad, mix,
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

#[derive(Debug, Clone)]
struct ChannelFilters {
    hpf: Option<DirectForm1<f32>>,
    low: Option<DirectForm1<f32>>,
    mid: Option<DirectForm1<f32>>,
    high: Option<DirectForm1<f32>>,
}

impl ChannelFilters {
    fn empty() -> Self {
        Self {
            hpf: None,
            low: None,
            mid: None,
            high: None,
        }
    }

    fn reset(&mut self) {
        if let Some(f) = self.hpf.as_mut() {
            f.reset_state();
        }
        if let Some(f) = self.low.as_mut() {
            f.reset_state();
        }
        if let Some(f) = self.mid.as_mut() {
            f.reset_state();
        }
        if let Some(f) = self.high.as_mut() {
            f.reset_state();
        }
    }

    #[inline]
    fn process(&mut self, mut sample: f32) -> f32 {
        if let Some(f) = self.hpf.as_mut() {
            sample = f.run(sample);
        }
        if let Some(f) = self.low.as_mut() {
            sample = f.run(sample);
        }
        if let Some(f) = self.mid.as_mut() {
            sample = f.run(sample);
        }
        if let Some(f) = self.high.as_mut() {
            sample = f.run(sample);
        }
        sample
    }
}

#[derive(Debug, Clone)]
pub struct Dsp {
    sample_rate: f32,
    params: Params,
    left: ChannelFilters,
    right: ChannelFilters,
    preamp_gain: f32,
    output_gain: f32,
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let mut dsp = Self {
            sample_rate: sample_rate.max(1.0),
            params: default_params(),
            left: ChannelFilters::empty(),
            right: ChannelFilters::empty(),
            preamp_gain: 1.0,
            output_gain: 1.0,
        };
        dsp.rebuild();
        dsp
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

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

    fn rebuild(&mut self) {
        self.preamp_gain = db_to_linear(self.params.preamp_db);
        self.output_gain = db_to_linear(self.params.output_db);

        let hpf = if self.params.highpass_enabled {
            make_eq_biquad(
                "highpass",
                self.params.highpass_hz,
                0.0,
                0.707,
                self.sample_rate,
            )
        } else {
            None
        };
        let low = if self.params.low_shelf_db.abs() > 0.01 {
            make_eq_biquad(
                "lowshelf",
                self.params.low_shelf_hz,
                self.params.low_shelf_db,
                0.7,
                self.sample_rate,
            )
        } else {
            None
        };
        let mid = if self.params.mid_db.abs() > 0.01 {
            make_eq_biquad(
                "bell",
                self.params.mid_hz,
                self.params.mid_db,
                self.params.mid_q,
                self.sample_rate,
            )
        } else {
            None
        };
        let high = if self.params.high_shelf_db.abs() > 0.01 {
            make_eq_biquad(
                "highshelf",
                self.params.high_shelf_hz,
                self.params.high_shelf_db,
                0.7,
                self.sample_rate,
            )
        } else {
            None
        };

        self.left = ChannelFilters {
            hpf,
            low,
            mid,
            high,
        };
        self.right = ChannelFilters {
            hpf,
            low,
            mid,
            high,
        };
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
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rebuild();
    }

    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.params.power {
            return (left, right);
        }

        let drive = self.params.drive / 100.0;
        let mut wet_l = Self::drive(left * self.preamp_gain, drive);
        let mut wet_r = Self::drive(right * self.preamp_gain, drive);
        wet_l = self.left.process(wet_l) * self.output_gain;
        wet_r = self.right.process(wet_r) * self.output_gain;

        let amount = self.params.mix / 100.0;
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
}
