//! Compressor stage — one topology, several voices.
//!
//! The detector and gain computer stay the shared [`SoftKneeCompressor`].
//! A voice only changes the knee, the sidechain corner, and how the knobs
//! scale. [`CompModel::Studio`] is the knee and sidechain the block used
//! before voices existed.

use builtin_dsp_core::SoftKneeCompressor;

use super::CompModel;

/// The studio voice: what every save without `comp_model` still runs.
const STUDIO_KNEE_DB: f32 = 6.0;
const STUDIO_SIDECHAIN_HZ: f32 = 80.0;

struct CompProfile {
    knee_db: f32,
    sidechain_hz: f32,
    thresh_offset_db: f32,
    ratio_scale: f32,
    attack_scale: f32,
    release_scale: f32,
}

impl CompProfile {
    fn for_model(model: CompModel) -> Self {
        match model {
            CompModel::Studio => Self {
                knee_db: STUDIO_KNEE_DB,
                sidechain_hz: STUDIO_SIDECHAIN_HZ,
                thresh_offset_db: 0.0,
                ratio_scale: 1.0,
                attack_scale: 1.0,
                release_scale: 1.0,
            },
            CompModel::Smash => Self {
                knee_db: 1.5,
                sidechain_hz: 140.0,
                thresh_offset_db: -6.0,
                ratio_scale: 1.8,
                attack_scale: 0.35,
                release_scale: 0.55,
            },
            CompModel::Brick => Self {
                knee_db: 0.4,
                sidechain_hz: 60.0,
                thresh_offset_db: -2.0,
                ratio_scale: 3.5,
                attack_scale: 0.2,
                release_scale: 0.4,
            },
            CompModel::PickAttack => Self {
                knee_db: 2.0,
                sidechain_hz: 280.0,
                thresh_offset_db: -4.0,
                ratio_scale: 2.2,
                attack_scale: 0.25,
                release_scale: 0.45,
            },
            CompModel::DeepPump => Self {
                knee_db: 3.0,
                sidechain_hz: 50.0,
                thresh_offset_db: -10.0,
                ratio_scale: 1.6,
                attack_scale: 0.8,
                release_scale: 2.8,
            },
            CompModel::MidPunch => Self {
                knee_db: 2.5,
                sidechain_hz: 400.0,
                thresh_offset_db: -8.0,
                ratio_scale: 2.4,
                attack_scale: 0.45,
                release_scale: 0.7,
            },
            CompModel::BassHold => Self {
                knee_db: 4.0,
                sidechain_hz: 28.0,
                thresh_offset_db: -4.0,
                ratio_scale: 1.7,
                attack_scale: 1.8,
                release_scale: 1.4,
            },
            CompModel::ParallelCrush => Self {
                knee_db: 12.0,
                sidechain_hz: 100.0,
                thresh_offset_db: -12.0,
                ratio_scale: 2.8,
                attack_scale: 0.5,
                release_scale: 0.9,
            },
            CompModel::OptoSlam => Self {
                knee_db: 8.0,
                sidechain_hz: 90.0,
                thresh_offset_db: -8.0,
                ratio_scale: 2.6,
                attack_scale: 3.2,
                release_scale: 1.6,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CompStage {
    comp: SoftKneeCompressor,
    attack_sec: f32,
    release_sec: f32,
}

impl CompStage {
    pub(super) fn new(sample_rate: f32) -> Self {
        let mut comp = SoftKneeCompressor::new(sample_rate.max(1.0));
        comp.set_sidechain_hpf(STUDIO_SIDECHAIN_HZ);
        Self {
            comp,
            attack_sec: 0.010,
            release_sec: 0.120,
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.comp.set_sample_rate(sample_rate.max(1.0));
        // The shared compressor does not re-derive envelope coefficients on a
        // rate change — re-apply the stored timing explicitly.
        self.comp.set_timing(self.attack_sec, self.release_sec);
        self.comp.set_sidechain_hpf(STUDIO_SIDECHAIN_HZ);
    }

    pub(super) fn reset(&mut self) {
        self.comp.reset();
    }

    /// Editor units: `thresh` −60..0 dB, `ratio` 1..20, `attack` 0.1..100 ms,
    /// `release` 10..1000 ms, `makeup` 0..24 dB.
    pub(super) fn configure(
        &mut self,
        model: CompModel,
        thresh_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
    ) {
        let voice = CompProfile::for_model(model);
        self.attack_sec = (attack_ms.clamp(0.1, 100.0) * voice.attack_scale).clamp(0.1, 100.0)
            * 0.001;
        self.release_sec =
            (release_ms.clamp(10.0, 1_000.0) * voice.release_scale).clamp(10.0, 1_000.0) * 0.001;
        self.comp.set_timing(self.attack_sec, self.release_sec);
        self.comp.set_sidechain_hpf(voice.sidechain_hz);
        self.comp.set_curve(
            (thresh_db + voice.thresh_offset_db).clamp(-60.0, 0.0),
            (ratio * voice.ratio_scale).clamp(1.0, 20.0),
            voice.knee_db,
            makeup_db.clamp(0.0, 24.0),
        );
    }

    /// Current gain reduction in positive dB, for a future GR meter.
    #[allow(dead_code)]
    pub(super) fn gain_reduction_db(&self) -> f32 {
        self.comp.gain_reduction_db()
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        self.comp.process_stereo_linked(left, right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_settings_are_near_transparent() {
        // Ratio 1:1 means no gain change regardless of threshold.
        let mut c = CompStage::new(48_000.0);
        c.configure(super::CompModel::Studio, -24.0, 1.0, 10.0, 120.0, 0.0);
        let mut max_err: f32 = 0.0;
        for n in 0..8_000 {
            let x = (n as f32 * 0.01).sin() * 0.5;
            let (l, _) = c.process(x, x);
            if n > 4_000 {
                max_err = max_err.max((l - x).abs());
            }
        }
        assert!(max_err < 1.0e-3, "1:1 ratio not transparent: {max_err}");
    }

    #[test]
    fn heavy_settings_reduce_gain_and_stay_finite() {
        let mut c = CompStage::new(48_000.0);
        c.configure(super::CompModel::Studio, -40.0, 20.0, 1.0, 50.0, 0.0);
        let mut peak: f32 = 0.0;
        for n in 0..24_000 {
            let x = (n as f32 * 0.02).sin() * 0.9;
            let (l, r) = c.process(x, x);
            assert!(l.is_finite() && r.is_finite());
            if n > 12_000 {
                peak = peak.max(l.abs());
            }
        }
        assert!(peak < 0.5, "20:1 at -40 dB should clamp 0.9 peaks: {peak}");
        assert!(c.gain_reduction_db() > 6.0);
    }

    #[test]
    fn studio_voice_is_the_previous_compressor() {
        let voice = CompProfile::for_model(super::CompModel::Studio);
        assert_eq!(voice.knee_db, 6.0);
        assert_eq!(voice.sidechain_hz, 80.0);
        assert_eq!(voice.thresh_offset_db, 0.0);
        assert_eq!(voice.ratio_scale, 1.0);
        assert_eq!(voice.attack_scale, 1.0);
        assert_eq!(voice.release_scale, 1.0);
    }
}
