//! WrapSynth's envelopes: analog-style ADSRs.
//!
//! Each stage is a one-pole approach toward a target slightly past where the
//! stage ends (the overshoot ratio), so the attack is a gentle convex curve
//! and the decay and release fall exponentially, the way a capacitor does —
//! instead of the straight ramps the first WrapSynth used, which sounded
//! mechanical and clicked on fast releases.
//!
//! A note-on starts the attack from wherever the envelope is, never from
//! zero, so retriggering a sounding note does not click.
//!
//! The coefficients depend only on the patch and the sample rate, so they are
//! worked out once per edit ([`EnvCoefficients`]) and shared by every voice;
//! a voice only keeps its [`Envelope`] state.

/// Overshoot of the attack target: a softly curved rise.
pub const ATTACK_RATIO: f32 = 0.3;
/// Overshoot of the decay and release targets: close to a true exponential.
pub const DECAY_RATIO: f32 = 1.0e-4;
/// Below this the release is over.
const SILENT: f32 = 1.0e-5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvTimes {
    pub attack_ms: f32,
    pub decay_ms: f32,
    pub sustain: f32,
    pub release_ms: f32,
}

/// One stage's step: `out = base + out · coef`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stage {
    coef: f32,
    base: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvCoefficients {
    attack: Stage,
    decay: Stage,
    release: Stage,
    sustain: f32,
}

/// The per-sample coefficient that covers a stage in `samples` samples with
/// the given overshoot `ratio` (Redmon, "Envelope generators", EarLevel 2013).
fn coefficient(samples: f32, ratio: f32) -> f32 {
    if samples <= 1.0 {
        return 0.0;
    }
    (-((1.0 + ratio) / ratio).ln() / samples).exp()
}

impl EnvCoefficients {
    pub fn new(times: EnvTimes, sample_rate: f32) -> Self {
        let samples = |ms: f32| ms.max(0.0) * 0.001 * sample_rate.max(1.0);
        let sustain = times.sustain.clamp(0.0, 1.0);
        let attack = coefficient(samples(times.attack_ms), ATTACK_RATIO);
        let decay = coefficient(samples(times.decay_ms), DECAY_RATIO);
        let release = coefficient(samples(times.release_ms), DECAY_RATIO);
        Self {
            attack: Stage {
                coef: attack,
                base: (1.0 + ATTACK_RATIO) * (1.0 - attack),
            },
            decay: Stage {
                coef: decay,
                base: (sustain - DECAY_RATIO) * (1.0 - decay),
            },
            release: Stage {
                coef: release,
                base: -DECAY_RATIO * (1.0 - release),
            },
            sustain,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Envelope {
    pub stage: EnvStage,
    pub value: f32,
}

impl Envelope {
    pub const IDLE: Self = Self {
        stage: EnvStage::Idle,
        value: 0.0,
    };

    /// Start (or restart) the attack from the current value.
    pub fn gate_on(&mut self) {
        self.stage = EnvStage::Attack;
    }

    pub fn gate_off(&mut self) {
        if self.stage != EnvStage::Idle {
            self.stage = EnvStage::Release;
        }
    }

    pub fn is_idle(&self) -> bool {
        self.stage == EnvStage::Idle
    }

    pub fn is_releasing(&self) -> bool {
        self.stage == EnvStage::Release
    }

    /// One sample further on. Allocation-free.
    #[inline]
    pub fn next(&mut self, c: &EnvCoefficients) -> f32 {
        match self.stage {
            EnvStage::Idle => {
                self.value = 0.0;
            }
            EnvStage::Attack => {
                self.value = c.attack.base + self.value * c.attack.coef;
                if self.value >= 1.0 {
                    self.value = 1.0;
                    self.stage = EnvStage::Decay;
                }
            }
            EnvStage::Decay => {
                self.value = c.decay.base + self.value * c.decay.coef;
                if self.value <= c.sustain {
                    self.value = c.sustain;
                    self.stage = EnvStage::Sustain;
                }
            }
            // Follows a sustain edit while held.
            EnvStage::Sustain => self.value = c.sustain,
            EnvStage::Release => {
                self.value = c.release.base + self.value * c.release.coef;
                if self.value <= SILENT {
                    self.value = 0.0;
                    self.stage = EnvStage::Idle;
                }
            }
        }
        self.value
    }
}

/// How far along a stage's curve the envelope is at `t` (0..1 of the stage's
/// time), from 0 to 1, for a stage with overshoot `ratio` — what the editor
/// draws between the corners.
pub fn segment_progress(t: f32, ratio: f32) -> f32 {
    let k = ((1.0 + ratio) / ratio).ln();
    let t = t.clamp(0.0, 1.0);
    ((1.0 - (-k * t).exp()) / (1.0 - (-k).exp())).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn times(attack_ms: f32, decay_ms: f32, sustain: f32, release_ms: f32) -> EnvCoefficients {
        EnvCoefficients::new(
            EnvTimes {
                attack_ms,
                decay_ms,
                sustain,
                release_ms,
            },
            RATE,
        )
    }

    #[test]
    fn the_stages_take_about_their_times() {
        let c = times(10.0, 100.0, 0.5, 200.0);
        let mut env = Envelope::IDLE;
        env.gate_on();
        let mut samples = 0;
        while env.stage == EnvStage::Attack {
            env.next(&c);
            samples += 1;
        }
        // 10 ms at 48 kHz.
        assert!((440..=520).contains(&samples), "attack took {samples}");
        while env.stage == EnvStage::Decay {
            env.next(&c);
        }
        assert_eq!(env.value, 0.5);
        env.gate_off();
        let mut samples = 0;
        while !env.is_idle() {
            env.next(&c);
            samples += 1;
        }
        // An exponential from 0.5 reaches silence a little before the full
        // 200 ms a fall from 1 takes.
        assert!((7_000..=9_700).contains(&samples), "release took {samples}");
    }

    #[test]
    fn a_retrigger_starts_from_where_it_is() {
        let c = times(20.0, 100.0, 0.7, 300.0);
        let mut env = Envelope::IDLE;
        env.gate_on();
        for _ in 0..2_000 {
            env.next(&c);
        }
        env.gate_off();
        for _ in 0..500 {
            env.next(&c);
        }
        let before = env.value;
        env.gate_on();
        let after = env.next(&c);
        assert!(
            after >= before && after - before < 0.05,
            "{before} → {after}"
        );
    }

    #[test]
    fn an_instant_stage_jumps_and_the_curve_spans_zero_to_one() {
        let c = times(0.0, 0.0, 0.3, 0.0);
        let mut env = Envelope::IDLE;
        env.gate_on();
        assert_eq!(env.next(&c), 1.0);
        for ratio in [ATTACK_RATIO, DECAY_RATIO] {
            assert_eq!(segment_progress(0.0, ratio), 0.0);
            assert!((segment_progress(1.0, ratio) - 1.0).abs() < 1.0e-6);
            assert!(segment_progress(0.5, ratio) > 0.5);
        }
    }
}
