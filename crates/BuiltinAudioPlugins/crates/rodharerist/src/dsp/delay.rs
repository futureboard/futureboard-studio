//! The Delay slot: five echo voicings behind one set of Time/Feedback/Mix/Tone
//! knobs (the same shared-knob pattern the Mod and Reverb slots use).
//!
//! All voicings are the same topology — a pair of interpolated delay lines with
//! a filtered, saturated feedback path — differing in feedback-path colour
//! (cutoff, low-end body, saturation), wow/flutter depth and stereo routing.
//! A Time change never glides the read head (which bends the repeats' pitch):
//! it crossfades from the old tap to the new one, see [`TapFade`]. One preallocated buffer set serves every voicing, so a
//! model change never reallocates and is safe from the control thread.
//!
//! Ring buffers are sized up front for the maximum delay time in
//! [`DelayStage::new`] / [`DelayStage::set_sample_rate`];
//! [`DelayStage::process`] performs no allocation, locking or logging.

use builtin_dsp_core::{make_eq_coefficients, mix};

use super::smooth::Smoothed;
use super::{DelayModel, InterpDelay, Lfo, StereoBiquad, soft_clip};

const MAX_DELAY_MS: f32 = 1_300.0; // headroom above the 1200 ms max knob

/// Glide time for feedback/mix edits.
const SMOOTH_SECONDS: f32 = 0.010;

/// How long a Time change crossfades from the old tap to the new one.
const TIME_FADE_SECONDS: f32 = 0.050;

/// The Dual voicing's right-hand tap, as a fraction of the Time knob — a
/// quarter note on the left against a dotted eighth on the right.
const DUAL_RIGHT_RATIO: f32 = 0.75;

/// How the two lines are fed. Every voicing reads both lines; only the write
/// side differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Routing {
    /// Independent left/right lines, each with its own feedback.
    Stereo,
    /// Input enters the left line only; each line feeds the *other* one, so
    /// repeats alternate across the image.
    PingPong,
    /// Independent lines like `Stereo`, but the right tap runs at
    /// [`Voicing::right_ratio`] of the Time knob.
    Dual,
    /// Independent lines like `Stereo`, the right one on its own Time and
    /// Feedback knobs.
    Split,
}

/// Per-voicing tuning. A model switch replaces these wholesale; nothing
/// reallocates.
#[derive(Debug, Clone, Copy)]
struct Voicing {
    /// Feedback-path lowpass at the Tone knob's centre detent, in Hz. The Tone
    /// knob scales around this, so each voicing keeps its own character across
    /// the whole knob range.
    tone_hz: f32,
    /// Feedback-path highpass in Hz, or 0 for none — thins the repeats so they
    /// sit under the dry signal instead of muddying it (bucket-brigade echoes
    /// lose low end on every pass).
    body_hz: f32,
    /// Wow/flutter depth in seconds of read-head displacement (0 = none).
    flutter_s: f32,
    /// Feedback-path saturation drive. 1.0 is tape compression right at unity;
    /// smaller values stay effectively linear until well above it (clean
    /// digital repeats) while still bounding a runaway loop.
    drive: f32,
    /// Right-tap fraction of the Time knob. Only [`Routing::Dual`] reads it.
    right_ratio: f32,
    routing: Routing,
}

impl Voicing {
    fn for_model(model: DelayModel) -> Self {
        match model {
            // Warm, saturated, audibly moving — the original tape echo.
            DelayModel::Tape => Self {
                tone_hz: 4_000.0,
                body_hz: 0.0,
                flutter_s: 0.000_3,
                drive: 1.0,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            // Clean and full-bandwidth: repeats that stay where you put them.
            DelayModel::Digital => Self {
                tone_hz: 16_000.0,
                body_hz: 0.0,
                flutter_s: 0.0,
                drive: 0.25,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            // Bucket-brigade: dark, thinning, compressed repeats.
            DelayModel::Analog => Self {
                tone_hz: 2_200.0,
                body_hz: 160.0,
                flutter_s: 0.000_08,
                drive: 1.4,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            // Repeats alternating left/right across the image.
            DelayModel::PingPong => Self {
                tone_hz: 6_000.0,
                body_hz: 0.0,
                flutter_s: 0.000_05,
                drive: 0.8,
                right_ratio: 1.0,
                routing: Routing::PingPong,
            },
            // Two rhythmic taps: quarter left, dotted eighth right.
            DelayModel::Dual => Self {
                tone_hz: 5_000.0,
                body_hz: 0.0,
                flutter_s: 0.000_12,
                drive: 0.9,
                right_ratio: DUAL_RIGHT_RATIO,
                routing: Routing::Dual,
            },
            // Clean repeats, each side on its own time and feedback.
            DelayModel::Stereo => Self {
                tone_hz: 12_000.0,
                body_hz: 0.0,
                flutter_s: 0.0,
                drive: 0.35,
                right_ratio: 1.0,
                routing: Routing::Split,
            },
            // Bright and still. The Time knob is what makes it a slap.
            DelayModel::SlapEcho => Self {
                tone_hz: 9_000.0,
                body_hz: 0.0,
                flutter_s: 0.000_02,
                drive: 0.55,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            // Oil-can: dark, saturated, the head wanders.
            DelayModel::OilCan => Self {
                tone_hz: 1_400.0,
                body_hz: 240.0,
                flutter_s: 0.000_9,
                drive: 1.8,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            DelayModel::DarkEcho => Self {
                tone_hz: 900.0,
                body_hz: 320.0,
                flutter_s: 0.000_15,
                drive: 1.25,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            DelayModel::AmbientEcho => Self {
                tone_hz: 2_800.0,
                body_hz: 180.0,
                flutter_s: 0.000_18,
                drive: 0.45,
                right_ratio: 1.0,
                routing: Routing::PingPong,
            },
            DelayModel::WowEcho => Self {
                tone_hz: 3_200.0,
                body_hz: 80.0,
                flutter_s: 0.001_4,
                drive: 1.15,
                right_ratio: 1.0,
                routing: Routing::Stereo,
            },
            // Quarter against a triplet eighth (2/3).
            DelayModel::TripletTap => Self {
                tone_hz: 4_200.0,
                body_hz: 0.0,
                flutter_s: 0.000_06,
                drive: 0.7,
                right_ratio: 2.0 / 3.0,
                routing: Routing::Dual,
            },
        }
    }
}

/// Feedback-path saturation. `drive` scales into the shaper and back out, so a
/// small drive is effectively linear at normal levels while still bounding the
/// loop — a delay that can only ever reach `1/drive` cannot run away.
#[inline]
fn saturate(x: f32, drive: f32) -> f32 {
    soft_clip(x * drive) / drive
}

/// One side's read tap. A new delay time crossfades from the old tap to a
/// second one at the new time instead of moving the head, so the repeats keep
/// their pitch. A target that arrives mid-fade waits for the fade to finish and
/// then fades on from there, so a knob drag steps through the latest values.
#[derive(Debug, Clone, Copy)]
struct TapFade {
    /// The tap that is sounding (the old one while a fade runs).
    from: f32,
    /// The tap being faded in.
    to: f32,
    /// The latest requested time, picked up when the running fade ends.
    pending: f32,
    /// Fade position 0..1, and its per-sample step.
    phase: f32,
    step: f32,
    fading: bool,
    /// Cleared by a reset: the next target snaps instead of fading, since the
    /// line holds nothing worth fading from.
    armed: bool,
}

impl TapFade {
    fn new(sample_rate: f32, samples: f32) -> Self {
        Self {
            from: samples,
            to: samples,
            pending: samples,
            phase: 0.0,
            step: Self::step_for(sample_rate),
            fading: false,
            armed: false,
        }
    }

    fn step_for(sample_rate: f32) -> f32 {
        1.0 / (TIME_FADE_SECONDS * sample_rate).max(1.0)
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.step = Self::step_for(sample_rate);
        self.disarm();
    }

    /// Drops any running fade; the next target snaps.
    fn disarm(&mut self) {
        self.armed = false;
        self.fading = false;
        self.from = self.pending;
        self.to = self.pending;
    }

    fn set_target(&mut self, samples: f32) {
        self.pending = samples;
        if !self.armed {
            self.armed = true;
            self.from = samples;
            self.to = samples;
        }
    }

    /// The two taps and the new tap's gain (0 when no fade is running, in
    /// which case only `from` needs reading).
    #[inline]
    fn tick(&mut self) -> (f32, f32, f32) {
        if !self.fading {
            if (self.pending - self.from).abs() < 0.5 {
                // A sub-sample move is inaudible; take it without a fade.
                self.from = self.pending;
                return (self.from, self.from, 0.0);
            }
            self.to = self.pending;
            self.phase = 0.0;
            self.fading = true;
        }
        self.phase += self.step;
        if self.phase >= 1.0 {
            self.from = self.to;
            self.fading = false;
            return (self.from, self.from, 0.0);
        }
        // Raised cosine: the two gains always sum to one, so a fade can never
        // push more level into the feedback loop than either tap holds.
        let gain = 0.5 - 0.5 * (std::f32::consts::PI * self.phase).cos();
        (self.from, self.to, gain)
    }
}

/// Reads one side's tap, mixing in the second tap while a time fade runs.
#[inline]
fn read_tap(line: &InterpDelay, (from, to, gain): (f32, f32, f32), offset: f32) -> f32 {
    let old = line.read_interp(from + offset);
    if gain > 0.0 {
        old + (line.read_interp(to + offset) - old) * gain
    } else {
        old
    }
}

#[derive(Debug, Clone)]
pub(super) struct DelayStage {
    sample_rate: f32,
    model: DelayModel,
    voicing: Voicing,
    line_l: InterpDelay,
    line_r: InterpDelay,
    /// Each side's read tap. The right one carries whatever the routing gives
    /// that side: the left time, the Dual ratio of it, or its own knob.
    tap_l: TapFade,
    tap_r: TapFade,
    feedback: Smoothed,
    /// The right side's own feedback, read by the Split routing.
    feedback_r: Smoothed,
    mix: Smoothed,
    flutter: Lfo,
    flutter_depth: f32,
    /// Band-limits the feedback (tape head roll-off / BBD clock filter).
    tone: StereoBiquad,
    /// Optional feedback highpass; `None` for the voicings with `body_hz == 0`.
    body: StereoBiquad,
    /// Cutoff the installed `tone` filter was built for. Rebuilding a biquad
    /// clears its state, which is audible inside a feedback loop — so a Tone
    /// drag only rebuilds when the cutoff actually moves.
    tone_hz_current: f32,
    body_hz_current: f32,
    fb_l: f32,
    fb_r: f32,
}

impl DelayStage {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let capacity = ((sr * MAX_DELAY_MS * 0.001) as usize).max(4);
        let mut flutter = Lfo::new();
        flutter.set_rate(3.0, sr);
        let voicing = Voicing::for_model(DelayModel::Tape);
        Self {
            sample_rate: sr,
            model: DelayModel::Tape,
            voicing,
            line_l: InterpDelay::new(capacity),
            line_r: InterpDelay::new(capacity),
            tap_l: TapFade::new(sr, 0.42 * sr),
            tap_r: TapFade::new(sr, 0.42 * sr),
            feedback: Smoothed::new(sr, SMOOTH_SECONDS, 0.35),
            feedback_r: Smoothed::new(sr, SMOOTH_SECONDS, 0.35),
            mix: Smoothed::new(sr, SMOOTH_SECONDS, 0.3),
            flutter,
            flutter_depth: voicing.flutter_s * sr,
            tone: StereoBiquad::none(),
            body: StereoBiquad::none(),
            tone_hz_current: 0.0,
            body_hz_current: 0.0,
            fb_l: 0.0,
            fb_r: 0.0,
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        let capacity = ((sr * MAX_DELAY_MS * 0.001) as usize).max(4);
        self.sample_rate = sr;
        self.line_l = InterpDelay::new(capacity);
        self.line_r = InterpDelay::new(capacity);
        self.flutter.set_rate(3.0, sr);
        self.flutter_depth = self.voicing.flutter_s * sr;
        self.tap_l.set_sample_rate(sr);
        self.tap_r.set_sample_rate(sr);
        self.feedback.set_time(sr, SMOOTH_SECONDS);
        self.feedback_r.set_time(sr, SMOOTH_SECONDS);
        self.mix.set_time(sr, SMOOTH_SECONDS);
        // Coefficients are rate-dependent; force a rebuild on the next
        // `configure` rather than keeping filters tuned for the old rate.
        self.tone_hz_current = 0.0;
        self.body_hz_current = 0.0;
    }

    pub(super) fn reset(&mut self) {
        self.line_l.clear();
        self.line_r.clear();
        self.flutter.reset();
        self.tone.reset();
        self.body.reset();
        self.fb_l = 0.0;
        self.fb_r = 0.0;
        self.tap_l.disarm();
        self.tap_r.disarm();
        self.feedback.snap();
        self.feedback_r.snap();
        self.mix.snap();
    }

    /// `time_ms` 40..1200, `fb` 0..100 %, `mix` 0..100 %, `tone` 0..10 (5 is
    /// the voicing's own centre); `time_r_ms` and `fb_r` are the right
    /// side's, read only by the Stereo model. Switching models clears the
    /// lines: the old tail was written under a different routing and colour,
    /// and letting it bleed into the new voicing sounds like a fault, not a
    /// crossfade.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure(
        &mut self,
        model: DelayModel,
        time_ms: f32,
        fb: f32,
        mix: f32,
        tone: f32,
        time_r_ms: f32,
        fb_r: f32,
    ) {
        if self.model != model {
            self.model = model;
            self.voicing = Voicing::for_model(model);
            self.flutter_depth = self.voicing.flutter_s * self.sample_rate;
            self.reset();
        }

        let samples = |ms: f32| (ms.clamp(40.0, 1_200.0) * 0.001 * self.sample_rate).max(1.0);
        let left = samples(time_ms);
        self.tap_l.set_target(left);
        self.tap_r.set_target(match self.voicing.routing {
            Routing::Dual => left * self.voicing.right_ratio,
            Routing::Split => samples(time_r_ms),
            Routing::Stereo | Routing::PingPong => left,
        });
        self.feedback.set_target((fb / 100.0).clamp(0.0, 0.95));
        self.feedback_r.set_target((fb_r / 100.0).clamp(0.0, 0.95));
        self.mix.set_target((mix / 100.0).clamp(0.0, 1.0));

        // Tone rides the voicing's own centre: ±2 octaves around `tone_hz`.
        let octaves = (tone.clamp(0.0, 10.0) - 5.0) / 5.0 * 2.0;
        let tone_hz = (self.voicing.tone_hz * octaves.exp2()).clamp(400.0, 18_000.0);
        if (tone_hz - self.tone_hz_current).abs() > 1.0 {
            self.tone_hz_current = tone_hz;
            self.tone.set(make_eq_coefficients(
                "lowpass",
                tone_hz,
                0.0,
                0.707,
                self.sample_rate,
            ));
        }

        let body_hz = self.voicing.body_hz;
        if (body_hz - self.body_hz_current).abs() > 1.0 {
            self.body_hz_current = body_hz;
            self.body.set(if body_hz > 0.0 {
                make_eq_coefficients("highpass", body_hz, 0.0, 0.707, self.sample_rate)
            } else {
                None
            });
        }
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        // A time change crossfades between taps rather than moving the head,
        // so the repeats keep their pitch. Wow and flutter ride on top with
        // opposite modulation per channel for width.
        let tap_l = self.tap_l.tick();
        let tap_r = self.tap_r.tick();
        let feedback = self.feedback.tick();
        let feedback_r = self.feedback_r.tick();
        let mix_amount = self.mix.tick();
        let flut = self.flutter.tick() * self.flutter_depth;
        let right_feedback = match self.voicing.routing {
            Routing::Split => feedback_r,
            Routing::Stereo | Routing::PingPong | Routing::Dual => feedback,
        };

        let echo_l = read_tap(&self.line_l, tap_l, flut);
        let echo_r = read_tap(&self.line_r, tap_r, -flut);

        // Colour and compress the feedback, then write the lines per routing.
        let (tone_l, tone_r) = self.tone.run(echo_l, echo_r);
        let (fb_l, fb_r) = self.body.run(tone_l, tone_r);
        let drive = self.voicing.drive;
        self.fb_l = saturate(fb_l * feedback, drive);
        self.fb_r = saturate(fb_r * right_feedback, drive);

        match self.voicing.routing {
            Routing::Stereo | Routing::Dual | Routing::Split => {
                self.line_l.write_sample(left + self.fb_l);
                self.line_r.write_sample(right + self.fb_r);
            }
            Routing::PingPong => {
                // Input enters one side only; each line regenerates into the
                // other, so successive repeats alternate across the image.
                self.line_l.write_sample((left + right) * 0.5 + self.fb_r);
                self.line_r.write_sample(self.fb_l);
            }
        }

        (
            mix(left, echo_l, mix_amount),
            mix(right, echo_r, mix_amount),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(stage: &mut DelayStage, samples: usize) -> (f32, f32) {
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for n in 0..samples {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = stage.process(x, x);
            assert!(l.is_finite() && r.is_finite(), "non-finite at n={n}");
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
        }
        (peak_l, peak_r)
    }

    #[test]
    fn every_model_is_finite_and_bounded_at_maximum_feedback() {
        for &model in DelayModel::ALL {
            let mut stage = DelayStage::new(SR);
            // 100 % feedback is clamped to 0.95 internally; a hot, sustained
            // input on top of that is the worst case a runaway loop can see.
            stage.configure(model, 120.0, 100.0, 100.0, 5.0, 120.0, 100.0);
            for n in 0..(SR as usize) {
                let x = ((n as f32) * 0.05).sin();
                let (l, r) = stage.process(x, x);
                assert!(l.is_finite() && r.is_finite(), "{model:?} non-finite");
                assert!(
                    l.abs() < 8.0 && r.abs() < 8.0,
                    "{model:?} ran away: {l} {r}"
                );
            }
        }
    }

    #[test]
    fn ping_pong_puts_the_first_repeat_on_one_side_only() {
        let mut stage = DelayStage::new(SR);
        stage.configure(DelayModel::PingPong, 100.0, 60.0, 100.0, 5.0, 100.0, 60.0);
        // Long enough for the first repeat (100 ms) but not the second.
        let mut first_l = 0.0f32;
        let mut first_r = 0.0f32;
        for n in 0..((SR * 0.15) as usize) {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = stage.process(x, x);
            if n > 100 {
                first_l = first_l.max(l.abs());
                first_r = first_r.max(r.abs());
            }
        }
        assert!(first_l > 0.05, "no first repeat on the left: {first_l}");
        assert!(
            first_r < first_l * 0.1,
            "first repeat leaked to the right: {first_r} vs {first_l}"
        );
    }

    #[test]
    fn dual_places_the_right_tap_before_the_left_one() {
        let mut stage = DelayStage::new(SR);
        stage.configure(DelayModel::Dual, 400.0, 0.0, 100.0, 5.0, 400.0, 0.0);
        let mut peak_l_at = 0usize;
        let mut peak_r_at = 0usize;
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for n in 0..((SR * 0.6) as usize) {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = stage.process(x, x);
            if n > 100 {
                if l.abs() > peak_l {
                    peak_l = l.abs();
                    peak_l_at = n;
                }
                if r.abs() > peak_r {
                    peak_r = r.abs();
                    peak_r_at = n;
                }
            }
        }
        let expected_r = (SR * 0.4 * DUAL_RIGHT_RATIO) as usize;
        let expected_l = (SR * 0.4) as usize;
        assert!(
            peak_r_at.abs_diff(expected_r) < 600,
            "right tap at {peak_r_at}, expected ~{expected_r}"
        );
        assert!(
            peak_l_at.abs_diff(expected_l) < 600,
            "left tap at {peak_l_at}, expected ~{expected_l}"
        );
    }

    #[test]
    fn digital_repeats_keep_more_high_end_than_analog() {
        // Same impulse, same time/feedback: the analogue voicing's darker,
        // thinner feedback path must lose more energy per pass.
        fn tail_energy(model: DelayModel) -> f32 {
            let mut stage = DelayStage::new(SR);
            stage.configure(model, 60.0, 80.0, 100.0, 5.0, 60.0, 80.0);
            let mut energy = 0.0f32;
            for n in 0..((SR * 0.5) as usize) {
                let x = if n == 0 { 1.0 } else { 0.0 };
                let (l, _) = stage.process(x, x);
                // Skip the dry impulse; measure the regenerating tail only.
                if n > 1_000 {
                    energy += l * l;
                }
            }
            energy
        }
        let digital = tail_energy(DelayModel::Digital);
        let analog = tail_energy(DelayModel::Analog);
        assert!(
            digital > analog * 1.5,
            "digital tail {digital} not brighter/longer than analog {analog}"
        );
    }

    #[test]
    fn a_model_switch_clears_the_previous_tail() {
        let mut stage = DelayStage::new(SR);
        stage.configure(DelayModel::Tape, 400.0, 90.0, 100.0, 5.0, 400.0, 90.0);
        let _ = run(&mut stage, (SR * 0.2) as usize);
        stage.configure(DelayModel::Digital, 400.0, 90.0, 100.0, 5.0, 400.0, 90.0);
        // With the lines cleared and no new input, the wet path is silent.
        for _ in 0..64 {
            let (l, r) = stage.process(0.0, 0.0);
            assert!(
                l.abs() < 1.0e-6 && r.abs() < 1.0e-6,
                "tail survived: {l} {r}"
            );
        }
    }

    #[test]
    fn tone_knob_darkens_the_repeats() {
        fn tail_energy(tone: f32) -> f32 {
            let mut stage = DelayStage::new(SR);
            stage.configure(DelayModel::Digital, 60.0, 80.0, 100.0, tone, 60.0, 80.0);
            let mut energy = 0.0f32;
            for n in 0..((SR * 0.5) as usize) {
                let x = if n == 0 { 1.0 } else { 0.0 };
                let (l, _) = stage.process(x, x);
                if n > 1_000 {
                    energy += l * l;
                }
            }
            energy
        }
        assert!(tail_energy(10.0) > tail_energy(0.0));
    }

    /// Where each side's first repeat lands, in samples after the impulse.
    fn first_taps(stage: &mut DelayStage, seconds: f32) -> (usize, usize) {
        let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);
        let (mut at_l, mut at_r) = (0usize, 0usize);
        for n in 0..((SR * seconds) as usize) {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = stage.process(x, x);
            if n > 100 {
                if l.abs() > peak_l {
                    (peak_l, at_l) = (l.abs(), n);
                }
                if r.abs() > peak_r {
                    (peak_r, at_r) = (r.abs(), n);
                }
            }
        }
        (at_l, at_r)
    }

    #[test]
    fn stereo_runs_each_side_on_its_own_time() {
        let mut stage = DelayStage::new(SR);
        stage.configure(DelayModel::Stereo, 200.0, 0.0, 100.0, 5.0, 500.0, 0.0);
        let (at_l, at_r) = first_taps(&mut stage, 0.7);
        let (want_l, want_r) = ((SR * 0.2) as usize, (SR * 0.5) as usize);
        assert!(
            at_l.abs_diff(want_l) < 600,
            "left at {at_l}, want ~{want_l}"
        );
        assert!(
            at_r.abs_diff(want_r) < 600,
            "right at {at_r}, want ~{want_r}"
        );
    }

    #[test]
    fn stereo_feedback_is_per_side() {
        // Left repeats regenerate, right ones die after the first.
        let mut stage = DelayStage::new(SR);
        stage.configure(DelayModel::Stereo, 100.0, 80.0, 100.0, 5.0, 100.0, 0.0);
        let (mut late_l, mut late_r) = (0.0f32, 0.0f32);
        for n in 0..((SR * 0.5) as usize) {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = stage.process(x, x);
            // Past the first repeat (100 ms): only regenerated echoes.
            if n > (SR * 0.15) as usize {
                late_l = late_l.max(l.abs());
                late_r = late_r.max(r.abs());
            }
        }
        assert!(late_l > 0.1, "left did not regenerate: {late_l}");
        assert!(
            late_r < 1.0e-3,
            "right regenerated with 0 feedback: {late_r}"
        );
    }

    /// Worst pitch error of the wet signal, as a fraction, while the Time knob
    /// jumps from 300 ms to 500 ms under a steady 1 kHz tone. 1 kHz is exactly
    /// 48 samples at 48 kHz, and both times are whole periods, so a pitch-true
    /// change keeps every period at 48 samples.
    fn pitch_error_across_a_time_change(model: DelayModel) -> f32 {
        let tone = |n: usize| (std::f32::consts::TAU * n as f32 / 48.0).sin();
        let mut stage = DelayStage::new(SR);
        stage.configure(model, 300.0, 0.0, 100.0, 5.0, 300.0, 0.0);
        let warm = (SR * 0.7) as usize;
        for n in 0..warm {
            stage.process(tone(n), tone(n));
        }
        stage.configure(model, 500.0, 0.0, 100.0, 5.0, 500.0, 0.0);
        let mut worst = 0.0f32;
        let mut prev = 0.0f32;
        let mut last_crossing: Option<f32> = None;
        for n in warm..warm + (SR * 0.4) as usize {
            let (l, _) = stage.process(tone(n), tone(n));
            if prev < 0.0 && l >= 0.0 {
                let at = (n - 1) as f32 + prev / (prev - l);
                if let Some(before) = last_crossing {
                    worst = worst.max(((at - before) - 48.0).abs() / 48.0);
                }
                last_crossing = Some(at);
            }
            prev = l;
        }
        worst
    }

    #[test]
    fn a_time_change_keeps_the_pitch_of_the_repeats() {
        // Wow and flutter are meant to move the pitch; the deepest voicings
        // would hide the time change behind their own modulation.
        for &model in DelayModel::ALL {
            if Voicing::for_model(model).flutter_s > 0.000_5 {
                continue;
            }
            let error = pitch_error_across_a_time_change(model);
            assert!(error < 0.03, "{model:?} bent the pitch by {error}");
        }
    }

    #[test]
    fn a_time_drag_does_not_click_and_lands_on_the_last_value() {
        // A block-rate drag from 300 ms to 713 ms under a steady tone: every
        // step crossfades, so the wet signal never jumps further per sample
        // than the tone itself does (2π/48 ≈ 0.13).
        let tone = |n: usize| (std::f32::consts::TAU * n as f32 / 48.0).sin();
        let mut stage = DelayStage::new(SR);
        stage.configure(DelayModel::Digital, 300.0, 0.0, 100.0, 5.0, 300.0, 0.0);
        let mut n = 0usize;
        while n < (SR * 0.8) as usize {
            stage.process(tone(n), tone(n));
            n += 1;
        }
        let mut prev = stage.process(tone(n), tone(n)).0;
        n += 1;
        let mut worst = 0.0f32;
        for step in 0..=59 {
            let time = 300.0 + step as f32 * 7.0;
            stage.configure(DelayModel::Digital, time, 0.0, 100.0, 5.0, time, 0.0);
            for _ in 0..256 {
                let (l, _) = stage.process(tone(n), tone(n));
                worst = worst.max((l - prev).abs());
                prev = l;
                n += 1;
            }
        }
        assert!(worst < 0.2, "the drag clicked: a {worst} step");

        // Settle, clear, and check where an impulse comes back.
        for _ in 0..(SR * 0.2) as usize {
            stage.process(0.0, 0.0);
        }
        stage.line_l.clear();
        stage.line_r.clear();
        let (at_l, _) = first_taps(&mut stage, 0.8);
        let want = (SR * 0.713) as usize;
        assert!(at_l.abs_diff(want) < 2, "landed at {at_l}, want {want}");
    }

    #[test]
    fn only_stereo_reads_the_right_side_knobs() {
        // Same left knobs, wildly different right ones: Digital ignores them.
        let mut a = DelayStage::new(SR);
        let mut b = DelayStage::new(SR);
        a.configure(DelayModel::Digital, 300.0, 40.0, 100.0, 5.0, 300.0, 40.0);
        b.configure(DelayModel::Digital, 300.0, 40.0, 100.0, 5.0, 900.0, 0.0);
        assert_eq!(first_taps(&mut a, 0.5), first_taps(&mut b, 0.5));
    }
}
