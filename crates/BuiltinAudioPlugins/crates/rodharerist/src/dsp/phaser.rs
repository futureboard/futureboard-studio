//! Swept-allpass stereo phasers — one engine, several voiced circuits.
//!
//! A phaser is not the allpass cascade on its own. The cascade has a flat
//! magnitude response by construction, so what you hear is the *sum* of the
//! dry path and the cascade: where the two arrive in antiphase the sum
//! cancels, and those moving nulls are the effect. Feeding back around the
//! cascade lifts the peaks between the nulls, which is the resonant "vowel"
//! character of an OTA pedal.
//!
//! Two consequences drive everything below:
//!
//! * The blend has a maximum. Notch depth peaks at an equal dry/wet sum and
//!   falls away on either side, so a Mix control has to reach 50% wet and
//!   stop. Running it to 100% wet returns the bare cascade — flat, and very
//!   nearly inaudible.
//! * Notch *count* is the stage count. Every two allpass stages buys one
//!   null, so a four-stage circuit has two and a twelve-stage has six. That,
//!   not depth or rate, is what separates a discreet Phase 90 from the
//!   swirling multi-notch boxes.
//!
//! Voices differ in stage count, regeneration, sweep span, whether the stages
//! share a corner or are staggered like a Univibe's unequal capacitors, LFO
//! symmetry and stereo spread. All state is fixed-size; nothing here
//! allocates or branches on unbounded data.

use builtin_dsp_core::{mix, time_constant};

use super::Lfo;
use super::smooth::Smoothed;

/// Glide time for depth/mix edits (see `smooth.rs`).
const SMOOTH_SECONDS: f32 = 0.010;

/// Soften allpass-coefficient jumps when the LFO / skew curve moves the corner
/// quickly — a ~1.5 ms one-pole is enough to kill zipper clicks without
/// smearing a musical sweep.
const FREQ_SMOOTH_SECONDS: f32 = 0.001_5;

/// Largest cascade any voice uses (six notches).
const MAX_STAGES: usize = 12;

/// Deepest useful wet blend: an equal dry/wet sum, where the nulls are total.
const MAX_BLEND: f32 = 0.5;

/// Where the resonant peak is allowed to land, as a linear gain (+1 dB).
///
/// Regeneration does not only sharpen the peaks between the nulls, it raises
/// them: at the frequency where the loop comes back in phase the cascade sees
/// a gain of `1 / (1 - feedback)`, so a voice sitting at 0.8 handed the rest
/// of the chain a 9.5 dB lift. Into an already-driven amp that is not
/// "resonant", it is clipping. A pedal's output stage does not do this, and
/// neither should these — a slight lift at the peak, never a shove.
const PEAK_TARGET: f32 = 1.122;

#[inline]
fn finite(x: f32) -> f32 {
    if x.is_finite() {
        super::flush_denormal(x.clamp(-8.0, 8.0))
    } else {
        0.0
    }
}

/// Which voiced phaser circuit the Mod slot is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PhaserVoice {
    /// "Vibe Phase 90" — the original four-stage script pedal.
    Phase90,
    /// "Molam Swirl" — Uni-Vibe-style staggered throb with regeneration, for the
    /// slow vowel sweep under an Isan / luk-thung lead.
    MolamSwirl,
    /// "Phin Vibe" — staggered stages and no feedback at all: a throb rather
    /// than a notch sweep, sitting under a phin-style melody.
    PhinVibe,
    /// "Khaen Swirl" — eight stages, four nulls, lush and continuous.
    KhaenSwirl,
    /// "Bi-Lam" — twelve stages in one cascade, slow and very wide.
    BiLam,
    /// "Isan Jet" — six stages, hard regeneration, fast and narrow up top.
    IsanJet,
    /// "Soft Phase" — two stages, one gentle notch: the subtle end nothing
    /// else in the lineup covers (a Phase-45 rather than a Phase-90).
    SoftPhase,
    /// "Wide Vibe" — real linked stereo spread (unlike every mono voice
    /// above), staggered corners, a lush width-forward swirl.
    WideVibe,
    /// "Chainsaw Phase" — six stages, hard feedback, a fast narrow sweep.
    Chainsaw,
    /// "Panic Vibe" — staggered corners and a lopsided snap.
    Panic,
    /// "Shred Phase" — twelve stages across the mid.
    Shred,
}

#[derive(Debug, Clone, Copy)]
struct Profile {
    stages: usize,
    /// Regeneration around the cascade. Approaching 1.0 rings.
    feedback: f32,
    sweep_lo: f32,
    sweep_hi: f32,
    /// Ratio between adjacent stage corners. 1.0 puts every stage on the same
    /// corner (a true notch phaser); above that the stages spread out the way
    /// a Univibe's four unequal capacitors do, which trades the sharp nulls
    /// for a broad wobble.
    stagger: f32,
    /// LFO shape exponent. 1.0 is a plain sine; above it the sweep dwells at
    /// the bottom and snaps back, which is the lopsided Univibe throb.
    skew: f32,
    /// Right-channel LFO offset in cycles. Pedal-authentic voices keep this
    /// near zero — independent L/R sweeps read as a doubled/ghosted guitar,
    /// especially after a stereo bounce or offline render.
    spread: f32,
    /// Multiplies the rate knob, so a "fast" voice reaches further.
    rate_scale: f32,
    /// Ceiling on the wet blend. Only the vibe voice sits below a full sum.
    max_blend: f32,
    /// True = process a mono sum through one cascade and copy to both outs
    /// (real stompbox behaviour). False = linked stereo LFOs with `spread`.
    mono: bool,
}

/// Map the shared 0..10 Rate knob onto Hertz for a phaser voice.
///
/// Cubic bias keeps most of the travel in the musical pedal range (~0.1–1.5 Hz).
/// The previous square curve put mid-knob near 2 Hz and full throw at 8 Hz, which
/// reads as a jet rather than a Phase-90 / Uni-Vibe swirl — and is why the Molam
/// voices felt like they were spinning.
#[inline]
fn rate_hz_from_knob(rate: f32, scale: f32) -> f32 {
    let t = (rate / 10.0).clamp(0.0, 1.0);
    (0.05 + t * t * t * 3.95) * scale
}

impl PhaserVoice {
    fn profile(self) -> Profile {
        match self {
            // Two notches, moderate regeneration: present without swamping
            // the note. Rate is pedal-slow so the default knob lands near a
            // script Phase 90's "musical" region rather than a jet.
            Self::Phase90 => Profile {
                stages: 4,
                feedback: 0.50,
                sweep_lo: 200.0,
                sweep_hi: 2_200.0,
                stagger: 1.0,
                skew: 1.0,
                spread: 0.0,
                rate_scale: 0.85,
                max_blend: MAX_BLEND,
                // Mono cascade like a real Phase 90 — dual L/R sweeps read as
                // ghosted / stuttering doubles in headphones and stereo bounces.
                mono: true,
            },
            // Molam / luk-thung lead swirl: Uni-Vibe DNA (mild stagger + soft
            // asymmetric throb) through a single mono cascade — a real pedal
            // does not run two independent L/R sweeps, and that dual path was
            // what read as "ซ้อน" / stutter under playback and offline render.
            Self::MolamSwirl => Profile {
                stages: 4,
                feedback: 0.42,
                sweep_lo: 160.0,
                sweep_hi: 1_900.0,
                stagger: 1.28,
                skew: 1.28,
                spread: 0.0,
                rate_scale: 0.55,
                max_blend: MAX_BLEND,
                mono: true,
            },
            // No feedback and widely staggered corners: the nulls never line
            // up, so nothing "swooshes" — it pulses. Still mono: a dual-LFO
            // throb under a phin line is the ghosted-double complaint.
            Self::PhinVibe => Profile {
                stages: 4,
                feedback: 0.0,
                sweep_lo: 280.0,
                sweep_hi: 1_600.0,
                stagger: 1.65,
                skew: 1.35,
                spread: 0.0,
                rate_scale: 0.90,
                max_blend: 0.44,
                mono: true,
            },
            Self::KhaenSwirl => Profile {
                stages: 8,
                feedback: 0.48,
                sweep_lo: 200.0,
                sweep_hi: 2_600.0,
                stagger: 1.12,
                skew: 1.15,
                spread: 0.0,
                rate_scale: 0.55,
                max_blend: MAX_BLEND,
                mono: true,
            },
            Self::BiLam => Profile {
                stages: 12,
                feedback: 0.34,
                sweep_lo: 180.0,
                sweep_hi: 2_400.0,
                stagger: 1.08,
                skew: 1.12,
                spread: 0.0,
                rate_scale: 0.40,
                max_blend: MAX_BLEND,
                mono: true,
            },
            // Intentionally the fast stereo jet — tiny linked spread only.
            Self::IsanJet => Profile {
                stages: 6,
                feedback: 0.55,
                sweep_lo: 400.0,
                sweep_hi: 3_600.0,
                stagger: 1.0,
                skew: 1.0,
                spread: 0.06,
                rate_scale: 1.35,
                max_blend: MAX_BLEND,
                mono: false,
            },
            // One notch, gentle regeneration, narrow sweep: the subtle "just
            // a little movement" voice — everything else in the lineup is at
            // least a two-notch swirl.
            Self::SoftPhase => Profile {
                stages: 2,
                feedback: 0.30,
                sweep_lo: 250.0,
                sweep_hi: 1_800.0,
                stagger: 1.0,
                skew: 1.0,
                spread: 0.0,
                rate_scale: 0.75,
                max_blend: MAX_BLEND,
                mono: true,
            },
            // The one voice that actually runs linked-but-independent L/R
            // sweeps at a musical width — every other voice here is mono for
            // exactly the ghosted-double reason explained above; this one
            // spends that risk deliberately for a genuinely wide swirl.
            Self::WideVibe => Profile {
                stages: 4,
                feedback: 0.38,
                sweep_lo: 180.0,
                sweep_hi: 2_100.0,
                stagger: 1.35,
                skew: 1.30,
                spread: 0.22,
                rate_scale: 0.60,
                max_blend: MAX_BLEND,
                mono: false,
            },
            Self::Chainsaw => Profile {
                stages: 6,
                feedback: 0.72,
                sweep_lo: 700.0,
                sweep_hi: 4_200.0,
                stagger: 1.0,
                skew: 1.05,
                spread: 0.0,
                rate_scale: 1.8,
                max_blend: MAX_BLEND,
                mono: true,
            },
            Self::Panic => Profile {
                stages: 4,
                feedback: 0.62,
                sweep_lo: 120.0,
                sweep_hi: 3_200.0,
                stagger: 1.85,
                skew: 2.4,
                spread: 0.0,
                rate_scale: 1.6,
                max_blend: MAX_BLEND,
                mono: true,
            },
            Self::Shred => Profile {
                stages: 12,
                feedback: 0.58,
                sweep_lo: 400.0,
                sweep_hi: 3_400.0,
                stagger: 1.04,
                skew: 1.0,
                spread: 0.04,
                rate_scale: 1.45,
                max_blend: MAX_BLEND,
                mono: false,
            },
        }
    }
}

/// One channel: allpass state plus the regeneration sample.
#[derive(Debug, Clone)]
struct Channel {
    z: [f32; MAX_STAGES],
    feedback: f32,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            z: [0.0; MAX_STAGES],
            feedback: 0.0,
        }
    }
}

impl Channel {
    fn reset(&mut self) {
        self.z = [0.0; MAX_STAGES];
        self.feedback = 0.0;
    }

    /// Run `stages` allpass sections whose coefficients are already resolved.
    #[inline]
    fn run(&mut self, input: f32, coeffs: &[f32; MAX_STAGES], stages: usize, feedback: f32) -> f32 {
        let mut x = input + self.feedback * feedback;
        for (z, &a) in self.z.iter_mut().zip(coeffs.iter()).take(stages) {
            // First-order allpass H(z) = (a - z^-1) / (1 - a*z^-1), as a
            // transposed direct form:
            //   y[n] = a*x[n] + s ;  s = a*y[n] - x[n]
            //
            // The sign pattern is the whole effect. Written with both signs
            // positive — H(z) = (a + z^-1) / (1 + a*z^-1), which is what this
            // file used to contain — the section is still an allpass, but its
            // quadrature point sits up near Nyquist instead of at the corner
            // the coefficient was solved for. At a = 0.9 that is 0.3 degrees
            // of shift where 90 was intended, so the cascade never reached
            // antiphase, no null ever formed, and the "phaser" was audible
            // only as the broad shelf its own feedback produced.
            let y = a * x + *z;
            *z = a * y - x;
            x = y;
        }
        // The loop is bounded and the coefficients stay inside (-1, 1), but a
        // hard-regenerating cascade still has to be kept from running away
        // across a rate or model change.
        self.feedback = finite(x);
        self.feedback
    }
}

#[derive(Debug, Clone)]
pub(super) struct Phaser {
    sample_rate: f32,
    voice: PhaserVoice,
    profile: Profile,
    /// Per-stage corner multipliers, resolved on the control path.
    stagger: [f32; MAX_STAGES],
    /// One shared LFO — L/R read as a locked pair via [`Lfo::tick_stereo`].
    lfo: Lfo,
    left: Channel,
    right: Channel,
    depth: Smoothed,
    blend: Smoothed,
    /// Compensates the regeneration's peak gain. Derived from the blend, so
    /// the effect never adds level at any Mix setting.
    makeup: Smoothed,
    /// Geometric centre of the sweep — depth widens around it rather than
    /// dragging the whole sweep up off the floor.
    centre_hz: f32,
    span: f32,
    /// One-pole on the sweep corners so allpass coefficients never jump.
    freq_smooth_l: f32,
    freq_smooth_r: f32,
    freq_smooth_coeff: f32,
}

impl Phaser {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let voice = PhaserVoice::Phase90;
        let mut phaser = Self {
            sample_rate: sr,
            voice,
            profile: voice.profile(),
            stagger: [1.0; MAX_STAGES],
            lfo: Lfo::new(),
            left: Channel::default(),
            right: Channel::default(),
            depth: Smoothed::new(sr, SMOOTH_SECONDS, 0.7),
            blend: Smoothed::new(sr, SMOOTH_SECONDS, 0.5),
            makeup: Smoothed::new(sr, SMOOTH_SECONDS, 1.0),
            centre_hz: 800.0,
            span: 1.0,
            freq_smooth_l: 800.0,
            freq_smooth_r: 800.0,
            freq_smooth_coeff: time_constant(sr, FREQ_SMOOTH_SECONDS),
        };
        phaser.adopt(voice);
        phaser
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        self.depth.set_time(sr, SMOOTH_SECONDS);
        self.blend.set_time(sr, SMOOTH_SECONDS);
        self.makeup.set_time(sr, SMOOTH_SECONDS);
        self.freq_smooth_coeff = time_constant(sr, FREQ_SMOOTH_SECONDS);
    }

    pub(super) fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
        self.lfo.reset();
        self.depth.snap();
        self.blend.snap();
        self.makeup.snap();
        self.freq_smooth_l = self.centre_hz;
        self.freq_smooth_r = self.centre_hz;
    }

    /// Resolve everything about a voice that does not change per sample.
    fn adopt(&mut self, voice: PhaserVoice) {
        self.voice = voice;
        let p = voice.profile();
        self.profile = p;
        // Centre the stagger on the sweep so a staggered voice covers the same
        // band overall as an unstaggered one.
        let mid = (p.stages as f32 - 1.0) * 0.5;
        for (i, slot) in self.stagger.iter_mut().enumerate() {
            *slot = if i < p.stages {
                p.stagger.powf(i as f32 - mid)
            } else {
                1.0
            };
        }
        self.centre_hz = (p.sweep_lo * p.sweep_hi).sqrt();
        self.span = p.sweep_hi / p.sweep_lo;
        self.freq_smooth_l = self.centre_hz;
        self.freq_smooth_r = self.centre_hz;
    }

    /// `rate` and `depth` are 0..10; `mix` is 0..100 %.
    pub(super) fn configure(&mut self, voice: PhaserVoice, rate: f32, depth: f32, mix: f32) {
        if voice != self.voice {
            self.adopt(voice);
            // Allpass state and the regeneration sample belong to the old
            // cascade; carrying them into a different stage count is a click.
            self.left.reset();
            self.right.reset();
        }
        // Pedal-biased rate: cubic curve + per-voice scale. Mid-knob sits in
        // the 0.1–1 Hz swirl range; full throw still reaches a lively jet on
        // IsanJet without the old 8 Hz helicopter.
        let rate_hz = rate_hz_from_knob(rate, self.profile.rate_scale);
        self.lfo.set_rate(rate_hz, self.sample_rate);
        self.depth.set_target((depth / 10.0).clamp(0.0, 1.0));
        // Mix reaches the deepest sum and stops. Past an equal blend the nulls
        // fill back in, so letting the knob run to bare wet would make the
        // effect quietly disappear at the top of its travel.
        let blend = (mix / 100.0).clamp(0.0, 1.0) * self.profile.max_blend;
        self.blend.set_target(blend);
        // Where the loop returns in phase the cascade reaches 1/(1 - feedback),
        // so the summed path peaks at (1 - blend) + blend/(1 - feedback). Undo
        // all but a decibel of that. Never above unity: at Mix at zero the dry
        // signal has to pass through untouched.
        let peak = (1.0 - blend) + blend / (1.0 - self.profile.feedback).max(0.05);
        self.makeup
            .set_target((PEAK_TARGET / peak.max(1.0e-3)).min(1.0));
    }

    /// Allpass coefficient for a corner at `freq`.
    ///
    /// `a = (1 - tan(w)) / (1 + tan(w))`, with tan taken from its first two
    /// series terms — accurate to well under a percent across the sweep and
    /// far closer than dropping the tan entirely, which is what the previous
    /// build did and which drifts badly once the sweep reaches a few kHz.
    #[inline]
    fn coeff(w: f32) -> f32 {
        let t = w * (1.0 + w * w * (1.0 / 3.0));
        (1.0 - t) / (1.0 + t)
    }

    #[inline]
    fn coeffs_for(&self, freq: f32, out: &mut [f32; MAX_STAGES]) {
        let base = std::f32::consts::PI * freq / self.sample_rate;
        for (slot, &ratio) in out.iter_mut().zip(self.stagger.iter()) {
            *slot = Self::coeff((base * ratio).clamp(1.0e-4, 1.2));
        }
    }

    #[inline]
    fn shape_freq(&self, raw: f32, depth: f32) -> f32 {
        let p = self.profile;
        let unit = (raw * 0.5 + 0.5).clamp(0.0, 1.0);
        let skewed = if p.skew == 1.0 {
            unit
        } else {
            unit.powf(p.skew)
        };
        self.centre_hz * self.span.powf((skewed - 0.5) * depth)
    }

    #[inline]
    fn smooth_freq(current: &mut f32, target: f32, coeff: f32) -> f32 {
        *current = target + (*current - target) * coeff;
        *current
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let depth = self.depth.tick();
        let blend = self.blend.tick();
        let makeup = self.makeup.tick();
        let p = self.profile;
        let coeff = self.freq_smooth_coeff;

        let (raw_l, raw_r) = self.lfo.tick_stereo(p.spread);
        let target_l = self.shape_freq(raw_l, depth);
        let target_r = if p.mono {
            target_l
        } else {
            self.shape_freq(raw_r, depth)
        };
        let freq_l = Self::smooth_freq(&mut self.freq_smooth_l, target_l, coeff);
        let freq_r = if p.mono {
            self.freq_smooth_r = freq_l;
            freq_l
        } else {
            Self::smooth_freq(&mut self.freq_smooth_r, target_r, coeff)
        };

        let mut coeffs = [0.0f32; MAX_STAGES];
        if p.mono {
            // Real stompbox: one cascade on the mid, copy to both outs. Dual
            // independent L/R sweeps were the "ซ้อน / กระตุก" under headphones
            // and offline stereo renders.
            let mid = 0.5 * (left + right);
            self.coeffs_for(freq_l, &mut coeffs);
            let wet = self.left.run(mid, &coeffs, p.stages, p.feedback);
            // Keep the unused channel's state from going stale / denormal.
            self.right.feedback = self.left.feedback;
            let out = finite(mix(mid, wet, blend) * makeup);
            (out, out)
        } else {
            self.coeffs_for(freq_l, &mut coeffs);
            let wet_l = self.left.run(left, &coeffs, p.stages, p.feedback);
            self.coeffs_for(freq_r, &mut coeffs);
            let wet_r = self.right.run(right, &coeffs, p.stages, p.feedback);
            (
                finite(mix(left, wet_l, blend) * makeup),
                finite(mix(right, wet_r, blend) * makeup),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PhaserVoice; 8] = [
        PhaserVoice::Phase90,
        PhaserVoice::MolamSwirl,
        PhaserVoice::PhinVibe,
        PhaserVoice::KhaenSwirl,
        PhaserVoice::BiLam,
        PhaserVoice::IsanJet,
        PhaserVoice::SoftPhase,
        PhaserVoice::WideVibe,
    ];

    /// Peak-to-null depth of the parked cascade's magnitude response, in dB.
    ///
    /// Chasing a moving null with an envelope follower measures the follower:
    /// at a playable rate the notch crosses a given frequency in a couple of
    /// milliseconds, so any window long enough to give a stable RMS averages
    /// the dip away. Parking the sweep — Depth at zero holds the corner on the
    /// voice's centre — turns the question back into a static one, and the
    /// spread between the response's peak and its deepest null is exactly the
    /// spectral bite the sweep will drag across the signal.
    fn notch_depth(voice: PhaserVoice, mix_pct: f32) -> f32 {
        let mut lo = f32::INFINITY;
        let mut hi = 0.0f32;
        // Fine enough to land in the bottom of a four-stage null, which is
        // narrow: a coarse grid straddles it and reports a shallow dip.
        const POINTS: usize = 400;
        for step in 0..POINTS {
            let freq = 90.0 * (9_000.0f32 / 90.0).powf(step as f32 / (POINTS - 1) as f32);
            let mut p = Phaser::new(48_000.0);
            p.configure(voice, 0.0, 0.0, mix_pct);
            p.reset();
            let tone = |n: usize| (n as f32 * freq * std::f32::consts::TAU / 48_000.0).sin();
            for n in 0..4_800 {
                p.process(tone(n), tone(n));
            }
            // Goertzel-style magnitude at the probe frequency itself.
            let n_win = 4_096;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for k in 0..n_win {
                let n = 4_800 + k;
                let y = p.process(tone(n), tone(n)).0 as f64;
                let w = k as f32 * freq * std::f32::consts::TAU / 48_000.0;
                re += y * w.cos() as f64;
                im -= y * w.sin() as f64;
            }
            let mag = ((re * re + im * im).sqrt() * 2.0 / n_win as f64) as f32;
            lo = lo.min(mag);
            hi = hi.max(mag);
        }
        20.0 * (hi / lo.max(1.0e-9)).log10()
    }

    /// The bug this file was rewritten for: the blend ran to bare wet, and an
    /// allpass cascade is flat, so the top of the Mix knob switched the effect
    /// off. Full Mix must be the *strongest* setting, not the weakest.
    #[test]
    fn full_mix_is_the_deepest_setting_not_the_shallowest() {
        for voice in ALL {
            let half = notch_depth(voice, 50.0);
            let full = notch_depth(voice, 100.0);
            assert!(
                full > half,
                "{voice:?}: mix 100% ({full:.1} dB) is weaker than 50% ({half:.1} dB)"
            );
            assert!(
                full > 12.0,
                "{voice:?}: only {full:.1} dB peak-to-null — that is a wobble, not a phaser"
            );
        }
    }

    /// Regeneration raises the peaks between the nulls as well as sharpening
    /// them, and the first cut of these voices shipped that raw: at feedback
    /// 0.8 the resonance handed everything downstream a 9.5 dB lift, which
    /// into a driven amp is not resonance, it is clipping. No voice may make
    /// the signal louder than a hair over unity, at any Mix setting.
    #[test]
    fn no_voice_boosts_the_signal() {
        for voice in ALL {
            for mix_pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
                let peak = peak_gain(voice, mix_pct);
                assert!(
                    peak < 1.6,
                    "{voice:?} at mix {mix_pct}%: peaks {:.1} dB above the input",
                    20.0 * peak.log10()
                );
            }
        }
        // ...and Mix at zero has to be a true bypass, not a quiet one.
        for voice in ALL {
            let unity = peak_gain(voice, 0.0);
            assert!(
                (unity - 1.0).abs() < 0.02,
                "{voice:?} at mix 0% is not unity: {unity:.3}"
            );
        }
    }

    /// Largest steady-state gain the parked cascade applies to any tone.
    fn peak_gain(voice: PhaserVoice, mix_pct: f32) -> f32 {
        let mut hi = 0.0f32;
        for step in 0..80 {
            let freq = 90.0 * (9_000.0f32 / 90.0).powf(step as f32 / 79.0);
            let mut p = Phaser::new(48_000.0);
            p.configure(voice, 0.0, 0.0, mix_pct);
            p.reset();
            let tone = |n: usize| (n as f32 * freq * std::f32::consts::TAU / 48_000.0).sin();
            for n in 0..4_800 {
                p.process(tone(n), tone(n));
            }
            let n_win = 4_096;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for k in 0..n_win {
                let y = p.process(tone(4_800 + k), tone(4_800 + k)).0 as f64;
                let w = k as f32 * freq * std::f32::consts::TAU / 48_000.0;
                re += y * w.cos() as f64;
                im -= y * w.sin() as f64;
            }
            hi = hi.max(((re * re + im * im).sqrt() * 2.0 / n_win as f64) as f32);
        }
        hi
    }

    /// Six voices that measure the same are one voice with six names.
    #[test]
    fn the_voices_are_audibly_distinct() {
        let render = |voice: PhaserVoice| {
            let mut p = Phaser::new(48_000.0);
            p.configure(voice, 5.0, 7.0, 100.0);
            p.reset();
            (0..24_000)
                .map(|n| {
                    let t = n as f32 / 48_000.0;
                    let x = (t * 220.0 * std::f32::consts::TAU).sin() * 0.3
                        + (t * 987.0 * std::f32::consts::TAU).sin() * 0.2;
                    p.process(x, x).0
                })
                .collect::<Vec<_>>()
        };
        let rendered: Vec<_> = ALL.iter().map(|v| render(*v)).collect();
        for i in 0..rendered.len() {
            for j in (i + 1)..rendered.len() {
                let rms = (rendered[i]
                    .iter()
                    .zip(rendered[j].iter())
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    / rendered[i].len() as f32)
                    .sqrt();
                assert!(rms > 0.01, "{:?} == {:?}: {rms}", ALL[i], ALL[j]);
            }
        }
    }

    /// More allpass stages must buy more nulls, which is what separates the
    /// discreet voices from the swirling ones.
    #[test]
    fn stage_count_tracks_the_voice() {
        assert_eq!(PhaserVoice::Phase90.profile().stages, 4);
        assert_eq!(PhaserVoice::KhaenSwirl.profile().stages, 8);
        assert_eq!(PhaserVoice::BiLam.profile().stages, 12);
        for voice in ALL {
            let p = voice.profile();
            assert!(p.stages <= MAX_STAGES, "{voice:?} overruns the cascade");
            assert!(p.stages % 2 == 0, "{voice:?} has a half-formed notch");
            assert!(p.feedback < 1.0, "{voice:?} would ring");
        }
    }

    #[test]
    fn stays_finite_and_bounded_under_abuse() {
        for voice in ALL {
            for &sr in &[44_100.0, 48_000.0, 96_000.0, 192_000.0] {
                let mut p = Phaser::new(sr);
                p.configure(voice, 10.0, 10.0, 100.0);
                p.reset();
                let mut peak = 0.0f32;
                for n in 0..8_000 {
                    // Full-scale square: the worst case for a regenerating
                    // cascade.
                    let x = if n % 41 < 20 { 1.0 } else { -1.0 };
                    let (l, r) = p.process(x, -x);
                    assert!(l.is_finite() && r.is_finite(), "{voice:?} sr={sr}");
                    peak = peak.max(l.abs()).max(r.abs());
                }
                assert!(peak < 6.0, "{voice:?} sr={sr} ran away: {peak}");
            }
        }
    }

    /// Switching voice mid-signal must not hand the new cascade the old one's
    /// charge.
    #[test]
    fn changing_voice_does_not_click() {
        let mut p = Phaser::new(48_000.0);
        p.configure(PhaserVoice::BiLam, 4.0, 8.0, 100.0);
        p.reset();
        let mut previous = 0.0;
        let mut worst = 0.0f32;
        for n in 0..16_000 {
            if n == 8_000 {
                p.configure(PhaserVoice::PhinVibe, 4.0, 8.0, 100.0);
            }
            let x = (n as f32 * 0.05).sin() * 0.4;
            let y = p.process(x, x).0;
            if n > 100 {
                worst = worst.max((y - previous).abs());
            }
            previous = y;
        }
        assert!(worst < 0.5, "voice change clicked: {worst}");
    }

    /// Rate at a typical musical knob setting must stay in the pedal range —
    /// the bug report that drove the cubic remap was "หมุนไวมาก" (spins too fast).
    #[test]
    fn mid_knob_rate_is_a_pedal_not_a_helicopter() {
        let phase90 = rate_hz_from_knob(5.0, PhaserVoice::Phase90.profile().rate_scale);
        let molam = rate_hz_from_knob(5.0, PhaserVoice::MolamSwirl.profile().rate_scale);
        let jet = rate_hz_from_knob(10.0, PhaserVoice::IsanJet.profile().rate_scale);
        assert!(
            phase90 < 0.8,
            "Phase90 at Rate 5 is {phase90:.2} Hz — still too fast for a script pedal"
        );
        assert!(
            molam < 0.5,
            "MolamSwirl at Rate 5 is {molam:.2} Hz — molam swirl should be slow"
        );
        assert!(
            jet < 6.0,
            "IsanJet at Rate 10 is {jet:.2} Hz — jet, not helicopter"
        );
        assert!(
            PhaserVoice::MolamSwirl.profile().stagger > 1.2,
            "MolamSwirl must stagger stages (Uni-Vibe DNA), not run a linear Phase-90 cascade"
        );
        assert!(
            PhaserVoice::MolamSwirl.profile().skew > 1.15,
            "MolamSwirl needs an asymmetric LFO throb"
        );
        assert!(
            PhaserVoice::MolamSwirl.profile().mono,
            "MolamSwirl must be mono-cascade — dual L/R sweeps were the stutter/ghost complaint"
        );
    }

    /// Mono pedal voices must keep L == R for a centred input, otherwise a
    /// stereo bounce / offline render hears two overlapping sweeps.
    #[test]
    fn mono_voices_do_not_ghost_a_centred_input() {
        for voice in [
            PhaserVoice::Phase90,
            PhaserVoice::MolamSwirl,
            PhaserVoice::PhinVibe,
            PhaserVoice::KhaenSwirl,
            PhaserVoice::BiLam,
            PhaserVoice::SoftPhase,
        ] {
            let mut p = Phaser::new(48_000.0);
            p.configure(voice, 4.0, 7.0, 100.0);
            p.reset();
            for n in 0..8_000 {
                let x = (n as f32 * 0.03).sin() * 0.4;
                let (l, r) = p.process(x, x);
                assert!(
                    (l - r).abs() < 1.0e-5,
                    "{voice:?} split a mono input: L={l} R={r}"
                );
            }
        }
    }
}
