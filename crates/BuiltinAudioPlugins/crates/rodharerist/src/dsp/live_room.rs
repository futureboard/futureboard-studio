//! Live Room — a modulated figure-8 tank, not another voicing of the comb bank.
//!
//! The other reverb models retune one Schroeder/Freeverb: parallel combs into
//! series allpasses. This one follows Dattorro's plate topology instead. Input
//! allpasses diffuse each side, then a cross-fed pair of delay lines (with a
//! slow modulation on the first allpass of each side and a one-pole damper in
//! the loop) builds the tail. A short tap cluster in front of that tank is the
//! early room. Decay maps to the loop gain; mix stays with the shared reverb
//! knob. Buffers are allocated in [`LiveRoom::new`] / [`LiveRoom::set_sample_rate`].
//! [`LiveRoom::process`] does not allocate.

use super::flush_denormal;
use super::smooth::Smoothed;

/// Glide for the decay knob. Matches the comb bank's control glide.
const SMOOTH_SECONDS: f32 = 0.010;
/// Dattorro's published tunings are in samples at this rate.
const REF_RATE: f32 = 29_761.0;
/// One-pole damper in each tank half. Higher is darker. Fixed: the Decay knob
/// already lengthens the tail, and a longer tail loses highs on its own.
const DAMP: f32 = 0.42;
/// Input highpass, so a distorted guitar does not turn the room to mud.
const INPUT_HP_HZ: f32 = 140.0;
/// How long the dry pick leads the tank.
const PREDELAY_S: f32 = 0.012;
const PREDELAY_LINE_S: f32 = 0.030;
/// Signed tank taps, scaled so a full-mix room sits with the comb-bank models
/// instead of drowning them. The taps alternate sign, so this is not a sum of
/// seven copies.
const TAP_GAIN: f32 = 0.08;
const EARLY_GAIN: f32 = 0.36;
const INPUT_GAIN: f32 = 0.85;

const IN_AP_L: [f32; 4] = [142.0, 107.0, 379.0, 277.0];
const IN_AP_R: [f32; 4] = [165.0, 130.0, 401.0, 299.0];
const IN_AP_G: [f32; 4] = [0.75, 0.75, 0.625, 0.625];
const MOD_L: f32 = 672.0;
const MOD_R: f32 = 908.0;
const MOD_EXCURSION: f32 = 16.0;
const DELAY_L1: f32 = 4_453.0;
const DELAY_L2: f32 = 3_720.0;
const DELAY_R1: f32 = 4_217.0;
const DELAY_R2: f32 = 3_163.0;
const AP_L: f32 = 1_800.0;
const AP_R: f32 = 2_656.0;

#[derive(Debug, Clone)]
struct Line {
    data: Vec<f32>,
    write: usize,
}

impl Line {
    fn new(len: usize) -> Self {
        Self {
            data: vec![0.0; len.max(2)],
            write: 0,
        }
    }

    fn clear(&mut self) {
        self.data.fill(0.0);
        self.write = 0;
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.data[self.write] = flush_denormal(x);
        self.write += 1;
        if self.write >= self.data.len() {
            self.write = 0;
        }
    }

    /// `delay` samples behind the write head, linear interpolation.
    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let len = self.data.len();
        let d = delay.clamp(1.0, (len - 2) as f32);
        let base = d.floor();
        let frac = d - base;
        let i0 = (self.write + len - base as usize) % len;
        let i1 = (i0 + len - 1) % len;
        self.data[i0] * (1.0 - frac) + self.data[i1] * frac
    }
}

#[inline]
fn allpass(line: &mut Line, x: f32, gain: f32, delay: f32) -> f32 {
    let buffered = line.read(delay);
    let out = buffered - gain * x;
    line.push(x + gain * buffered);
    flush_denormal(out)
}

fn at(samples: f32, sample_rate: f32) -> f32 {
    (samples * sample_rate / REF_RATE).max(2.0)
}

/// Loop gain for one tank half. Applied twice per round trip (left, then
/// right), so the round-trip time — not one half — is what Decay asks for.
fn decay_gain(decay_s: f32) -> f32 {
    let cycle =
        (MOD_L + DELAY_L1 + AP_L + DELAY_L2 + MOD_R + DELAY_R1 + AP_R + DELAY_R2) / REF_RATE;
    let rt = decay_s.clamp(0.5, 15.0);
    10f32.powf(-1.5 * cycle / rt).clamp(0.02, 0.93)
}

#[derive(Debug, Clone)]
pub(super) struct LiveRoom {
    sample_rate: f32,
    pre_l: Line,
    pre_r: Line,
    in_l: [Line; 4],
    in_r: [Line; 4],
    in_len_l: [f32; 4],
    in_len_r: [f32; 4],
    mod_l: Line,
    mod_r: Line,
    mod_base_l: f32,
    mod_base_r: f32,
    mod_depth: f32,
    delay_l1: Line,
    delay_l2: Line,
    delay_r1: Line,
    delay_r2: Line,
    len_l1: f32,
    len_l2: f32,
    len_r1: f32,
    len_r2: f32,
    ap_l: Line,
    ap_r: Line,
    ap_len_l: f32,
    ap_len_r: f32,
    damp_l: f32,
    damp_r: f32,
    tank_l: f32,
    tank_r: f32,
    hp_x_l: f32,
    hp_y_l: f32,
    hp_x_r: f32,
    hp_y_r: f32,
    hp_pole: f32,
    phase: f32,
    phase_inc: f32,
    predelay: f32,
    feedback: Smoothed,
}

impl LiveRoom {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let mut room = Self {
            sample_rate: sr,
            pre_l: Line::new(2),
            pre_r: Line::new(2),
            in_l: [Line::new(2), Line::new(2), Line::new(2), Line::new(2)],
            in_r: [Line::new(2), Line::new(2), Line::new(2), Line::new(2)],
            in_len_l: [2.0; 4],
            in_len_r: [2.0; 4],
            mod_l: Line::new(2),
            mod_r: Line::new(2),
            mod_base_l: 2.0,
            mod_base_r: 2.0,
            mod_depth: 0.0,
            delay_l1: Line::new(2),
            delay_l2: Line::new(2),
            delay_r1: Line::new(2),
            delay_r2: Line::new(2),
            len_l1: 2.0,
            len_l2: 2.0,
            len_r1: 2.0,
            len_r2: 2.0,
            ap_l: Line::new(2),
            ap_r: Line::new(2),
            ap_len_l: 2.0,
            ap_len_r: 2.0,
            damp_l: 0.0,
            damp_r: 0.0,
            tank_l: 0.0,
            tank_r: 0.0,
            hp_x_l: 0.0,
            hp_y_l: 0.0,
            hp_x_r: 0.0,
            hp_y_r: 0.0,
            hp_pole: 0.0,
            phase: 0.0,
            phase_inc: 0.0,
            predelay: 2.0,
            feedback: Smoothed::new(sr, SMOOTH_SECONDS, decay_gain(8.5)),
        };
        room.rebuild();
        room
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.feedback.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.rebuild();
    }

    /// Drop the tail. The decay glide stays where `configure` put it.
    pub(super) fn reset_audio(&mut self) {
        self.pre_l.clear();
        self.pre_r.clear();
        for line in self.in_l.iter_mut().chain(self.in_r.iter_mut()) {
            line.clear();
        }
        self.mod_l.clear();
        self.mod_r.clear();
        self.delay_l1.clear();
        self.delay_l2.clear();
        self.delay_r1.clear();
        self.delay_r2.clear();
        self.ap_l.clear();
        self.ap_r.clear();
        self.damp_l = 0.0;
        self.damp_r = 0.0;
        self.tank_l = 0.0;
        self.tank_r = 0.0;
        self.hp_x_l = 0.0;
        self.hp_y_l = 0.0;
        self.hp_x_r = 0.0;
        self.hp_y_r = 0.0;
        self.phase = 0.0;
        self.feedback.snap();
    }

    pub(super) fn configure(&mut self, decay_s: f32) {
        self.feedback.set_target(decay_gain(decay_s));
    }

    fn rebuild(&mut self) {
        let sr = self.sample_rate;
        let line = |samples: f32| Line::new(samples.ceil() as usize + 8);
        self.predelay = (sr * PREDELAY_S).max(2.0);
        self.pre_l = line(sr * PREDELAY_LINE_S);
        self.pre_r = line(sr * PREDELAY_LINE_S);
        for i in 0..4 {
            self.in_len_l[i] = at(IN_AP_L[i], sr);
            self.in_len_r[i] = at(IN_AP_R[i], sr);
            self.in_l[i] = line(self.in_len_l[i]);
            self.in_r[i] = line(self.in_len_r[i]);
        }
        self.mod_depth = at(MOD_EXCURSION, sr);
        self.mod_base_l = at(MOD_L, sr);
        self.mod_base_r = at(MOD_R, sr);
        self.mod_l = line(self.mod_base_l + self.mod_depth);
        self.mod_r = line(self.mod_base_r + self.mod_depth);
        self.len_l1 = at(DELAY_L1, sr);
        self.len_l2 = at(DELAY_L2, sr);
        self.len_r1 = at(DELAY_R1, sr);
        self.len_r2 = at(DELAY_R2, sr);
        self.delay_l1 = line(self.len_l1);
        self.delay_l2 = line(self.len_l2);
        self.delay_r1 = line(self.len_r1);
        self.delay_r2 = line(self.len_r2);
        self.ap_len_l = at(AP_L, sr);
        self.ap_len_r = at(AP_R, sr);
        self.ap_l = line(self.ap_len_l);
        self.ap_r = line(self.ap_len_r);
        self.hp_pole = (-std::f32::consts::TAU * INPUT_HP_HZ / sr).exp();
        // About 0.8 Hz. Slow enough that a sustained note does not warble,
        // fast enough that the tank modes do not ring like a naked comb.
        self.phase_inc = std::f32::consts::TAU * 0.8 / sr;
    }

    #[inline]
    fn hipass(x1: &mut f32, y1: &mut f32, pole: f32, x: f32) -> f32 {
        let y = x - *x1 + pole * *y1;
        *x1 = x;
        *y1 = flush_denormal(y);
        *y1
    }

    /// Wet room only. The shared mix knob is applied by the caller.
    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let decay = self.feedback.tick().clamp(0.02, 0.93);
        let lfo = self.phase.sin();
        self.phase += self.phase_inc;
        if self.phase > std::f32::consts::TAU {
            self.phase -= std::f32::consts::TAU;
        }
        let mod_l = self.mod_base_l + self.mod_depth * lfo;
        let mod_r = self.mod_base_r - self.mod_depth * lfo;

        let hp_l = Self::hipass(&mut self.hp_x_l, &mut self.hp_y_l, self.hp_pole, left);
        let hp_r = Self::hipass(&mut self.hp_x_r, &mut self.hp_y_r, self.hp_pole, right);
        // A little crossfeed, not a mono sum: a hard-panned or anti-phase
        // guitar still enters the room.
        let mid = (hp_l + hp_r) * 0.5;
        let in_l = (hp_l * 0.78 + mid * 0.22) * INPUT_GAIN;
        let in_r = (hp_r * 0.78 + mid * 0.22) * INPUT_GAIN;

        self.pre_l.push(in_l);
        self.pre_r.push(in_r);
        let sr = self.sample_rate;
        let early_l = self.pre_l.read(sr * 0.0046) * 0.42 + self.pre_r.read(sr * 0.0084) * 0.27
            - self.pre_l.read(sr * 0.0128) * 0.16
            + self.pre_r.read(sr * 0.0186) * 0.10;
        let early_r = self.pre_r.read(sr * 0.0051) * 0.42 + self.pre_l.read(sr * 0.0091) * 0.27
            - self.pre_r.read(sr * 0.0136) * 0.16
            + self.pre_l.read(sr * 0.0194) * 0.10;

        let mut body_l = self.pre_l.read(self.predelay);
        let mut body_r = self.pre_r.read(self.predelay);
        for i in 0..4 {
            body_l = allpass(&mut self.in_l[i], body_l, IN_AP_G[i], self.in_len_l[i]);
            body_r = allpass(&mut self.in_r[i], body_r, IN_AP_G[i], self.in_len_r[i]);
        }

        let left_in = body_l + self.tank_r;
        let (end_l, taps_l) = self.tank_left(left_in, mod_l, decay);
        self.tank_l = end_l;
        let right_in = body_r + self.tank_l;
        let (end_r, taps_r) = self.tank_right(right_in, mod_r, decay);
        self.tank_r = end_r;

        let wet_l = flush_denormal(taps_l * TAP_GAIN + early_l * EARLY_GAIN);
        let wet_r = flush_denormal(taps_r * TAP_GAIN + early_r * EARLY_GAIN);
        (wet_l, wet_r)
    }

    /// Left half of the figure-8. Returns the loop end (fed to the right half)
    /// and a signed tap sum for the left output.
    #[inline]
    fn tank_left(&mut self, input: f32, mod_delay: f32, decay: f32) -> (f32, f32) {
        let mut x = allpass(&mut self.mod_l, input, 0.70, mod_delay);
        let t_a = self.delay_l1.read(self.len_l1 * 0.08);
        let t_b = self.delay_l1.read(self.len_l1 * 0.63);
        self.delay_l1.push(x);
        x = self.delay_l1.read(self.len_l1);
        self.damp_l = flush_denormal(x * (1.0 - DAMP) + self.damp_l * DAMP);
        x = self.damp_l * decay;
        let t_ap = self.ap_l.read(self.ap_len_l * 0.42);
        x = allpass(&mut self.ap_l, x, 0.50, self.ap_len_l);
        let t_c = self.delay_l2.read(self.len_l2 * 0.51);
        self.delay_l2.push(x);
        let end = self.delay_l2.read(self.len_l2);
        // Right-half taps, read from the lines as they stand. The right half
        // runs after this, so these are the previous sample's right tank —
        // the same one-sample skew the cross feedback already has.
        let r_a = self.delay_r1.read(self.len_r1 * 0.47);
        let r_ap = self.ap_r.read(self.ap_len_r * 0.18);
        let r_b = self.delay_r2.read(self.len_r2 * 0.33);
        let taps = t_a + t_b - t_ap + t_c - r_a - r_ap - r_b;
        (flush_denormal(end), taps)
    }

    #[inline]
    fn tank_right(&mut self, input: f32, mod_delay: f32, decay: f32) -> (f32, f32) {
        let mut x = allpass(&mut self.mod_r, input, 0.70, mod_delay);
        let t_a = self.delay_r1.read(self.len_r1 * 0.09);
        let t_b = self.delay_r1.read(self.len_r1 * 0.71);
        self.delay_r1.push(x);
        x = self.delay_r1.read(self.len_r1);
        self.damp_r = flush_denormal(x * (1.0 - DAMP) + self.damp_r * DAMP);
        x = self.damp_r * decay;
        let t_ap = self.ap_r.read(self.ap_len_r * 0.46);
        x = allpass(&mut self.ap_r, x, 0.50, self.ap_len_r);
        let t_c = self.delay_r2.read(self.len_r2 * 0.58);
        self.delay_r2.push(x);
        let end = self.delay_r2.read(self.len_r2);
        let l_a = self.delay_l1.read(self.len_l1 * 0.41);
        let l_ap = self.ap_l.read(self.ap_len_l * 0.22);
        let l_b = self.delay_l2.read(self.len_l2 * 0.29);
        let taps = t_a + t_b - t_ap + t_c - l_a - l_ap - l_b;
        (flush_denormal(end), taps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(decay_s: f32, seconds: f32) -> (f32, f32) {
        let sr = 48_000.0;
        let mut room = LiveRoom::new(sr);
        room.configure(decay_s);
        room.reset_audio();
        let mut peak = 0.0f32;
        let mut tail = 0.0f32;
        let total = (sr * seconds) as usize;
        let excite = (sr * 0.25) as usize;
        let tail_from = (sr * (seconds - 0.20)) as usize;
        for n in 0..total {
            let x = if n < excite {
                (n as f32 * 196.0 * std::f32::consts::TAU / sr).sin() * 0.5
            } else {
                0.0
            };
            let (l, r) = room.process(x, x);
            assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
            peak = peak.max(l.abs()).max(r.abs());
            if n >= tail_from {
                tail = tail.max(l.abs()).max(r.abs());
            }
        }
        (peak, tail)
    }

    #[test]
    fn live_room_is_audible_bounded_and_decay_does_something() {
        let (short_peak, short_tail) = render(0.6, 1.6);
        let (long_peak, long_tail) = render(8.0, 1.6);
        assert!(short_peak > 0.02, "short room silent: {short_peak}");
        assert!(long_peak < 2.5, "room too hot: {long_peak}");
        assert!(
            long_tail > short_tail * 2.0,
            "decay did not lengthen the tail: short {short_tail} long {long_tail}"
        );
    }

    #[test]
    fn anti_phase_still_enters_the_room() {
        let sr = 48_000.0;
        let mut room = LiveRoom::new(sr);
        room.configure(3.0);
        room.reset_audio();
        let mut energy = 0.0f32;
        for n in 0..(sr as usize / 4) {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (l, r) = room.process(x, -x);
            energy += l * l + r * r;
        }
        assert!(energy > 1.0e-3, "anti-phase was summed away: {energy}");
    }
}
