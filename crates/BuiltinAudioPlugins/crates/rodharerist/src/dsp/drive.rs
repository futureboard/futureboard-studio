//! Overdrive / boost / fuzz pedals — multiple voicings sharing one processor.
//!
//! The legacy six voicings model the real pedal topology instead of drawing a
//! curve. The Drive knob feeds a gain stage whose coupling network only lifts
//! the band *above* its corner, so the low end arrives at the diodes at unity
//! and never reaches their threshold at all. That is how a pedal keeps its bass
//! clean — by not driving it — and it replaces the old split-band trick, which
//! re-blended a full-level clean low band onto a compressed clipped one and so
//! grew muddier the harder it was driven.
//!
//! The clipper itself is [`DiodeClipper`]: an implicit diode node solved per
//! sample, not a `tanh`. Its state is the charge held by the fixed capacitor
//! *and* by the junctions themselves, so the node's history moves where the
//! next sample lands, edges round off instead of cornering into
//! digital-sounding fizz, and the two diodes leave conduction at their own
//! rates — which is where the even harmonics come from. The diode exponential,
//! not a chosen clamp, sets the ceiling. It runs 2× oversampled
//! ([`Oversampler2x`]) with its state at that rate, and the gain/mix controls
//! are smoothed ([`Smoothed`]) so live knob drags don't zipper.

use builtin_dsp_core::{make_eq_coefficients, mix};

use super::drive_models::{DcBlock, DsClassic, MetalCore, SuperDrive, TightRift};
use super::nonlinear::{DiodeClipper, DiodeParams};
use super::smooth::{Oversampler2x, Smoothed};
use super::{DriveModel, StereoBiquad};

/// Glide time for gain/level/mix edits (fast enough to feel immediate).
const SMOOTH_SECONDS: f32 = 0.010;

/// Drive taper for the legacy voicings.
///
/// These pedals reach full saturation at a few times unity gain, so a pre-gain
/// that is *linear* in the knob spends its whole useful range in the bottom
/// fifth of the travel — measured, Screamer gained 6.7 dB and went from 3% to
/// 58% THD between 0 and 2/10, then did nothing at all above it. Tapering
/// makes the knob roughly linear in dB instead, so the clean-to-crunch
/// transition lands in the middle where it can be dialled in.
#[inline]
fn drive_taper(g01: f32) -> f32 {
    g01.clamp(0.0, 1.0).powf(2.1)
}

/// The diode pair, its charge storage, and the clipping corner per voicing.
///
/// The diode voltages are normalized so a nominal drop is 1.0 — that puts the
/// clipping threshold at plugin unity, so the node is transparent under it and
/// its ceiling needs no makeup gain. Everything here is a component choice, not
/// a tone control: matched silicon has a tight knee, germanium is much softer,
/// and an unequal pair is a genuinely asymmetric pedal.
///
/// `tau_pos`/`tau_neg` are junction transit times, which decide how much
/// diffusion charge each diode holds while it conducts. This is the parameter
/// that separates the family: a fast silicon switching part (nanoseconds)
/// stores essentially nothing and clips cleanly, while germanium and rectifier
/// junctions (microseconds) hold enough charge to leave conduction slowly,
/// which thickens the tone and — because the two diodes are unequal — skews the
/// half-cycle durations into real even harmonics that survive DC blocking.
///
/// `bass_hz` is the corner of the gain stage's own coupling network. Below it
/// the Drive knob adds no gain at all, so the low end never reaches the diodes
/// and cannot be turned to mud — a TS-9 really does leave everything under
/// ~720 Hz at unity, which is why it stays tight wherever the knob sits.
#[derive(Debug, Clone, Copy)]
struct DiodeVoicing {
    v_pos: f32,
    v_neg: f32,
    knee_pos: f32,
    knee_neg: f32,
    tau_pos: f32,
    tau_neg: f32,
    smooth_hz: f32,
    bass_hz: f32,
}

impl DiodeVoicing {
    #[allow(clippy::too_many_arguments)]
    const fn new(
        v_pos: f32,
        v_neg: f32,
        knee_pos: f32,
        knee_neg: f32,
        tau_pos: f32,
        tau_neg: f32,
        smooth_hz: f32,
        bass_hz: f32,
    ) -> Self {
        Self {
            v_pos,
            v_neg,
            knee_pos,
            knee_neg,
            tau_pos,
            tau_neg,
            smooth_hz,
            bass_hz,
        }
    }

    fn for_model(model: DriveModel) -> Self {
        match model {
            // Matched silicon in the feedback loop, boosting from the classic
            // 720 Hz corner up.
            DriveModel::Screamer => {
                Self::new(1.0, 0.97, 0.075, 0.074, 1.2e-8, 1.6e-8, 11_000.0, 720.0)
            }
            // Germanium: soft knee and a low corner, so it stays transparent
            // rather than mid-honky.
            DriveModel::Minotaur => {
                Self::new(1.0, 1.02, 0.190, 0.190, 1.1e-6, 1.5e-6, 16_000.0, 380.0)
            }
            // Silicon to ground with a hard knee and a low corner — the whole
            // guitar band gets driven, which is where the filth comes from.
            DriveModel::Rat => Self::new(1.0, 0.965, 0.042, 0.041, 4.0e-7, 6.5e-7, 7_000.0, 180.0),
            // Two diodes up against one down: real asymmetry.
            DriveModel::Breaker => {
                Self::new(0.70, 1.30, 0.045, 0.043, 1.5e-7, 4.0e-7, 9_000.0, 300.0)
            }
            // Leaky germanium, very soft, and a corner low enough that
            // everything reaches the diodes.
            DriveModel::Fuzz => Self::new(0.76, 1.24, 0.250, 0.220, 3.0e-6, 1.4e-6, 5_000.0, 70.0),
            // Germanium like the Minotaur but voiced brighter.
            DriveModel::Centurion => {
                Self::new(1.0, 0.975, 0.160, 0.158, 8.0e-7, 1.05e-6, 14_000.0, 540.0)
            }
            // Near-matched silicon, harder knee than Breaker and a brighter,
            // higher corner than Rat — a blues/rock rhythm crunch, not a
            // filth pedal.
            DriveModel::AmberCrunch => {
                Self::new(1.0, 0.98, 0.055, 0.053, 2.2e-7, 2.6e-7, 10_000.0, 260.0)
            }
            // Silicon fuzz: fast junctions (no germanium leak) and a hard
            // asymmetric pair, so it stays tight and immediate where Face
            // Fuzz's germanium pair is soft and smeary.
            DriveModel::CopperFuzz => {
                Self::new(0.65, 1.35, 0.130, 0.120, 2.5e-7, 4.5e-7, 6_500.0, 105.0)
            }
            // Rangemaster: the corner is high, so only the top reaches the
            // diodes and the low strings stay clean.
            DriveModel::GlassBoost => {
                Self::new(1.0, 0.99, 0.090, 0.088, 8.0e-9, 1.0e-8, 14_000.0, 1_400.0)
            }
            // Bluesbreaker: a wide soft knee, little asymmetry, a low corner.
            DriveModel::VelvetOd => {
                Self::new(0.90, 1.10, 0.140, 0.135, 9.0e-7, 1.1e-6, 8_000.0, 220.0)
            }
            // Octavia: one junction much lower than the other, so the even
            // harmonics read as an octave.
            DriveModel::OctaveFuzz => {
                Self::new(0.42, 1.55, 0.080, 0.200, 1.8e-7, 8.0e-7, 4_200.0, 90.0)
            }
            // Bass overdrive: the corner sits under the fundamental.
            DriveModel::BassGrind => {
                Self::new(0.85, 1.15, 0.070, 0.065, 6.0e-7, 8.0e-7, 3_200.0, 40.0)
            }
            DriveModel::HonkBox => {
                Self::new(1.0, 0.96, 0.060, 0.058, 2.0e-7, 2.4e-7, 6_000.0, 480.0)
            }
            DriveModel::SmoothLead => {
                Self::new(0.80, 1.20, 0.180, 0.170, 1.4e-6, 1.8e-6, 3_600.0, 520.0)
            }
            // Gated fuzz: the two time constants disagree, so the clipper
            // opens on the attack and chokes as the note decays.
            DriveModel::SputterFuzz => {
                Self::new(0.38, 1.70, 0.280, 0.060, 4.0e-6, 1.2e-7, 2_800.0, 80.0)
            }
            DriveModel::CleanPush => {
                Self::new(1.0, 1.0, 0.420, 0.420, 2.0e-6, 2.0e-6, 16_000.0, 30.0)
            }
            // Hard knee, fast junctions, a high corner so the low end is
            // already gone before the clip. The scoop is the post EQ.
            DriveModel::RazorSaw => {
                Self::new(0.55, 0.55, 0.028, 0.028, 8.0e-8, 8.0e-8, 9_000.0, 280.0)
            }
            // The corner sits high: low strings stay near unity and never
            // reach the diodes. What clips is tight.
            DriveModel::DjentClamp => {
                Self::new(0.70, 0.72, 0.034, 0.032, 1.2e-7, 1.4e-7, 8_500.0, 420.0)
            }
            DriveModel::Chainsaw => {
                Self::new(0.48, 1.40, 0.022, 0.090, 6.0e-8, 3.0e-7, 7_500.0, 900.0)
            }
            // Everything reaches the diodes, and they leave slowly.
            DriveModel::DoomWall => {
                Self::new(0.80, 1.20, 0.040, 0.038, 9.0e-7, 1.1e-6, 2_400.0, 35.0)
            }
            DriveModel::MidSpike => {
                Self::new(0.90, 0.92, 0.048, 0.046, 1.6e-7, 1.8e-7, 5_500.0, 700.0)
            }
            DriveModel::IcePick => {
                Self::new(0.62, 0.64, 0.030, 0.030, 5.0e-8, 6.0e-8, 14_000.0, 1_800.0)
            }
            // The two junctions disagree harder than Sputter Fuzz, and the
            // corner is higher so the choke is tighter.
            DriveModel::GateGrind => {
                Self::new(0.32, 1.80, 0.220, 0.040, 6.0e-6, 8.0e-8, 4_800.0, 160.0)
            }
            DriveModel::RedLead => {
                Self::new(0.78, 1.05, 0.036, 0.034, 1.4e-7, 1.8e-7, 8_000.0, 240.0)
            }
            // Dedicated-topology models never reach this table.
            _ => Self::new(1.0, 1.0, 0.075, 0.075, 2.0e-8, 2.0e-8, 12_000.0, 400.0),
        }
    }
}

/// Stereo one-pole high-pass. The gain stage's feedback network: it decides
/// which band the Drive knob actually boosts.
#[derive(Debug, Clone)]
struct StereoOnePoleHp {
    a: f32,
    y_l: f32,
    y_r: f32,
}

impl StereoOnePoleHp {
    fn new() -> Self {
        Self {
            a: 1.0,
            y_l: 0.0,
            y_r: 0.0,
        }
    }

    fn set(&mut self, freq: f32, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        let f = freq.clamp(1.0, sr * 0.45);
        self.a = 1.0 - (-std::f32::consts::TAU * f / sr).exp();
    }

    fn reset(&mut self) {
        self.y_l = 0.0;
        self.y_r = 0.0;
    }

    /// Returns the *high* band; the low band is `x - high`.
    #[inline]
    fn run(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.y_l += self.a * (l - self.y_l);
        self.y_r += self.a * (r - self.y_r);
        if !self.y_l.is_finite() {
            self.y_l = 0.0;
        }
        if !self.y_r.is_finite() {
            self.y_r = 0.0;
        }
        (l - self.y_l, r - self.y_r)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Drive {
    sample_rate: f32,
    model: DriveModel,
    /// Gain applied to the band above the clipping corner only. The bass keeps
    /// unity all the way up the knob, so it can never be driven into mud.
    pre_gain: Smoothed,
    out_gain: Smoothed,
    mix: Smoothed,
    gain_hp: StereoOnePoleHp,
    mid_boost: StereoBiquad,
    tone_lpf: StereoBiquad,
    clipper: DiodeClipper,
    dc: DcBlock,
    oversampler: Oversampler2x,
    // The four modern models own full dedicated topologies (multi-stage,
    // higher oversampling, dynamics) — see `drive_models.rs`. The legacy six
    // keep the generic path above.
    ds_one: DsClassic,
    super_drive: SuperDrive,
    metal_core: MetalCore,
    tight_rift: TightRift,
}

impl Drive {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let voicing = DiodeVoicing::for_model(DriveModel::Screamer);
        Self {
            sample_rate: sr,
            model: DriveModel::Screamer,
            pre_gain: Smoothed::new(sr, SMOOTH_SECONDS, 1.0),
            out_gain: Smoothed::new(sr, SMOOTH_SECONDS, 1.0),
            mix: Smoothed::new(sr, SMOOTH_SECONDS, 1.0),
            gain_hp: StereoOnePoleHp::new(),
            mid_boost: StereoBiquad::none(),
            tone_lpf: StereoBiquad::none(),
            clipper: DiodeClipper::new(DiodeParams::new(
                voicing.v_pos,
                voicing.v_neg,
                voicing.knee_pos,
                voicing.knee_neg,
                voicing.tau_pos,
                voicing.tau_neg,
                voicing.smooth_hz,
                sr * 2.0,
            )),
            dc: DcBlock::new(sr),
            oversampler: Oversampler2x::new(),
            ds_one: DsClassic::new(sr),
            super_drive: SuperDrive::new(sr),
            metal_core: MetalCore::new(sr),
            tight_rift: TightRift::new(sr),
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.pre_gain.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.out_gain.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.mix.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.dc.set_sample_rate(self.sample_rate);
        self.ds_one.set_sample_rate(self.sample_rate);
        self.super_drive.set_sample_rate(self.sample_rate);
        self.metal_core.set_sample_rate(self.sample_rate);
        self.tight_rift.set_sample_rate(self.sample_rate);
    }

    pub(super) fn reset(&mut self) {
        self.gain_hp.reset();
        self.mid_boost.reset();
        self.tone_lpf.reset();
        self.clipper.reset();
        self.dc.reset();
        self.oversampler.reset();
        self.pre_gain.snap();
        self.out_gain.snap();
        self.mix.snap();
        self.ds_one.reset();
        self.super_drive.reset();
        self.metal_core.reset();
        self.tight_rift.reset();
    }

    /// `gain`, `tone`, `level` are the editor's 0..10 knobs.
    pub(super) fn configure(&mut self, model: DriveModel, gain: f32, tone: f32, level: f32) {
        self.model = model;
        // Dedicated-topology models: route and return — the generic voicing
        // table below only serves the legacy six.
        match model {
            DriveModel::DsOne => return self.ds_one.configure(gain, tone, level),
            DriveModel::SuperDrive => return self.super_drive.configure(gain, tone, level),
            DriveModel::MetalCore => return self.metal_core.configure(gain, tone, level),
            DriveModel::TightRift => return self.tight_rift.configure(gain, tone, level),
            _ => {}
        }
        let g = drive_taper((gain / 10.0).clamp(0.0, 1.0));
        let t = (tone / 10.0).clamp(0.0, 1.0);
        let lvl = (level / 10.0).clamp(0.0, 1.0);
        let sr = self.sample_rate;

        let voicing = DiodeVoicing::for_model(model);
        let params = DiodeParams::new(
            voicing.v_pos,
            voicing.v_neg,
            voicing.knee_pos,
            voicing.knee_neg,
            voicing.tau_pos,
            voicing.tau_neg,
            voicing.smooth_hz,
            sr * 2.0,
        );
        self.clipper.set_params(params);
        // The gain stage boosts exactly the band the diodes can see.
        self.gain_hp.set(voicing.bass_hz, sr);

        // (pre_gain, out_gain, mix) targets. The low end of each `pre` range
        // sits *below* unity so Drive at 0 is genuinely close to clean.
        let (pre, out, mix_amount) = match model {
            DriveModel::Screamer => (0.40 + g * 24.0, 0.17 + lvl * 0.75, 1.0),
            DriveModel::Minotaur => (0.45 + g * 11.0, 0.20 + lvl * 0.66, 0.85),
            DriveModel::Rat => (0.35 + g * 60.0, 0.16 + lvl * 0.82, 1.0),
            DriveModel::Breaker => (0.45 + g * 14.0, 0.21 + lvl * 0.68, 0.92),
            DriveModel::Fuzz => (0.35 + g * 54.0, 0.10 + lvl * 0.55, 1.0),
            DriveModel::Centurion => (0.42 + g * 16.0, 0.20 + lvl * 0.74, 0.88),
            DriveModel::AmberCrunch => (0.42 + g * 20.0, 0.19 + lvl * 0.72, 0.90),
            DriveModel::CopperFuzz => (0.35 + g * 58.0, 0.12 + lvl * 0.58, 1.0),
            DriveModel::GlassBoost => (0.70 + g * 6.0, 0.28 + lvl * 0.70, 0.80),
            DriveModel::VelvetOd => (0.50 + g * 9.0, 0.22 + lvl * 0.64, 0.86),
            DriveModel::OctaveFuzz => (0.30 + g * 36.0, 0.11 + lvl * 0.50, 1.0),
            DriveModel::BassGrind => (0.40 + g * 16.0, 0.16 + lvl * 0.60, 0.94),
            DriveModel::HonkBox => (0.40 + g * 22.0, 0.15 + lvl * 0.68, 1.0),
            DriveModel::SmoothLead => (0.45 + g * 26.0, 0.14 + lvl * 0.58, 0.96),
            DriveModel::SputterFuzz => (0.30 + g * 46.0, 0.09 + lvl * 0.48, 1.0),
            DriveModel::CleanPush => (0.85 + g * 3.5, 0.35 + lvl * 0.70, 0.62),
            DriveModel::RazorSaw => (0.30 + g * 72.0, 0.12 + lvl * 0.70, 1.0),
            DriveModel::DjentClamp => (0.32 + g * 50.0, 0.14 + lvl * 0.66, 1.0),
            DriveModel::Chainsaw => (0.22 + g * 84.0, 0.10 + lvl * 0.62, 1.0),
            DriveModel::DoomWall => (0.28 + g * 66.0, 0.11 + lvl * 0.55, 1.0),
            DriveModel::MidSpike => (0.34 + g * 44.0, 0.13 + lvl * 0.64, 1.0),
            DriveModel::IcePick => (0.40 + g * 38.0, 0.15 + lvl * 0.68, 0.96),
            DriveModel::GateGrind => (0.26 + g * 58.0, 0.09 + lvl * 0.50, 1.0),
            DriveModel::RedLead => (0.30 + g * 54.0, 0.13 + lvl * 0.66, 1.0),
            // Dedicated-topology models returned above.
            _ => (1.0, 1.0, 1.0),
        };
        self.pre_gain.set_target(pre);
        // No inverse compensation for the pre-gain: the diodes are their own
        // level reference, so subtracting the gain that was added would take
        // back level the clipper already removed and leave the pedal loudest
        // with the knob at zero. The small rise with `g` keeps a harder setting
        // reading a little hotter, which is the one direction Drive may move.
        self.out_gain.set_target(out * (1.0 + g * 0.18));
        self.mix.set_target(mix_amount);

        // Pre-clip mid emphasis and the post tone low-pass per model. Both are
        // opened up relative to the waveshaper era: the diode branch rounds its
        // own top end, so the low-pass no longer has to hide fizz by being dark.
        match model {
            DriveModel::Screamer => {
                // A little more mid than the first cut, so the screamer
                // does not sit as a flat boost into the amp.
                self.mid_boost
                    .set(make_eq_coefficients("bell", 720.0, 7.5, 0.7, sr));
                let cutoff = 3_200.0 + t * 6_800.0;
                self.tone_lpf.set(make_eq_coefficients(
                    "lowpass",
                    cutoff.min(sr * 0.45),
                    0.0,
                    0.707,
                    sr,
                ));
            }
            DriveModel::Minotaur => {
                // A small low-mid bell. Without it the boost was flat.
                self.mid_boost
                    .set(make_eq_coefficients("bell", 420.0, 2.5, 0.6, sr));
                let cutoff = (5_000.0 + t * 9_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::Rat => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 1_100.0, 5.0, 0.9, sr));
                let cutoff = 1_600.0 + t * 7_400.0;
                self.tone_lpf.set(make_eq_coefficients(
                    "lowpass",
                    cutoff.min(sr * 0.45),
                    0.0,
                    0.707,
                    sr,
                ));
            }
            DriveModel::Breaker => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 650.0, 2.5, 0.8, sr));
                let cutoff = 3_600.0 + t * 8_000.0;
                self.tone_lpf.set(make_eq_coefficients(
                    "lowpass",
                    cutoff.min(sr * 0.45),
                    0.0,
                    0.707,
                    sr,
                ));
            }
            DriveModel::Fuzz => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 400.0, 4.0, 0.6, sr));
                let cutoff = (1_200.0 + t * 4_600.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::Centurion => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 780.0, 4.5, 0.75, sr));
                let cutoff = 4_000.0 + t * 8_000.0;
                self.tone_lpf.set(make_eq_coefficients(
                    "lowpass",
                    cutoff.min(sr * 0.45),
                    0.0,
                    0.707,
                    sr,
                ));
            }
            DriveModel::AmberCrunch => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 850.0, 4.0, 0.8, sr));
                let cutoff = 3_800.0 + t * 7_500.0;
                self.tone_lpf.set(make_eq_coefficients(
                    "lowpass",
                    cutoff.min(sr * 0.45),
                    0.0,
                    0.707,
                    sr,
                ));
            }
            DriveModel::CopperFuzz => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 500.0, 3.5, 0.65, sr));
                let cutoff = (1_500.0 + t * 5_200.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::GlassBoost => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 2_800.0, 5.0, 0.8, sr));
                let cutoff = (6_000.0 + t * 8_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::VelvetOd => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 480.0, 3.0, 0.6, sr));
                let cutoff = (2_400.0 + t * 4_800.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::OctaveFuzz => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 700.0, 6.0, 0.9, sr));
                let cutoff = (1_600.0 + t * 2_800.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::BassGrind => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 160.0, 5.0, 0.7, sr));
                let cutoff = (800.0 + t * 2_200.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::HonkBox => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 1_050.0, 8.0, 1.3, sr));
                let cutoff = (2_000.0 + t * 3_800.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::SmoothLead => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 620.0, 3.0, 0.55, sr));
                let cutoff = (1_200.0 + t * 2_600.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::SputterFuzz => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 320.0, 5.0, 0.5, sr));
                let cutoff = (900.0 + t * 2_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::CleanPush => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 2_000.0, 1.2, 0.5, sr));
                let cutoff = (8_000.0 + t * 6_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::RazorSaw => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 750.0, -8.0, 0.7, sr));
                let cutoff = (4_200.0 + t * 6_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::DjentClamp => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 1_800.0, 3.5, 0.9, sr));
                let cutoff = (3_400.0 + t * 5_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::Chainsaw => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 1_600.0, 10.0, 2.4, sr));
                let cutoff = (2_800.0 + t * 4_200.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::DoomWall => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 180.0, 6.0, 0.6, sr));
                let cutoff = (700.0 + t * 1_600.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::MidSpike => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 1_150.0, 12.0, 2.8, sr));
                let cutoff = (2_200.0 + t * 3_400.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::IcePick => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 450.0, -6.0, 0.8, sr));
                let cutoff = (6_500.0 + t * 6_000.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::GateGrind => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 900.0, 4.0, 1.1, sr));
                let cutoff = (1_800.0 + t * 2_600.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            DriveModel::RedLead => {
                self.mid_boost
                    .set(make_eq_coefficients("bell", 980.0, 7.0, 1.0, sr));
                let cutoff = (3_000.0 + t * 4_400.0).min(sr * 0.45);
                self.tone_lpf
                    .set(make_eq_coefficients("lowpass", cutoff, 0.0, 0.707, sr));
            }
            // Dedicated-topology models returned above.
            _ => {}
        }
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        match self.model {
            DriveModel::DsOne => return self.ds_one.process(left, right),
            DriveModel::SuperDrive => return self.super_drive.process(left, right),
            DriveModel::MetalCore => return self.metal_core.process(left, right),
            DriveModel::TightRift => return self.tight_rift.process(left, right),
            _ => {}
        }
        let pre = self.pre_gain.tick();
        let out = self.out_gain.tick();
        let mix_amount = self.mix.tick();

        // Voicing goes *into* the clipper (pre-emphasis), not after it.
        let (em_l, em_r) = self.mid_boost.run(left, right);
        // The gain stage only lifts the band the diodes will see. Below the
        // corner the signal stays at unity and arrives at the node too small to
        // turn a diode on, so the low end passes clean without a parallel band
        // to re-blend and without getting louder as the clipped band compresses.
        let (hi_l, hi_r) = self.gain_hp.run(em_l, em_r);
        let dr_l = em_l + (pre - 1.0) * hi_l;
        let dr_r = em_r + (pre - 1.0) * hi_r;
        // Solve the diode node at 2× rate — its capacitor state lives there.
        let clipper = &mut self.clipper;
        let (sh_l, sh_r) = self
            .oversampler
            .process_stereo(dr_l, dr_r, |a, b| clipper.process(a, b));
        let (t_l, t_r) = self.tone_lpf.run(sh_l, sh_r);
        // Unequal diodes leave a standing charge on the coupling capacitor —
        // wanted as duty-cycle asymmetry, not as an offset in the mix.
        let (d_l, d_r) = self.dc.run(t_l, t_r);
        let wet_l = d_l * out;
        let wet_r = d_r * out;
        (mix(left, wet_l, mix_amount), mix(right, wet_r, mix_amount))
    }
}
