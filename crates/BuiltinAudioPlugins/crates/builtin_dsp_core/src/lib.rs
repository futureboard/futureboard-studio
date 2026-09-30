//! Shared descriptors, math, and dynamics helpers for BuiltinAudioPlugins.
//!
//! Keep this crate free of engine / host dependencies. Hot-path helpers must
//! stay allocation-free after construction.

use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type};

pub mod crossover;

/// Metadata for a builtin DSP core (host integration can map this later).
#[derive(Debug, Clone)]
pub struct PluginDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub vendor: &'static str,
    pub category: PluginCategory,
    pub version: &'static str,
    pub params: &'static [ParamDescriptor],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginCategory {
    Effect,
    Instrument,
    Analyzer,
    Utility,
}

#[derive(Debug, Clone, Copy)]
pub struct ParamDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub default_value: f32,
    pub min: f32,
    pub max: f32,
    pub unit: &'static str,
}

/// Realtime-safe stereo insert contract.
pub trait StereoEffect: Send {
    fn reset(&mut self);
    fn set_sample_rate(&mut self, sample_rate: f32);
    fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32);
}

/// Realtime-safe instrument contract (MIDI + render).
pub trait Instrument: Send {
    fn reset(&mut self);
    fn set_sample_rate(&mut self, sample_rate: f32);
    fn note_on(&mut self, note: u8, velocity: u8);
    fn note_off(&mut self, note: u8);
    fn process_stereo(&mut self) -> (f32, f32);
}

#[inline]
pub fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

#[inline]
pub fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.max(1.0e-12).log10()
}

#[inline]
pub fn mix(dry: f32, wet: f32, amount: f32) -> f32 {
    let a = clamp(amount, 0.0, 1.0);
    dry * (1.0 - a) + wet * a
}

/// One-pole coefficient for attack/release times in seconds.
#[inline]
pub fn time_constant(sample_rate: f32, seconds: f32) -> f32 {
    let samples = sample_rate.max(1.0) * seconds.max(1.0e-6);
    (-1.0 / samples).exp()
}

/// Below this magnitude a filter or envelope state is silence: 1e-25 is
/// −500 dBFS, far under anything a converter can represent.
pub const DENORMAL_FLOOR: f32 = 1.0e-25;

/// Snap a decaying recursive state to zero before it reaches the subnormal
/// range.
///
/// Every IIR state — envelope followers, one-poles, filter feedback taps —
/// decays geometrically once the input goes quiet. Left alone it crosses into
/// denormal floats, where x86 hands the operation to microcode at tens to
/// hundreds of cycles apiece, so a *silent* plugin can cost several times what
/// a playing one does. Apply this wherever a state is fed back into itself.
///
/// Compiles to a compare and a select, and nothing at −500 dBFS is audible.
#[inline]
pub fn flush_denormal(x: f32) -> f32 {
    if x.abs() < DENORMAL_FLOOR { 0.0 } else { x }
}

/// Soft-knee peak compressor (Giannoulis / Reiss style gain computer).
///
/// Envelope detection is allocation-free. Sidechain HPF uses `biquad`.
#[derive(Debug, Clone)]
pub struct SoftKneeCompressor {
    sample_rate: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup_linear: f32,
    envelope: f32,
    sidechain_hpf: Option<DirectForm1<f32>>,
    sc_cutoff_hz: f32,
}

impl SoftKneeCompressor {
    pub fn new(sample_rate: f32) -> Self {
        let mut dsp = Self {
            sample_rate: sample_rate.max(1.0),
            threshold_db: -18.0,
            ratio: 4.0,
            knee_db: 6.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            makeup_linear: 1.0,
            envelope: 0.0,
            sidechain_hpf: None,
            sc_cutoff_hz: 0.0,
        };
        dsp.set_timing(0.01, 0.1);
        dsp
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rebuild_sidechain();
    }

    pub fn set_timing(&mut self, attack_sec: f32, release_sec: f32) {
        self.attack_coeff = time_constant(self.sample_rate, attack_sec);
        self.release_coeff = time_constant(self.sample_rate, release_sec);
    }

    pub fn set_curve(&mut self, threshold_db: f32, ratio: f32, knee_db: f32, makeup_db: f32) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.knee_db = knee_db.max(0.0);
        self.makeup_linear = db_to_linear(makeup_db);
    }

    pub fn set_sidechain_hpf(&mut self, cutoff_hz: f32) {
        self.sc_cutoff_hz = cutoff_hz.max(0.0);
        self.rebuild_sidechain();
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
        if let Some(filter) = self.sidechain_hpf.as_mut() {
            filter.reset_state();
        }
    }

    pub fn gain_reduction_db(&self) -> f32 {
        let level_db = linear_to_db(self.envelope.max(1.0e-12));
        -self.compute_gr_db(level_db)
    }

    /// Returns wet sample after compression (makeup applied).
    #[inline]
    pub fn process_mono(&mut self, input: f32) -> f32 {
        let detected = match self.sidechain_hpf.as_mut() {
            Some(hpf) => hpf.run(input),
            None => input,
        };
        let level = detected.abs();
        let coeff = if level > self.envelope {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        // Flushed: the envelope decays toward zero on every release, and a
        // subnormal envelope makes a *silent* compressor cost several times
        // what a working one does.
        self.envelope = flush_denormal(coeff * self.envelope + (1.0 - coeff) * level);

        let level_db = linear_to_db(self.envelope.max(1.0e-12));
        let gr_db = self.compute_gr_db(level_db);
        input * db_to_linear(gr_db) * self.makeup_linear
    }

    #[inline]
    pub fn process_stereo_linked(&mut self, left: f32, right: f32) -> (f32, f32) {
        let detector = left.abs().max(right.abs());
        let detected = match self.sidechain_hpf.as_mut() {
            Some(hpf) => hpf.run(detector),
            None => detector,
        };
        let level = detected.abs();
        let coeff = if level > self.envelope {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        // Flushed: the envelope decays toward zero on every release, and a
        // subnormal envelope makes a *silent* compressor cost several times
        // what a working one does.
        self.envelope = flush_denormal(coeff * self.envelope + (1.0 - coeff) * level);

        let level_db = linear_to_db(self.envelope.max(1.0e-12));
        let gr_db = self.compute_gr_db(level_db);
        let gain = db_to_linear(gr_db) * self.makeup_linear;
        (left * gain, right * gain)
    }

    #[inline]
    fn compute_gr_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let half_knee = self.knee_db * 0.5;
        let compressed = if over <= -half_knee {
            level_db
        } else if over >= half_knee {
            self.threshold_db + over / self.ratio
        } else {
            let t = over + half_knee;
            level_db + (1.0 / self.ratio - 1.0) * (t * t) / (2.0 * self.knee_db.max(1.0e-6))
        };
        compressed - level_db
    }

    fn rebuild_sidechain(&mut self) {
        if self.sc_cutoff_hz < 10.0 {
            self.sidechain_hpf = None;
            return;
        }
        let fs = self.sample_rate.hz();
        let f0 = self
            .sc_cutoff_hz
            .min(max_filter_frequency(self.sample_rate))
            .hz();
        let Ok(coeffs) = Coefficients::<f32>::from_params(Type::HighPass, fs, f0, 0.707) else {
            self.sidechain_hpf = None;
            return;
        };
        self.sidechain_hpf = Some(DirectForm1::<f32>::new(coeffs));
    }
}

/// Fraction of the sample rate a filter's centre frequency may reach.
///
/// The bilinear transform degenerates as `f0` approaches Nyquist — `sin(w0)`
/// goes to zero and with it `alpha`, so the coefficients lose all resolution —
/// hence a guard band rather than `0.5` outright.
///
/// It was `0.45`, which is below 20 kHz at 44.1 kHz: the top of the audio band
/// was unreachable at the most common sample rate, so an EQ band dragged to
/// 20 kHz silently sat at 19,845 Hz instead. `0.49` clears 20 kHz at 44.1 kHz
/// with ~440 Hz still in hand, and stays comfortably stable — at the extreme
/// (`Q = 12`, ±18 dB) the poles sit at a radius near 0.998.
pub const MAX_FILTER_FREQUENCY_RATIO: f32 = 0.49;

/// Highest centre frequency a filter may be tuned to at `sample_rate`.
pub fn max_filter_frequency(sample_rate: f32) -> f32 {
    sample_rate.max(1.0) * MAX_FILTER_FREQUENCY_RATIO
}

/// Build biquad coefficients from common EQ band kinds.
///
/// Prefer this over [`make_eq_biquad`] for a filter that is already running:
/// coefficients can be handed to `Biquad::update_coefficients`, which retunes
/// in place, whereas installing a fresh `DirectForm1` also installs its zeroed
/// history and steps the signal.
///
/// The centre frequency is clamped to [`max_filter_frequency`].
pub fn make_eq_coefficients(
    kind: &str,
    freq_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate: f32,
) -> Option<Coefficients<f32>> {
    let fs = sample_rate.max(1.0);
    let f0 = clamp(freq_hz, 10.0, max_filter_frequency(fs));
    let q = clamp(q, 0.1, 12.0);
    let filter_type = match kind {
        "bell" | "peak" | "peaking" => Type::PeakingEQ(gain_db),
        "lowshelf" | "ls" => Type::LowShelf(gain_db),
        "highshelf" | "hs" => Type::HighShelf(gain_db),
        "lowpass" | "lp" => Type::LowPass,
        "highpass" | "hp" => Type::HighPass,
        "notch" => Type::Notch,
        // Not an EQ band shape — used to audition one band's frequency region
        // in isolation (see Equz8's band solo).
        "bandpass" | "bp" => Type::BandPass,
        _ => return None,
    };
    Coefficients::<f32>::from_params(filter_type, fs.hz(), f0.hz(), q).ok()
}

/// Build a `biquad` DirectForm1 from common EQ band kinds.
pub fn make_eq_biquad(
    kind: &str,
    freq_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate: f32,
) -> Option<DirectForm1<f32>> {
    make_eq_coefficients(kind, freq_hz, gain_db, q, sample_rate).map(DirectForm1::<f32>::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_reduces_hot_signal() {
        let mut comp = SoftKneeCompressor::new(48_000.0);
        comp.set_curve(-12.0, 4.0, 6.0, 0.0);
        // Very fast attack so steady-state GR is reached quickly.
        comp.set_timing(0.0001, 0.05);
        for _ in 0..2_000 {
            let _ = comp.process_mono(0.9);
        }
        let mut peak_out = 0.0f32;
        for _ in 0..256 {
            peak_out = peak_out.max(comp.process_mono(0.9).abs());
        }
        assert!(peak_out < 0.9 * 0.95);
        assert!(comp.gain_reduction_db() > 1.0);
    }

    #[test]
    fn eq_biquad_builds() {
        assert!(make_eq_biquad("bell", 1_000.0, 3.0, 1.0, 48_000.0).is_some());
        assert!(make_eq_biquad("highpass", 80.0, 0.0, 0.7, 48_000.0).is_some());
    }

    /// The top of the audio band has to be reachable at every supported rate.
    /// The old 0.45 ratio put the ceiling at 19,845 Hz on 44.1 kHz, so a band
    /// asked for 20 kHz quietly sat below it.
    #[test]
    fn twenty_kilohertz_is_reachable_at_every_common_rate() {
        for rate in [44_100.0, 48_000.0, 88_200.0, 96_000.0, 192_000.0] {
            assert!(
                max_filter_frequency(rate) >= 20_000.0,
                "{rate} Hz caps filters at {} Hz, below the audio band",
                max_filter_frequency(rate)
            );
        }
    }

    /// ...but never at or above Nyquist, where the bilinear transform collapses.
    #[test]
    fn the_ceiling_stays_below_nyquist() {
        for rate in [8_000.0, 44_100.0, 48_000.0, 192_000.0] {
            assert!(max_filter_frequency(rate) < rate * 0.5);
        }
    }

    /// A filter built right at the ceiling must stay stable and finite under
    /// the worst-case settings, not just be constructible.
    #[test]
    fn filters_at_the_ceiling_stay_stable() {
        for rate in [44_100.0f32, 48_000.0, 96_000.0] {
            let ceiling = max_filter_frequency(rate);
            for kind in [
                "bell",
                "lowshelf",
                "highshelf",
                "lowpass",
                "highpass",
                "notch",
            ] {
                for gain in [-18.0f32, 0.0, 18.0] {
                    let mut filter = make_eq_biquad(kind, ceiling, gain, 12.0, rate)
                        .unwrap_or_else(|| panic!("{kind} at {ceiling} Hz should build"));
                    // Drive it hard, then confirm it settles rather than running away.
                    let mut peak = 0.0f32;
                    for i in 0..4_000 {
                        let input = if i % 2 == 0 { 1.0 } else { -1.0 };
                        let out = filter.run(input);
                        assert!(
                            out.is_finite(),
                            "{kind} {gain} dB at {rate} Hz produced {out}"
                        );
                        if i > 2_000 {
                            peak = peak.max(out.abs());
                        }
                    }
                    assert!(
                        peak < 100.0,
                        "{kind} {gain} dB at {rate} Hz ran away to {peak}"
                    );
                }
            }
        }
    }
}
