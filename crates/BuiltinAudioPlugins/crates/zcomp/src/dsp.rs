//! Z-Comp gain-cell algorithm.
//!
//! One realtime-safe cell serves all four circuit models. Everything that
//! differs between them is a coefficient resolved on the control thread by
//! [`model_coeffs`]; the per-sample path below never branches on the model,
//! allocates, locks, formats, or calls into the host.
//!
//! # Signal flow (per sample, per channel)
//!
//! ```txt
//! in ──┬───────────────────────────────────────────────► gain cell ─► colour ─► out
//!      │                                                    ▲
//!      └─► detector tap ─► topology mix (FF/FB) ─► SC HPF ──┤
//!                                    │                      │
//!                                    ├─► thrust tilt        │
//!                                    ├─► peak / RMS blend   │
//!                                    ├─► static curve (dB)  │
//!                                    └─► ballistics ────────┘
//! ```
//!
//! Four design decisions carry most of the character:
//!
//! 1. **One smoothing stage.** The detector produces a level, the static curve
//!    turns it into a target reduction in dB, and exactly one attack/release
//!    stage follows that target. Smoothing the level *and* the reduction with
//!    the same constants — the previous shape here — makes the effective attack
//!    roughly twice the dialled value and rounds off every transient edge.
//! 2. **Program-dependent release.** Release interpolates between a fast and a
//!    slow time constant using a slow integrator of the reduction itself
//!    (`memory_db`). That is what the SSL auto-release network, the optical
//!    cell's LDR memory, and the Distressor's second stage all do physically.
//! 3. **Real feedback topology.** Feedback models detect the cell's own output
//!    (one sample old), not the input scaled by the previous reduction. The
//!    loop gain is what makes optical/FET units ease into their ratio.
//! 4. **Anti-aliased colour.** The harmonic stage is a soft clipper evaluated
//!    through its first antiderivative (ADAA), so drive adds harmonics instead
//!    of folding them back down the spectrum.

use builtin_dsp_core::{clamp, db_to_linear, flush_denormal, linear_to_db, mix, time_constant};

use crate::{CompModel, Params};

/// Reduction depth (dB) at which program-dependent release reaches its slow
/// end. Chosen so bus-glue amounts (2–4 dB) stay quick and limiting amounts
/// (10 dB+) breathe.
pub const PROGRAM_REFERENCE_DB: f32 = 10.0;

/// Time constant of the reduction integrator that drives program dependence.
const MEMORY_SECONDS: f32 = 0.320;

/// Fixed tilt frequency of the 2500-style "thrust" sidechain shelf.
const THRUST_HZ: f32 = 170.0;

/// Gain/mix smoothing so a moved knob cannot step the output.
const SMOOTH_SECONDS: f32 = 0.020;

/// Amplitude (≈ −10 dBFS) the colour stage is level-matched at, so Colour
/// changes the harmonic content and not the fader.
const COLOUR_MATCH_LEVEL: f32 = 0.32;

/// Below this sidechain cutoff the filter is bypassed instead of running at a
/// frequency that changes nothing but costs state.
const HPF_BYPASS_HZ: f32 = 20.5;

/// Resolved runtime coefficients for the selected circuit model.
///
/// Times are seconds, levels dB, blends 0..1. Everything here is computed off
/// the audio thread in [`model_coeffs`].
#[derive(Debug, Clone, Copy)]
pub struct ModelCoeffs {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    /// Overshoot (dB) at which the cell reaches half of its dialled ratio.
    /// `0` = fixed ratio, `> 0` = optical "over-easy" rise.
    pub over_easy_db: f32,
    pub attack_sec: f32,
    pub release_fast_sec: f32,
    pub release_slow_sec: f32,
    /// How far program dependence may push release toward the slow constant.
    pub program_depth: f32,
    /// Extra program dependence applied when Auto Release is engaged.
    pub program_auto_bias: f32,
    /// Detector averaging window; sets how much the RMS half of the blend
    /// smooths.
    pub rms_window_sec: f32,
    /// Detector blend, 0 = peak, 1 = RMS.
    pub rms_blend: f32,
    /// 0 = pure feed-forward, 1 = pure feedback.
    pub feedback: f32,
    /// Sidechain sensitivity multiplier.
    pub detector_boost: f32,
    /// Amount of the fixed high-pass tilt mixed into the sidechain.
    pub thrust: f32,
    /// Harmonic drive after the gain cell.
    pub drive: f32,
    /// 0 = symmetric (odd harmonics, FET/VCA), 1 = fully biased (even
    /// harmonics, Class-A).
    pub asymmetry: f32,
}

/// Map the user-facing parameters onto one model's circuit constants.
///
/// Control thread only: this is the single place a model's character lives.
pub fn model_coeffs(params: &Params) -> ModelCoeffs {
    let color = clamp(params.color, 0.0, 100.0) / 100.0;
    let ratio = params.ratio.max(1.0);
    let knee = params.knee_db.max(0.0);
    let attack_ms = params.attack_ms.max(0.01);
    let release_ms = params.release_ms.max(10.0);
    let auto = params.auto_release;

    match params.model {
        // API 2500 style: feed-forward VCA, wide soft knee, thrust-shaped
        // sidechain, low-order THD from the output amp.
        CompModel::Comp2500 => ModelCoeffs {
            threshold_db: params.threshold_db,
            ratio,
            knee_db: (knee + 3.0 + color * 3.0).min(24.0),
            over_easy_db: 0.0,
            attack_sec: (attack_ms * 0.001).clamp(0.0002, 0.080),
            release_fast_sec: (release_ms * 0.001).clamp(0.020, 1.200),
            release_slow_sec: (release_ms * 0.001 * 2.6).clamp(0.060, 2.400),
            program_depth: 0.45,
            program_auto_bias: if auto { 0.25 } else { 0.0 },
            rms_window_sec: 0.010,
            rms_blend: 0.30,
            feedback: 0.0,
            detector_boost: 1.0,
            // Thrust is the 2500's own sidechain tilt, so it tracks Colour.
            thrust: color * 0.85,
            drive: color * 0.45,
            asymmetry: 0.15,
        },

        // Distressor style: FET cell in a feedback loop, hard ratios, fast
        // detector, British-mode grit as Colour rises.
        CompModel::Distressor => {
            let british = color;
            ModelCoeffs {
                threshold_db: params.threshold_db - british * 1.5,
                ratio: (ratio * (1.0 + british * 0.5)).min(40.0),
                knee_db: (knee * (1.0 - british * 0.6)).max(0.3),
                over_easy_db: 0.0,
                attack_sec: (attack_ms * 0.001 * (1.0 - british * 0.35)).clamp(0.00005, 0.050),
                release_fast_sec: (release_ms * 0.001).clamp(0.010, 1.500),
                release_slow_sec: (release_ms * 0.001 * (3.0 + british * 2.0)).clamp(0.040, 3.000),
                program_depth: 0.55,
                program_auto_bias: if auto { 0.30 } else { 0.0 },
                rms_window_sec: 0.003,
                rms_blend: 0.08,
                feedback: 0.35 + british * 0.20,
                detector_boost: 1.15 + british * 0.85,
                thrust: british * 0.35,
                drive: 0.20 + british * 0.95,
                asymmetry: 0.05,
            }
        }

        // Avalon style: Class-A optical leveller. The LDR eases into its ratio
        // and remembers how hard it has been working, so release is the slowest
        // and the most program dependent of the four.
        CompModel::Avalon => ModelCoeffs {
            threshold_db: params.threshold_db,
            ratio: (2.0 + (ratio - 1.0) * 0.55).clamp(1.5, 8.0),
            knee_db: (knee + 6.0 + color * 4.0).min(24.0),
            // Optical cells only reach their nominal ratio well past the knee.
            over_easy_db: 8.0,
            attack_sec: (attack_ms * 0.001).clamp(0.008, 0.080),
            release_fast_sec: (release_ms * 0.001).clamp(0.060, 1.800),
            release_slow_sec: (release_ms * 0.001 * 4.0 + 0.8 + color * 1.2).clamp(0.400, 5.000),
            program_depth: 0.85,
            program_auto_bias: if auto { 0.35 } else { 0.0 },
            rms_window_sec: 0.025,
            rms_blend: 0.60,
            feedback: 0.85,
            detector_boost: 1.0,
            thrust: 0.0,
            drive: color * 0.30,
            // Class-A stage is single-ended: even harmonics dominate.
            asymmetry: 0.85,
        },

        // SSL bus style: gentle VCA glue with the dual time-constant automatic
        // release network.
        CompModel::Ssl => ModelCoeffs {
            threshold_db: params.threshold_db,
            ratio: ratio.clamp(1.5, 10.0),
            knee_db: (knee + 3.0).clamp(2.0, 18.0),
            over_easy_db: 0.0,
            attack_sec: (attack_ms * 0.001).clamp(0.0001, 0.030),
            release_fast_sec: if auto {
                0.100
            } else {
                (release_ms * 0.001).clamp(0.050, 1.200)
            },
            release_slow_sec: if auto {
                1.200 + color * 0.400
            } else {
                (release_ms * 0.001 * 2.4).clamp(0.100, 2.400)
            },
            program_depth: 0.60,
            program_auto_bias: if auto { 0.40 } else { 0.0 },
            rms_window_sec: 0.015,
            rms_blend: 0.40 + color * 0.15,
            feedback: 0.10,
            detector_boost: 1.0,
            thrust: color * 0.25,
            drive: color * 0.30,
            asymmetry: 0.25,
        },
    }
}

/// Shared coefficients for a 12 dB/oct topology-preserving state-variable
/// high-pass. TPT form stays stable when the cutoff is changed while running.
#[derive(Debug, Clone, Copy)]
struct HpfCoeffs {
    active: bool,
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
}

impl HpfCoeffs {
    fn bypass() -> Self {
        Self {
            active: false,
            k: 0.0,
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
        }
    }

    fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        if cutoff_hz <= HPF_BYPASS_HZ {
            return Self::bypass();
        }
        let nyquist_limit = sample_rate * 0.45;
        let fc = clamp(
            cutoff_hz,
            HPF_BYPASS_HZ,
            nyquist_limit.max(HPF_BYPASS_HZ + 1.0),
        );
        let g = (std::f32::consts::PI * fc / sample_rate).tan();
        // Butterworth Q keeps the corner flat — a sidechain filter that rings
        // would pump on its own resonance.
        let k = std::f32::consts::SQRT_2;
        let a1 = 1.0 / (1.0 + g * (g + k));
        Self {
            active: true,
            k,
            a1,
            a2: g * a1,
            a3: g * (g * a1),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct HpfState {
    ic1: f32,
    ic2: f32,
}

impl HpfState {
    #[inline]
    fn process(&mut self, coeffs: &HpfCoeffs, input: f32) -> f32 {
        if !coeffs.active {
            return input;
        }
        let v3 = input - self.ic2;
        let v1 = coeffs.a1 * self.ic1 + coeffs.a2 * v3;
        let v2 = self.ic2 + coeffs.a2 * self.ic1 + coeffs.a3 * v3;
        self.ic1 = flush_denormal(2.0 * v1 - self.ic1);
        self.ic2 = flush_denormal(2.0 * v2 - self.ic2);
        input - coeffs.k * v1 - v2
    }

    fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }
}

/// First-order high-pass used for the thrust tilt, kept separate from the
/// user's sidechain filter so the two are independently auditable.
#[derive(Debug, Clone, Copy, Default)]
struct TiltState {
    x1: f32,
    y1: f32,
}

impl TiltState {
    #[inline]
    fn process(&mut self, coeff: f32, input: f32) -> f32 {
        let y = coeff * (self.y1 + input - self.x1);
        self.x1 = input;
        self.y1 = flush_denormal(y);
        self.y1
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }
}

/// Cubic soft clipper, the shape every analogue output stage approaches.
#[inline]
fn clip_shape(x: f32) -> f32 {
    if x >= 1.0 {
        2.0 / 3.0
    } else if x <= -1.0 {
        -2.0 / 3.0
    } else {
        x - (x * x * x) / 3.0
    }
}

/// Antiderivative of [`clip_shape`], used for first-order ADAA.
#[inline]
fn clip_antiderivative(x: f32) -> f32 {
    let a = x.abs();
    if a >= 1.0 {
        5.0 / 12.0 + (2.0 / 3.0) * (a - 1.0)
    } else {
        let x2 = x * x;
        0.5 * x2 - (x2 * x2) / 12.0
    }
}

/// Anti-aliased saturator state (one per channel).
///
/// Naive waveshaping folds every harmonic above Nyquist back into the band.
/// Evaluating the shaper as the average of its antiderivative over the sample
/// interval suppresses that fold-back by roughly 10–20 dB for the cost of one
/// extra evaluation and two stored floats.
#[derive(Debug, Clone, Copy, Default)]
struct SaturatorState {
    last_x: f32,
    last_anti: f32,
}

impl SaturatorState {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let anti = clip_antiderivative(x);
        let dx = x - self.last_x;
        let y = if dx.abs() > 1.0e-4 {
            (anti - self.last_anti) / dx
        } else {
            clip_shape(0.5 * (x + self.last_x))
        };
        self.last_x = x;
        self.last_anti = anti;
        y
    }

    fn reset(&mut self) {
        self.last_x = 0.0;
        self.last_anti = 0.0;
    }
}

/// What the cell produced for one stereo frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct CellFrame {
    pub left: f32,
    pub right: f32,
    /// Post-filter sidechain, so the editor's Sidechain Listen switch auditions
    /// exactly what the detector hears.
    pub sidechain_left: f32,
    pub sidechain_right: f32,
}

/// Stereo gain cell shared by every model.
#[derive(Debug, Clone)]
pub struct GainCell {
    sample_rate: f32,

    // Static curve.
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    over_easy_db: f32,

    // Ballistics.
    attack_coeff: f32,
    release_fast_coeff: f32,
    release_slow_coeff: f32,
    program_depth: f32,
    program_base: f32,
    memory_coeff: f32,

    // Detector.
    rms_coeff: f32,
    rms_blend: f32,
    feedback: f32,
    detector_boost: f32,
    thrust: f32,
    thrust_coeff: f32,
    hpf: HpfCoeffs,
    stereo_link: f32,

    // Colour.
    drive_gain: f32,
    drive_bias: f32,
    drive_bias_offset: f32,
    drive_norm: f32,
    drive_mix: f32,

    // Per-channel state.
    hpf_state: [HpfState; 2],
    tilt_state: [TiltState; 2],
    saturator: [SaturatorState; 2],
    mean_square: [f32; 2],
    gr_db: [f32; 2],
    memory_db: [f32; 2],
    feedback_sample: [f32; 2],
}

impl GainCell {
    pub fn new(sample_rate: f32, coeffs: ModelCoeffs, sidechain_hz: f32, stereo_link: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut cell = Self {
            sample_rate: sr,
            threshold_db: -18.0,
            ratio: 4.0,
            knee_db: 6.0,
            over_easy_db: 0.0,
            attack_coeff: 0.0,
            release_fast_coeff: 0.0,
            release_slow_coeff: 0.0,
            program_depth: 0.0,
            program_base: 0.0,
            memory_coeff: time_constant(sr, MEMORY_SECONDS),
            rms_coeff: time_constant(sr, 0.010),
            rms_blend: 0.0,
            feedback: 0.0,
            detector_boost: 1.0,
            thrust: 0.0,
            thrust_coeff: (-2.0 * std::f32::consts::PI * THRUST_HZ / sr).exp(),
            hpf: HpfCoeffs::bypass(),
            stereo_link: 1.0,
            drive_gain: 1.0,
            drive_bias: 0.0,
            drive_bias_offset: 0.0,
            drive_norm: 1.0,
            drive_mix: 0.0,
            hpf_state: [HpfState::default(); 2],
            tilt_state: [TiltState::default(); 2],
            saturator: [SaturatorState::default(); 2],
            mean_square: [0.0; 2],
            gr_db: [0.0; 2],
            memory_db: [0.0; 2],
            feedback_sample: [0.0; 2],
        };
        cell.set_model(coeffs, sidechain_hz, stereo_link);
        cell
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.memory_coeff = time_constant(self.sample_rate, MEMORY_SECONDS);
        self.thrust_coeff = (-2.0 * std::f32::consts::PI * THRUST_HZ / self.sample_rate).exp();
    }

    /// Resolve one model's constants into per-sample coefficients.
    ///
    /// Control thread only. Every `exp`, `tan`, and division in the algorithm
    /// happens here so the audio path stays arithmetic.
    pub fn set_model(&mut self, coeffs: ModelCoeffs, sidechain_hz: f32, stereo_link: f32) {
        let sr = self.sample_rate;

        self.threshold_db = clamp(coeffs.threshold_db, -80.0, 12.0);
        self.ratio = coeffs.ratio.max(1.0);
        self.knee_db = coeffs.knee_db.max(0.0);
        self.over_easy_db = coeffs.over_easy_db.max(0.0);

        self.attack_coeff = time_constant(sr, coeffs.attack_sec);
        let fast = coeffs.release_fast_sec.max(0.001);
        let slow = coeffs.release_slow_sec.max(fast);
        self.release_fast_coeff = time_constant(sr, fast);
        self.release_slow_coeff = time_constant(sr, slow);
        self.program_depth = clamp(coeffs.program_depth, 0.0, 1.0);
        self.program_base = clamp(coeffs.program_auto_bias, 0.0, 1.0);

        self.rms_coeff = time_constant(sr, coeffs.rms_window_sec.max(0.0005));
        self.rms_blend = clamp(coeffs.rms_blend, 0.0, 1.0);
        self.feedback = clamp(coeffs.feedback, 0.0, 0.95);
        self.detector_boost = coeffs.detector_boost.max(0.1);
        self.thrust = clamp(coeffs.thrust, 0.0, 1.0);
        self.hpf = HpfCoeffs::new(sr, sidechain_hz);
        self.stereo_link = clamp(stereo_link, 0.0, 100.0) / 100.0;

        let drive = coeffs.drive.max(0.0);
        self.drive_gain = 1.0 + drive * 2.0;
        self.drive_bias = clamp(coeffs.asymmetry, 0.0, 1.0) * drive * 0.35;
        self.drive_bias_offset = clip_shape(self.drive_bias);
        // Level-match the colour stage at a nominal operating amplitude rather
        // than at zero. Normalising on the small-signal slope would leave a
        // heavily driven cell several dB quieter, which turns Colour into a
        // volume control; matching at −10 dBFS keeps the knob about harmonics.
        let shaped = clip_shape(COLOUR_MATCH_LEVEL * self.drive_gain + self.drive_bias)
            - self.drive_bias_offset;
        self.drive_norm = if shaped.abs() > 1.0e-6 {
            COLOUR_MATCH_LEVEL / shaped
        } else {
            1.0
        };
        self.drive_mix = clamp(drive, 0.0, 0.85);
    }

    pub fn reset(&mut self) {
        for channel in 0..2 {
            self.hpf_state[channel].reset();
            self.tilt_state[channel].reset();
            self.saturator[channel].reset();
            self.mean_square[channel] = 0.0;
            self.gr_db[channel] = 0.0;
            self.memory_db[channel] = 0.0;
            self.feedback_sample[channel] = 0.0;
        }
    }

    /// Reduction currently applied, in positive dB — the louder channel, which
    /// is what a hardware GR meter shows.
    #[inline]
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db[0].max(self.gr_db[1])
    }

    /// Static gain-computer curve: level in dB → reduction in dB.
    ///
    /// Quadratic soft knee (Giannoulis/Reiss) with an optional optical
    /// "over-easy" term that only reaches the dialled ratio well past the knee.
    #[inline]
    fn target_gr_db(&self, level_db: f32) -> f32 {
        curve_reduction_db(
            level_db,
            self.threshold_db,
            self.ratio,
            self.knee_db,
            self.over_easy_db,
        )
    }

    /// Detector level for one channel, returning `(detector, sidechain)`.
    #[inline]
    fn detect(&mut self, channel: usize, input: f32) -> (f32, f32) {
        let sense = mix(input, self.feedback_sample[channel], self.feedback);
        let mut sidechain = self.hpf_state[channel].process(&self.hpf, sense);
        if self.thrust > 0.0 {
            // Thrust tilts the sidechain away from low-frequency energy so a
            // kick stops ducking the whole mix.
            let tilted = self.tilt_state[channel].process(self.thrust_coeff, sidechain);
            sidechain = mix(sidechain, tilted, self.thrust);
        }
        let driven = sidechain * self.detector_boost;

        let square = driven * driven;
        self.mean_square[channel] = flush_denormal(
            self.rms_coeff * self.mean_square[channel] + (1.0 - self.rms_coeff) * square,
        );

        let peak = driven.abs();
        let rms = self.mean_square[channel].max(0.0).sqrt();
        (mix(peak, rms, self.rms_blend), sidechain)
    }

    /// Advance one channel's reduction toward its target.
    ///
    /// Attack is a fixed constant. Release interpolates between the fast and
    /// slow constants by how much reduction the cell has been holding — the
    /// program dependence that makes these circuits sound different from a
    /// textbook compressor.
    #[inline]
    fn follow(&mut self, channel: usize, detector: f32) -> f32 {
        let target = self.target_gr_db(linear_to_db(detector.max(1.0e-9)));
        let current = self.gr_db[channel];

        let coeff = if target > current {
            self.attack_coeff
        } else {
            let depth = clamp(self.memory_db[channel] / PROGRAM_REFERENCE_DB, 0.0, 1.0);
            let blend = clamp(self.program_base + self.program_depth * depth, 0.0, 1.0);
            self.release_fast_coeff + (self.release_slow_coeff - self.release_fast_coeff) * blend
        };

        let next = flush_denormal(coeff * current + (1.0 - coeff) * target);
        self.gr_db[channel] = next;
        self.memory_db[channel] = flush_denormal(
            self.memory_coeff * self.memory_db[channel] + (1.0 - self.memory_coeff) * next,
        );
        next
    }

    /// Harmonic colour stage: anti-aliased soft clip, blended back by drive.
    #[inline]
    fn colour(&mut self, channel: usize, sample: f32) -> f32 {
        if self.drive_mix <= 0.0 {
            return sample;
        }
        let shaped = self.saturator[channel].process(sample * self.drive_gain + self.drive_bias);
        let wet = (shaped - self.drive_bias_offset) * self.drive_norm;
        mix(sample, wet, self.drive_mix)
    }

    /// One stereo frame through the whole cell.
    #[inline]
    pub fn process_stereo(&mut self, left: f32, right: f32) -> CellFrame {
        let (det_l, sc_l) = self.detect(0, left);
        let (det_r, sc_r) = self.detect(1, right);

        // Stereo link is a detector-side sum, exactly as the hardware ties the
        // two control voltages together — not a post-hoc average of two
        // independently computed reductions.
        let link = self.stereo_link;
        let linked = det_l.max(det_r);
        let use_l = mix(det_l, linked, link);
        let use_r = mix(det_r, linked, link);

        let gr_l = self.follow(0, use_l);
        let gr_r = self.follow(1, use_r);

        let out_l = self.colour(0, left * db_to_linear(-gr_l));
        let out_r = self.colour(1, right * db_to_linear(-gr_r));

        // Feedback models detect this sample on the next one.
        self.feedback_sample[0] = flush_denormal(out_l);
        self.feedback_sample[1] = flush_denormal(out_r);

        CellFrame {
            left: out_l,
            right: out_r,
            sidechain_left: sc_l,
            sidechain_right: sc_r,
        }
    }
}

/// The gain cell's static curve: decibels taken off a detector level of
/// `level_db`. Quadratic soft knee (Giannoulis/Reiss) with an optional
/// optical "over-easy" term that only reaches the dialled ratio well past the
/// knee. Positive.
#[inline]
pub fn curve_reduction_db(
    level_db: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    over_easy_db: f32,
) -> f32 {
    let over = level_db - threshold_db;
    let half_knee = knee_db * 0.5;
    let curved = if over <= -half_knee {
        return 0.0;
    } else if over >= half_knee || knee_db <= 1.0e-4 {
        over.max(0.0)
    } else {
        let t = over + half_knee;
        t * t / (2.0 * knee_db)
    };

    let ratio = if over_easy_db > 0.0 {
        // Saturating rise: half the dialled ratio at `over_easy_db` of
        // overshoot, asymptotically all of it. No transcendental needed.
        let t = curved / (curved + over_easy_db);
        1.0 + (ratio - 1.0) * t
    } else {
        ratio
    };

    curved * (1.0 - 1.0 / ratio)
}

/// One-pole parameter smoother: keeps gain and mix moves click-free.
#[derive(Debug, Clone, Copy)]
pub struct Smoothed {
    value: f32,
    target: f32,
    coeff: f32,
}

impl Smoothed {
    pub fn new(sample_rate: f32, value: f32) -> Self {
        Self {
            value,
            target: value,
            coeff: time_constant(sample_rate.max(1.0), SMOOTH_SECONDS),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.coeff = time_constant(sample_rate.max(1.0), SMOOTH_SECONDS);
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump to the target: used on reset, never mid-stream.
    pub fn snap(&mut self, target: f32) {
        self.target = target;
        self.value = target;
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        self.value = flush_denormal(self.coeff * self.value + (1.0 - self.coeff) * self.target);
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_params;

    const SR: f32 = 48_000.0;

    fn cell_for(params: &Params) -> GainCell {
        GainCell::new(
            SR,
            model_coeffs(params),
            params.sidechain_hpf_hz,
            params.stereo_link,
        )
    }

    /// A model with no over-easy term, no feedback, and no colour, so the
    /// static curve can be checked against the textbook formula.
    fn plain_params() -> Params {
        let mut params = default_params();
        params.model = CompModel::Comp2500;
        params.color = 0.0;
        params.knee_db = 0.0;
        params.ratio = 4.0;
        params.threshold_db = -30.0;
        params
    }

    #[test]
    fn static_curve_matches_the_dialled_ratio_above_the_knee() {
        let params = plain_params();
        let cell = cell_for(&params);
        let knee = cell.knee_db;

        // Well below threshold the cell is transparent.
        assert_eq!(cell.target_gr_db(params.threshold_db - knee), 0.0);

        for over in [6.0_f32, 12.0, 24.0] {
            let expected = over * (1.0 - 1.0 / params.ratio);
            let actual = cell.target_gr_db(params.threshold_db + over);
            assert!(
                (actual - expected).abs() < 1.0e-3,
                "{over} dB over: expected {expected} dB, got {actual} dB"
            );
        }
    }

    #[test]
    fn soft_knee_is_continuous_and_monotonic() {
        let mut params = plain_params();
        params.knee_db = 12.0;
        let cell = cell_for(&params);

        let mut previous = 0.0_f32;
        let mut level = params.threshold_db - cell.knee_db;
        while level <= params.threshold_db + 24.0 {
            let gr = cell.target_gr_db(level);
            assert!(gr >= previous - 1.0e-4, "curve dipped at {level} dB");
            assert!(gr - previous < 1.0, "curve stepped at {level} dB");
            previous = gr;
            level += 0.25;
        }
        assert!(previous > 10.0, "24 dB over should still reduce hard");
    }

    #[test]
    fn optical_over_easy_eases_into_its_ratio() {
        let mut params = default_params();
        params.model = CompModel::Avalon;
        params.ratio = 8.0;
        params.knee_db = 0.0;
        let mut cell = cell_for(&params);

        let easy_near = cell.target_gr_db(cell.threshold_db + 2.0);
        let easy_far = cell.target_gr_db(cell.threshold_db + 40.0);

        // Same curve with the optical term removed: a fixed-ratio reference.
        cell.over_easy_db = 0.0;
        let fixed_near = cell.target_gr_db(cell.threshold_db + 2.0);
        let fixed_far = cell.target_gr_db(cell.threshold_db + 40.0);

        assert!(
            easy_near < fixed_near * 0.7,
            "just past the knee the cell should barely be at its ratio: {easy_near} vs {fixed_near}"
        );
        assert!(
            easy_far > fixed_far * 0.85,
            "deep overshoot should approach the dialled ratio: {easy_far} vs {fixed_far}"
        );
    }

    #[test]
    fn attack_reaches_63_percent_in_one_time_constant() {
        let mut params = plain_params();
        params.attack_ms = 10.0;
        let mut cell = cell_for(&params);

        let detector = db_to_linear(params.threshold_db + 12.0);
        let target = cell.target_gr_db(linear_to_db(detector));
        let mut samples = 0usize;
        while cell.gr_db[0] < target * 0.632 && samples < SR as usize {
            cell.follow(0, detector);
            samples += 1;
        }

        let seconds = samples as f32 / SR;
        assert!(
            (seconds - 0.010).abs() < 0.002,
            "attack took {seconds} s, expected ~0.010 s"
        );
    }

    #[test]
    fn release_slows_down_after_sustained_reduction() {
        let mut params = default_params();
        params.model = CompModel::Ssl;
        params.auto_release = true;
        params.attack_ms = 1.0;
        let loud = db_to_linear(params.threshold_db + 15.0);

        let recovery = |hold_samples: usize| {
            let mut cell = cell_for(&params);
            for _ in 0..hold_samples {
                cell.follow(0, loud);
            }
            let from = cell.gr_db[0];
            let mut samples = 0usize;
            while cell.gr_db[0] > from * 0.368 && samples < SR as usize * 4 {
                cell.follow(0, 0.0);
                samples += 1;
            }
            samples as f32 / SR
        };

        // A 5 ms tick and a 1 s hold both reach the same reduction; only the
        // program-dependent network makes the long one recover slower.
        let brief = recovery((SR * 0.005) as usize);
        let sustained = recovery(SR as usize);
        assert!(
            sustained > brief * 1.5,
            "brief {brief} s vs sustained {sustained} s"
        );
    }

    #[test]
    fn feedback_topology_reduces_less_than_feed_forward() {
        let mut params = plain_params();
        params.threshold_db = -30.0;
        params.ratio = 6.0;

        let mut forward = cell_for(&params);
        forward.feedback = 0.0;
        let mut looped = cell_for(&params);
        looped.feedback = 0.85;

        for n in 0..(SR as usize / 2) {
            let x = (n as f32 * 0.1).sin() * 0.7;
            forward.process_stereo(x, x);
            looped.process_stereo(x, x);
        }

        assert!(
            looped.gain_reduction_db() < forward.gain_reduction_db(),
            "feedback {} dB vs feed-forward {} dB",
            looped.gain_reduction_db(),
            forward.gain_reduction_db()
        );
        assert!(looped.gain_reduction_db() > 0.5, "feedback cell went inert");
    }

    #[test]
    fn saturator_tracks_the_shaper_and_stays_bounded() {
        let mut state = SaturatorState::default();
        let mut worst = 0.0_f32;
        for n in 0..4_000 {
            // Slow sweep: ADAA converges on the plain shaper when the input
            // barely moves between samples.
            let x = (n as f32 * 0.001).sin() * 2.5;
            let y = state.process(x);
            assert!(y.is_finite());
            assert!(y.abs() <= 0.7, "shaper exceeded its bound: {y}");
            worst = worst.max((y - clip_shape(x)).abs());
        }
        assert!(worst < 0.01, "ADAA drifted from the shaper by {worst}");
    }

    #[test]
    fn colour_adds_harmonics_without_moving_the_fader() {
        for model in [
            CompModel::Comp2500,
            CompModel::Distressor,
            CompModel::Avalon,
            CompModel::Ssl,
        ] {
            let mut params = default_params();
            params.model = model;
            params.color = 100.0;
            let mut cell = cell_for(&params);

            // Drive a tone at the level the stage is matched at and compare the
            // colour stage in isolation against its own input.
            let step = 2.0 * std::f32::consts::PI * 220.0 / SR;
            let mut dry_peak = 0.0_f32;
            let mut wet_peak = 0.0_f32;
            for n in 0..(SR as usize / 8) {
                let x = (n as f32 * step).sin() * COLOUR_MATCH_LEVEL;
                let y = cell.colour(0, x);
                assert!(y.is_finite());
                if n > SR as usize / 16 {
                    dry_peak = dry_peak.max(x.abs());
                    wet_peak = wet_peak.max(y.abs());
                }
            }

            let shift_db = linear_to_db(wet_peak) - linear_to_db(dry_peak);
            assert!(
                shift_db.abs() < 1.5,
                "{model:?} colour moved the level by {shift_db} dB"
            );
        }
    }

    #[test]
    fn sidechain_high_pass_rejects_lows_and_passes_highs() {
        let mut params = default_params();
        params.sidechain_hpf_hz = 200.0;
        let mut cell = cell_for(&params);

        let level_at = |cell: &mut GainCell, hz: f32| {
            cell.hpf_state = [HpfState::default(); 2];
            let step = 2.0 * std::f32::consts::PI * hz / SR;
            let mut peak = 0.0_f32;
            for n in 0..(SR as usize / 2) {
                let x = (n as f32 * step).sin() * 0.5;
                let out = cell.hpf_state[0].process(&cell.hpf, x);
                if n > SR as usize / 4 {
                    peak = peak.max(out.abs());
                }
            }
            peak
        };

        let low = level_at(&mut cell, 30.0);
        let high = level_at(&mut cell, 2_000.0);
        assert!(high > 0.45, "passband lost level: {high}");
        // 12 dB/oct, ~2.7 octaves below the corner: at least 30 dB down.
        assert!(low < high * 0.03, "low {low} vs high {high}");
    }

    #[test]
    fn sidechain_filter_at_the_minimum_is_bypassed() {
        let mut params = default_params();
        params.sidechain_hpf_hz = 20.0;
        let cell = cell_for(&params);
        assert!(!cell.hpf.active);
    }

    #[test]
    fn every_model_stays_finite_under_full_scale_input() {
        for model in [
            CompModel::Comp2500,
            CompModel::Distressor,
            CompModel::Avalon,
            CompModel::Ssl,
        ] {
            let mut params = default_params();
            params.model = model;
            params.color = 100.0;
            params.threshold_db = -40.0;
            params.ratio = 20.0;
            let mut cell = cell_for(&params);
            for n in 0..(SR as usize / 4) {
                let x = (n as f32 * 0.37).sin();
                let frame = cell.process_stereo(x, -x);
                assert!(frame.left.is_finite() && frame.right.is_finite());
                assert!(frame.left.abs() < 4.0, "{model:?} ran away: {}", frame.left);
            }
            assert!(cell.gain_reduction_db() > 1.0, "{model:?} did not reduce");
        }
    }

    #[test]
    fn stereo_link_ties_the_two_control_voltages_together() {
        let mut params = plain_params();
        params.stereo_link = 100.0;
        let mut linked = cell_for(&params);
        params.stereo_link = 0.0;
        let mut split = cell_for(&params);

        for n in 0..(SR as usize / 4) {
            // Loud left, quiet right.
            let x = (n as f32 * 0.11).sin();
            linked.process_stereo(x * 0.9, x * 0.02);
            split.process_stereo(x * 0.9, x * 0.02);
        }

        assert!(
            (linked.gr_db[0] - linked.gr_db[1]).abs() < 0.05,
            "linked channels drifted: {} vs {}",
            linked.gr_db[0],
            linked.gr_db[1]
        );
        assert!(
            split.gr_db[0] > split.gr_db[1] + 3.0,
            "unlinked channels tracked together: {} vs {}",
            split.gr_db[0],
            split.gr_db[1]
        );
    }

    #[test]
    fn smoothed_gain_never_steps() {
        let mut smoothed = Smoothed::new(SR, 1.0);
        smoothed.set_target(4.0);
        let mut previous = 1.0_f32;
        // 0.2 s is ten smoothing time constants: long enough to have arrived.
        for _ in 0..(SR as usize / 5) {
            let value = smoothed.next();
            assert!(
                (value - previous).abs() < 0.01,
                "step of {}",
                value - previous
            );
            previous = value;
        }
        assert!((previous - 4.0).abs() < 0.01, "never arrived: {previous}");
    }
}
