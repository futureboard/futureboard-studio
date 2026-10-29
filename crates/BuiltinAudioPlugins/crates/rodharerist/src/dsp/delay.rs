//! The Delay slot: five echo voicings behind one set of Time/Feedback/Mix/Tone
//! knobs (the same shared-knob pattern the Mod and Reverb slots use).
//!
//! All voicings are the same topology — a pair of interpolated delay lines with
//! a filtered, saturated feedback path — differing in feedback-path colour
//! (cutoff, low-end body, saturation), wow/flutter depth, time-slew character
//! and stereo routing. One preallocated buffer set serves every voicing, so a
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
    /// [`DUAL_RIGHT_RATIO`] of the Time knob.
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
    /// Slew time for the delay time itself. Long enough on the analogue
    /// voicings that a Time drag reads as a tape-style pitch glide; short on
    /// the digital ones, where a glide would be wrong.
    time_slew_s: f32,
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
                time_slew_s: 0.100,
                routing: Routing::Stereo,
            },
            // Clean and full-bandwidth: repeats that stay where you put them.
            DelayModel::Digital => Self {
                tone_hz: 16_000.0,
                body_hz: 0.0,
                flutter_s: 0.0,
                drive: 0.25,
                time_slew_s: 0.020,
                routing: Routing::Stereo,
            },
            // Bucket-brigade: dark, thinning, compressed repeats.
            DelayModel::Analog => Self {
                tone_hz: 2_200.0,
                body_hz: 160.0,
                flutter_s: 0.000_08,
                drive: 1.4,
                time_slew_s: 0.090,
                routing: Routing::Stereo,
            },
            // Repeats alternating left/right across the image.
            DelayModel::PingPong => Self {
                tone_hz: 6_000.0,
                body_hz: 0.0,
                flutter_s: 0.000_05,
                drive: 0.8,
                time_slew_s: 0.025,
                routing: Routing::PingPong,
            },
            // Two rhythmic taps: quarter left, dotted eighth right.
            DelayModel::Dual => Self {
                tone_hz: 5_000.0,
                body_hz: 0.0,
                flutter_s: 0.000_12,
                drive: 0.9,
                time_slew_s: 0.030,
                routing: Routing::Dual,
            },
            // Clean repeats, each side on its own time and feedback.
            DelayModel::Stereo => Self {
                tone_hz: 12_000.0,
                body_hz: 0.0,
                flutter_s: 0.0,
                drive: 0.35,
                time_slew_s: 0.020,
                routing: Routing::Split,
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

#[derive(Debug, Clone)]
pub(super) struct DelayStage {
    sample_rate: f32,
    model: DelayModel,
    voicing: Voicing,
    line_l: InterpDelay,
    line_r: InterpDelay,
    delay_samples: Smoothed,
    feedback: Smoothed,
    /// The right side's own time and feedback, read by the Split routing.
    delay_samples_r: Smoothed,
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
            delay_samples: Smoothed::new(sr, voicing.time_slew_s, 0.42 * sr),
            feedback: Smoothed::new(sr, SMOOTH_SECONDS, 0.35),
            delay_samples_r: Smoothed::new(sr, voicing.time_slew_s, 0.315 * sr),
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
        self.delay_samples.set_time(sr, self.voicing.time_slew_s);
        self.feedback.set_time(sr, SMOOTH_SECONDS);
        self.delay_samples_r.set_time(sr, self.voicing.time_slew_s);
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
        self.delay_samples.snap();
        self.feedback.snap();
        self.delay_samples_r.snap();
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
            self.delay_samples
                .set_time(self.sample_rate, self.voicing.time_slew_s);
            self.delay_samples_r
                .set_time(self.sample_rate, self.voicing.time_slew_s);
            self.reset();
        }

        let samples = |ms: f32| (ms.clamp(40.0, 1_200.0) * 0.001 * self.sample_rate).max(1.0);
        self.delay_samples.set_target(samples(time_ms));
        self.delay_samples_r.set_target(samples(time_r_ms));
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
        // Slewed read position: on the analogue voicings a time-knob change
        // glides the head (pitch bend) instead of jumping it (click). Wow and
        // flutter ride on top with opposite modulation per channel for width.
        let delay_samples = self.delay_samples.tick();
        let feedback = self.feedback.tick();
        let delay_samples_r = self.delay_samples_r.tick();
        let feedback_r = self.feedback_r.tick();
        let mix_amount = self.mix.tick();
        let flut = self.flutter.tick() * self.flutter_depth;
        let (right_samples, right_feedback) = match self.voicing.routing {
            Routing::Dual => (delay_samples * DUAL_RIGHT_RATIO, feedback),
            Routing::Split => (delay_samples_r, feedback_r),
            Routing::Stereo | Routing::PingPong => (delay_samples, feedback),
        };
        let read_l = delay_samples + flut;
        let read_r = right_samples - flut;

        let echo_l = self.line_l.read_interp(read_l);
        let echo_r = self.line_r.read_interp(read_r);

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
