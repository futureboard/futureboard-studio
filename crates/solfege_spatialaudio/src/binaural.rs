//! Binaural rendering through a measured head.
//!
//! Each source is heard through the MIT KEMAR dummy head's measured responses
//! ([`crate::hrtf`]), taken apart into the two things the ear listens to:
//!
//! * **Interaural time difference** — how much later the far ear hears the
//!   source, as measured, read from a fractional delay line with cubic
//!   interpolation so it moves continuously.
//! * **The ears' filters** — minimum-phase, diffuse-field-equalised: the head's
//!   shadow on the far ear, and the pinna's notches that tell above from level
//!   and front from behind. Interpolated between the four measured directions
//!   around the source; a moving source crossfades from one block's filters
//!   to the next.
//!
//! Around that sit the room's parts, which is what takes a headphone source
//! out of the head:
//!
//! * **Distance** — beyond the walls' distance the source falls off as 1/r and
//!   loses treble to the air. Inside, it keeps its level; near the centre it
//!   crossfades into the head, where a stereo source is plain stereo.
//! * **Reflections** — the first-order images of the source in the square
//!   room's four walls, each delayed by its extra path, attenuated by its
//!   length and the walls' absorption, and heard from its own direction.
//!
//! Every parameter moves sample by sample from one block's target to the
//! next, so a moving source glides instead of clicking; a still source takes
//! a fast path with one filter per ear.

use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;

use crate::dsp::{DelayLine, PoleZero, PoleZeroState, smoothstep};
use crate::hrtf::{HrirPick, HrirSet};
use crate::position::{RoomPosition, RoomSettings, SourceParams};

/// Radius of the head the reflections' ear delays are worked out for,
/// metres (KEMAR's is close to this).
pub const HEAD_RADIUS_M: f32 = 0.0875;
/// Speed of sound, metres per second.
pub const SPEED_OF_SOUND_M_S: f32 = 343.0;

/// How far spread blurs a source: at full spread, this much of its filters
/// give way to a plain level and its time difference shrinks by as much.
const SPREAD_BLUR: f32 = 0.8;

/// Level of a wall's first reflection at full room amount, before its longer
/// path is accounted for. Early reflections around 10–12 dB under the direct
/// sound are what put a headphone source outside the head; at the default
/// room amount (0.35) each image lands there. (The measured filters carry the
/// direct sound at -3 dB per ear on average; the reflections match.)
const WALL_REFLECTANCE: f32 = 1.4 * std::f32::consts::FRAC_1_SQRT_2;
/// Walls darken what they reflect.
const WALL_LOWPASS_HZ: f32 = 6_000.0;
/// The longest room the delay line is sized for, metres (half-size 10 m).
const MAX_HALF_SIZE_M: f32 = 10.0;
/// The shortest delay the cubic read reaches: every ear is at least this late.
const BASE_DELAY: f32 = 1.0;

/// A reflection, heard from its own direction.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Reflection {
    delay_l: f32,
    delay_r: f32,
    gain_l: f32,
    gain_r: f32,
}

impl Reflection {
    fn lerp(self, to: Self, t: f32) -> Self {
        Self {
            delay_l: lerp(self.delay_l, to.delay_l, t),
            delay_r: lerp(self.delay_r, to.delay_r, t),
            gain_l: lerp(self.gain_l, to.gain_l, t),
            gain_r: lerp(self.gain_r, to.gain_r, t),
        }
    }
}

/// Everything one emitter's rendering depends on, at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PathParams {
    /// Each ear's delay, samples.
    itd_l: f32,
    itd_r: f32,
    /// The measured responses the ears' filters are made of.
    hrir: HrirPick,
    spread: f32,
    /// Direct-path gain (distance, spatialised share).
    direct: f32,
    air: PoleZero,
    /// Share of the source heard inside the head, per ear.
    inside_l: f32,
    inside_r: f32,
    reflections: [Reflection; 4],
}

impl PathParams {
    /// The parameters `t` of the way to `to`. The filters are not
    /// interpolated here: the emitter crossfades them over the block.
    fn lerp(&self, to: &Self, t: f32) -> Self {
        let mut reflections = self.reflections;
        for (r, target) in reflections.iter_mut().zip(to.reflections) {
            *r = r.lerp(target, t);
        }
        Self {
            itd_l: lerp(self.itd_l, to.itd_l, t),
            itd_r: lerp(self.itd_r, to.itd_r, t),
            hrir: to.hrir,
            spread: to.spread,
            direct: lerp(self.direct, to.direct, t),
            air: self.air.lerp(to.air, t),
            inside_l: lerp(self.inside_l, to.inside_l, t),
            inside_r: lerp(self.inside_r, to.inside_r, t),
            reflections,
        }
    }

    fn same_filters(&self, other: &Self) -> bool {
        self.hrir == other.hrir && self.spread == other.spread
    }
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Brown & Duda's ITD for an ear, seconds, offset so the nearer ear's delay is
/// never negative: `0` for a source straight at the ear. Used for the
/// reflections, which are there for space rather than precise direction.
fn ear_delay_seconds(incidence: f32) -> f32 {
    let a_over_c = HEAD_RADIUS_M / SPEED_OF_SOUND_M_S;
    let t = if incidence < FRAC_PI_2 {
        -a_over_c * incidence.cos()
    } else {
        a_over_c * (incidence - FRAC_PI_2)
    };
    t + a_over_c
}

/// Where a reflected image arrives from, and how, for each ear.
fn reflection_for(
    image: (f32, f32, f32),
    direct_distance: f32,
    gain: f32,
    spread: f32,
    sample_rate: f32,
) -> Reflection {
    let (x, y, z) = image;
    let distance = (x * x + y * y + z * z).sqrt().max(1.0e-3);
    let extra = ((distance - direct_distance) / SPEED_OF_SOUND_M_S * sample_rate).max(0.0);
    let dx = x / distance;
    let incidence_r = dx.clamp(-1.0, 1.0).acos();
    let incidence_l = (-dx).clamp(-1.0, 1.0).acos();
    // Broadband level difference in place of the full filters: the
    // reflections are there for space, not for precise direction.
    let level = |incidence: f32| (0.5 * (1.0 + 0.6 * (1.0 - spread) * incidence.cos())).sqrt();
    let attenuation = gain * WALL_REFLECTANCE * (direct_distance.max(0.5) / distance);
    Reflection {
        delay_l: BASE_DELAY + extra + ear_delay_seconds(incidence_l) * sample_rate,
        delay_r: BASE_DELAY + extra + ear_delay_seconds(incidence_r) * sample_rate,
        gain_l: attenuation * level(incidence_l),
        gain_r: attenuation * level(incidence_r),
    }
}

/// The last `taps` samples an ear's filter reads, written twice so they are
/// always one contiguous slice, oldest first.
#[derive(Debug, Clone)]
struct History {
    samples: Vec<f32>,
    pos: usize,
    taps: usize,
}

impl History {
    fn new(taps: usize) -> Self {
        Self {
            samples: vec![0.0; 2 * taps.max(1)],
            pos: 0,
            taps: taps.max(1),
        }
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.pos += 1;
        if self.pos == self.taps {
            self.pos = 0;
        }
        self.samples[self.pos] = x;
        self.samples[self.pos + self.taps] = x;
    }

    #[inline]
    fn window(&self) -> &[f32] {
        &self.samples[self.pos + 1..self.pos + 1 + self.taps]
    }

    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.pos = 0;
    }
}

/// `Σ a·b`, in eight lanes so it vectorises.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut lanes = [0.0f32; 8];
    let mut chunks_a = a.chunks_exact(8);
    let mut chunks_b = b.chunks_exact(8);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
        for i in 0..8 {
            lanes[i] += x[i] * y[i];
        }
    }
    let mut sum = lanes.iter().sum::<f32>();
    for (x, y) in chunks_a.remainder().iter().zip(chunks_b.remainder()) {
        sum += x * y;
    }
    sum
}

/// One point source rendered binaurally: a delay line, and the per-ear
/// filters that read it.
#[derive(Debug, Clone)]
struct Emitter {
    hrirs: Arc<HrirSet>,
    line: DelayLine,
    history_l: History,
    history_r: History,
    /// The ears' filters now, time-reversed; `next_*` is where a moving
    /// source's new filters are built, crossfaded to, and swapped in.
    filter_l: Vec<f32>,
    filter_r: Vec<f32>,
    next_l: Vec<f32>,
    next_r: Vec<f32>,
    air_l: PoleZeroState,
    air_r: PoleZeroState,
    wall_l: PoleZeroState,
    wall_r: PoleZeroState,
    wall: PoleZero,
    current: Option<PathParams>,
    /// Consecutive silent input samples. Once it exceeds the delay line and
    /// the filters, the emitter's output is silent too and processing is
    /// skipped.
    silent_run: usize,
}

impl Emitter {
    fn new(hrirs: Arc<HrirSet>, sample_rate: f32) -> Self {
        let max_extra = (4.0 * MAX_HALF_SIZE_M / SPEED_OF_SOUND_M_S * sample_rate) as usize;
        let max_itd = (3.0 * HEAD_RADIUS_M / SPEED_OF_SOUND_M_S * sample_rate) as usize;
        let taps = hrirs.taps();
        Self {
            hrirs,
            line: DelayLine::new(max_extra + max_itd + 8),
            history_l: History::new(taps),
            history_r: History::new(taps),
            filter_l: vec![0.0; taps],
            filter_r: vec![0.0; taps],
            next_l: vec![0.0; taps],
            next_r: vec![0.0; taps],
            air_l: PoleZeroState::default(),
            air_r: PoleZeroState::default(),
            wall_l: PoleZeroState::default(),
            wall_r: PoleZeroState::default(),
            wall: PoleZero::lowpass(WALL_LOWPASS_HZ, sample_rate),
            current: None,
            silent_run: usize::MAX / 2,
        }
    }

    fn reset(&mut self) {
        self.line.reset();
        self.history_l.reset();
        self.history_r.reset();
        for state in [
            &mut self.air_l,
            &mut self.air_r,
            &mut self.wall_l,
            &mut self.wall_r,
        ] {
            state.reset();
        }
        self.current = None;
        self.silent_run = usize::MAX / 2;
    }

    fn load_filters(&mut self, p: &PathParams) {
        self.hrirs
            .fill_reversed(&p.hrir, p.spread, &mut self.filter_l, &mut self.filter_r);
    }

    /// The direct path, both ears, reading the line at `p`; with `fade`,
    /// that far from the current filters to the next ones.
    #[inline]
    fn direct(&mut self, p: &PathParams, fade: Option<f32>) -> (f32, f32) {
        self.history_l.push(self.line.read_cubic(p.itd_l));
        self.history_r.push(self.line.read_cubic(p.itd_r));
        let (wl, wr) = (self.history_l.window(), self.history_r.window());
        let mut l = dot(wl, &self.filter_l);
        let mut r = dot(wr, &self.filter_r);
        if let Some(t) = fade {
            l += (dot(wl, &self.next_l) - l) * t;
            r += (dot(wr, &self.next_r) - r) * t;
        }
        let l = self.air_l.tick(p.air, l) * p.direct;
        let r = self.air_r.tick(p.air, r) * p.direct;
        // Inside the head: the plain signal, as late as the nearer ear.
        let dry = self
            .line
            .read_linear(BASE_DELAY + self.hrirs.latency() as f32);
        (l + dry * p.inside_l, r + dry * p.inside_r)
    }

    #[inline]
    fn reflections(&mut self, p: &PathParams) -> (f32, f32) {
        let mut l = 0.0;
        let mut r = 0.0;
        for refl in &p.reflections {
            if refl.gain_l.max(refl.gain_r) <= 1.0e-6 {
                continue;
            }
            l += self.line.read_linear(refl.delay_l) * refl.gain_l;
            r += self.line.read_linear(refl.delay_r) * refl.gain_r;
        }
        (
            self.wall_l.tick(self.wall, l),
            self.wall_r.tick(self.wall, r),
        )
    }

    /// Render `input` at `target` and add it to the ears.
    fn process(&mut self, input: &[f32], target: PathParams, out_l: &mut [f32], out_r: &mut [f32]) {
        let frames = input.len().min(out_l.len()).min(out_r.len());
        if frames == 0 {
            return;
        }
        let silent = input[..frames].iter().all(|s| *s == 0.0);
        if silent {
            self.silent_run = self.silent_run.saturating_add(frames);
            let tail = self.line.max_delay() as usize + self.filter_l.len() + 64;
            if self.silent_run > tail {
                // Nothing in the line, nothing in the tails: skip, and hold
                // the target so the next sound starts where the source is.
                if !self.current.is_some_and(|c| c.same_filters(&target)) {
                    self.load_filters(&target);
                }
                self.current = Some(target);
                return;
            }
        } else {
            self.silent_run = 0;
        }
        let from = match self.current {
            Some(from) => from,
            None => {
                self.load_filters(&target);
                target
            }
        };
        let crossfade = !from.same_filters(&target);
        if crossfade {
            self.hrirs.fill_reversed(
                &target.hrir,
                target.spread,
                &mut self.next_l,
                &mut self.next_r,
            );
        }
        let has_reflections = target
            .reflections
            .iter()
            .chain(from.reflections.iter())
            .any(|r| r.gain_l.max(r.gain_r) > 1.0e-6);
        if from == target {
            for n in 0..frames {
                self.line.push(input[n]);
                let (mut l, mut r) = self.direct(&target, None);
                if has_reflections {
                    let (rl, rr) = self.reflections(&target);
                    l += rl;
                    r += rr;
                }
                out_l[n] += l;
                out_r[n] += r;
            }
        } else {
            let inv = 1.0 / frames as f32;
            for n in 0..frames {
                self.line.push(input[n]);
                let t = (n + 1) as f32 * inv;
                let p = from.lerp(&target, t);
                let (mut l, mut r) = self.direct(&p, crossfade.then_some(t));
                if has_reflections {
                    let (rl, rr) = self.reflections(&p);
                    l += rl;
                    r += rr;
                }
                out_l[n] += l;
                out_r[n] += r;
            }
        }
        if crossfade {
            std::mem::swap(&mut self.filter_l, &mut self.next_l);
            std::mem::swap(&mut self.filter_r, &mut self.next_r);
        }
        self.current = Some(target);
    }
}

/// A channel rendered for headphones.
///
/// A stereo channel is two emitters, its sides turned either way of the
/// position by the channel's width; at zero width, one emitter carries the
/// mono sum.
#[derive(Debug, Clone)]
pub struct BinauralSource {
    sample_rate: f32,
    hrirs: Arc<HrirSet>,
    emitters: [Emitter; 2],
    mono: Vec<f32>,
}

impl BinauralSource {
    /// Allocates the delay lines for rooms up to 10 m half-size at
    /// `sample_rate`, and scratch for blocks of up to `max_block`. The
    /// measured set is resampled to `sample_rate` the first time that rate
    /// is asked for, and shared after.
    pub fn new(sample_rate: u32, max_block: usize) -> Self {
        let rate = sample_rate.max(8_000);
        let hrirs = HrirSet::shared(rate);
        let rate = rate as f32;
        Self {
            sample_rate: rate,
            emitters: [
                Emitter::new(hrirs.clone(), rate),
                Emitter::new(hrirs.clone(), rate),
            ],
            hrirs,
            mono: vec![0.0; max_block.max(1)],
        }
    }

    pub fn reset(&mut self) {
        for emitter in &mut self.emitters {
            emitter.reset();
        }
    }

    /// The rendering parameters of a point at `position`. `inside` is how the
    /// source splits between the ears once it is inside the head.
    fn path_params(
        &self,
        position: RoomPosition,
        spread: f32,
        room: &RoomSettings,
        inside: (f32, f32),
        emitter_gain: f32,
    ) -> PathParams {
        let rate = self.sample_rate;
        let half = room.half_size_m;
        let (sx, sy, sz) = (position.x * half, position.y * half, position.z * half);
        let distance = (sx * sx + sy * sy + sz * sz).sqrt();
        let (dx, dy, dz) = if distance < 1.0e-4 {
            (0.0, 1.0, 0.0)
        } else {
            (sx / distance, sy / distance, sz / distance)
        };
        let azimuth = dx.atan2(dy);
        let elevation = dz.clamp(-1.0, 1.0).asin();

        // Into the head as the source reaches the listener.
        let outside = smoothstep(0.05, 0.35, distance / half);
        let spatial = outside * emitter_gain;
        let inside_share = (1.0 - outside) * emitter_gain;

        // The measured head; spread blurs it, and shortens the time between
        // the ears with it.
        let hrir = self.hrirs.pick(azimuth, elevation);
        let blur = SPREAD_BLUR * spread;
        let itd = hrir.itd_s * (1.0 - blur) * rate;

        // Beyond the walls' distance: 1/r and air absorption.
        let beyond = (distance / half).max(1.0);
        let air_cutoff = 20_000.0 / (beyond * beyond);
        let air = if beyond <= 1.0001 {
            PoleZero::IDENTITY
        } else {
            PoleZero::lowpass(air_cutoff, rate)
        };

        let mut reflections = [Reflection::default(); 4];
        if room.reflections > 1.0e-4 {
            let images = [
                (2.0 * half - sx, sy, sz),
                (-2.0 * half - sx, sy, sz),
                (sx, 2.0 * half - sy, sz),
                (sx, -2.0 * half - sy, sz),
            ];
            let gain = room.reflections * emitter_gain / beyond;
            // As late as the filtered direct sound they follow.
            let latency = self.hrirs.latency() as f32;
            for (slot, image) in reflections.iter_mut().zip(images) {
                *slot = reflection_for(image, distance, gain, spread, rate);
                slot.delay_l += latency;
                slot.delay_r += latency;
            }
        }

        PathParams {
            // Positive: the left ear hears it later.
            itd_l: BASE_DELAY + itd.max(0.0),
            itd_r: BASE_DELAY + (-itd).max(0.0),
            hrir,
            spread: blur,
            direct: spatial / beyond,
            air,
            inside_l: inside_share * inside.0,
            inside_r: inside_share * inside.1,
            reflections,
        }
    }

    /// Render one block of a channel's stereo signal at `params` in `room`
    /// and add it to `out_l`/`out_r`.
    pub fn process(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        params: &SourceParams,
        room: &RoomSettings,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let frames = input_l
            .len()
            .min(input_r.len())
            .min(out_l.len())
            .min(out_r.len())
            .min(self.mono.len());
        if frames == 0 {
            return;
        }
        let params = params.sanitized();
        let room = room.sanitized();
        let half_width = params.half_width_radians();
        if half_width <= 1.0e-4 {
            for n in 0..frames {
                self.mono[n] = 0.5 * (input_l[n] + input_r[n]);
            }
            let target = self.path_params(
                params.position,
                params.spread,
                &room,
                (
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                1.0,
            );
            let mono = std::mem::take(&mut self.mono);
            self.emitters[0].process(&mono[..frames], target, out_l, out_r);
            self.mono = mono;
            // The other side is idle: let it run out and go quiet.
            let idle = self.path_params(params.position, params.spread, &room, (0.0, 0.0), 0.0);
            self.feed_silence(1, idle, frames, out_l, out_r);
        } else {
            // Two sides at −3 dB each, so a centred mono signal keeps its level.
            let side_gain = std::f32::consts::FRAC_1_SQRT_2;
            let left = self.path_params(
                params.position.rotated(-half_width),
                params.spread,
                &room,
                (std::f32::consts::SQRT_2, 0.0),
                side_gain,
            );
            let right = self.path_params(
                params.position.rotated(half_width),
                params.spread,
                &room,
                (0.0, std::f32::consts::SQRT_2),
                side_gain,
            );
            self.emitters[0].process(&input_l[..frames], left, out_l, out_r);
            self.emitters[1].process(&input_r[..frames], right, out_l, out_r);
        }
    }

    /// Render a mono signal as one point — a virtual speaker.
    pub(crate) fn process_mono(
        &mut self,
        input: &[f32],
        position: RoomPosition,
        room: &RoomSettings,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let room = room.sanitized();
        let target = self.path_params(
            position.clamped(),
            0.0,
            &room,
            (
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ),
            1.0,
        );
        self.emitters[0].process(input, target, out_l, out_r);
    }

    fn feed_silence(
        &mut self,
        emitter: usize,
        target: PathParams,
        frames: usize,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        // The scratch is only borrowed as zeros here; `mono` holds the other
        // side's input, so use a zero slice of the output length instead.
        const ZEROS: [f32; 256] = [0.0; 256];
        let mut offset = 0;
        while offset < frames {
            let n = (frames - offset).min(ZEROS.len());
            self.emitters[emitter].process(
                &ZEROS[..n],
                target,
                &mut out_l[offset..offset + n],
                &mut out_r[offset..offset + n],
            );
            offset += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Render `seconds` of noise at `params` and return the ears.
    fn render(params: SourceParams, room: RoomSettings) -> (Vec<f32>, Vec<f32>) {
        let rate = 48_000;
        let block = 512;
        let mut source = BinauralSource::new(rate, block);
        let mut seed = 0x1234_5678u32;
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();
        for _ in 0..40 {
            let mut noise = vec![0.0f32; block];
            for s in noise.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *s = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            }
            let mut l = vec![0.0f32; block];
            let mut r = vec![0.0f32; block];
            source.process(&noise, &noise, &params, &room, &mut l, &mut r);
            out_l.extend_from_slice(&l);
            out_r.extend_from_slice(&r);
        }
        (out_l, out_r)
    }

    fn at(x: f32, y: f32) -> SourceParams {
        SourceParams {
            position: RoomPosition::new(x, y, 0.0),
            width: 0.0,
            ..SourceParams::default()
        }
    }

    fn dry() -> RoomSettings {
        RoomSettings {
            reflections: 0.0,
            ..RoomSettings::default()
        }
    }

    /// Lag (samples) of `b` behind `a` that maximises their correlation.
    fn lag(a: &[f32], b: &[f32], max: i32) -> i32 {
        let mut best = (0, f32::MIN);
        for d in -max..=max {
            let mut sum = 0.0;
            for n in 1_000..a.len() - 1_000 {
                let m = n as i32 + d;
                sum += a[n] * b[m as usize];
            }
            if sum > best.1 {
                best = (d, sum);
            }
        }
        best.0
    }

    #[test]
    fn a_source_on_the_right_is_louder_and_earlier_in_the_right_ear() {
        let (l, r) = render(at(1.0, 0.0), dry());
        assert!(rms(&r) > rms(&l) * 1.4, "ILD: l {} r {}", rms(&l), rms(&r));
        // The left ear hears it later: about a/c·(1 + π/2)·48 kHz ≈ 31 samples.
        let d = lag(&r, &l, 48);
        assert!((25..=36).contains(&d), "ITD lag {d}");
    }

    #[test]
    fn a_source_in_front_reaches_both_ears_alike() {
        let (l, r) = render(at(0.0, 1.0), dry());
        assert!((rms(&l) - rms(&r)).abs() < 1.0e-3 * rms(&l).max(1.0e-6));
        assert_eq!(lag(&l, &r, 10), 0);
    }

    #[test]
    fn left_and_right_mirror_each_other() {
        let (ll, lr) = render(at(-0.8, 0.3), dry());
        let (rl, rr) = render(at(0.8, 0.3), dry());
        assert!((rms(&ll) - rms(&rr)).abs() < 1.0e-3);
        assert!((rms(&lr) - rms(&rl)).abs() < 1.0e-3);
    }

    #[test]
    fn the_centre_of_the_room_is_inside_the_head() {
        // A stereo source at the centre is plain stereo: left to left.
        let params = SourceParams {
            position: RoomPosition::CENTRE,
            width: 1.0,
            ..SourceParams::default()
        };
        let mut source = BinauralSource::new(48_000, 256);
        let silence = vec![0.0f32; 256];
        let tone: Vec<f32> = (0..256).map(|n| (n as f32 * 0.1).sin()).collect();
        let mut l = vec![0.0f32; 256];
        let mut r = vec![0.0f32; 256];
        source.process(&tone, &silence, &params, &dry(), &mut l, &mut r);
        assert!(
            rms(&l) > 0.5 && rms(&r) < 1.0e-4,
            "l {} r {}",
            rms(&l),
            rms(&r)
        );
    }

    #[test]
    fn reflections_add_energy_after_the_direct_sound() {
        let (dl, _) = render(at(0.3, 0.9), dry());
        let (wl, _) = render(at(0.3, 0.9), RoomSettings::default());
        assert!(
            rms(&wl) > rms(&dl) * 1.02,
            "dry {} wet {}",
            rms(&dl),
            rms(&wl)
        );
    }

    #[test]
    fn nothing_blows_up_while_moving_everywhere() {
        let rate = 48_000;
        let block = 128;
        let mut source = BinauralSource::new(rate, block);
        let room = RoomSettings::default();
        let input: Vec<f32> = (0..block).map(|n| ((n as f32) * 0.37).sin()).collect();
        let mut prev_end = 0.0f32;
        for step in 0..600 {
            let angle = step as f32 * 0.05;
            let params = SourceParams {
                position: RoomPosition::new(
                    angle.sin(),
                    angle.cos() * 0.9,
                    (step % 7) as f32 / 7.0,
                ),
                spread: (step % 5) as f32 / 5.0,
                width: (step % 3) as f32 / 2.0,
                lfe: 0.0,
            };
            let mut l = vec![0.0f32; block];
            let mut r = vec![0.0f32; block];
            source.process(&input, &input, &params, &room, &mut l, &mut r);
            assert!(
                l.iter()
                    .chain(r.iter())
                    .all(|s| s.is_finite() && s.abs() < 8.0)
            );
            // No click at the block edge.
            assert!((l[0] - prev_end).abs() < 1.5, "jump at block {step}");
            prev_end = l[block - 1];
        }
    }

    /// Band levels (dB) of `x`: <1k, 1-2.5k, 2.5-6k, 6-12k, via a crude DFT.
    fn bands(x: &[f32]) -> [f32; 4] {
        let rate = 48_000.0;
        let n = 4096;
        let mut out = [0.0f32; 4];
        let edges = [
            (100.0, 1000.0),
            (1000.0, 2500.0),
            (2500.0, 6000.0),
            (6000.0, 12000.0),
        ];
        let frames = (x.len() - 4096) / n;
        for f in 0..frames {
            let seg = &x[4096 + f * n..4096 + (f + 1) * n];
            for (b, (lo, hi)) in edges.iter().enumerate() {
                let mut k = (lo / rate * n as f32) as usize;
                while (k as f32) < hi / rate * n as f32 {
                    let (mut re, mut im) = (0.0f32, 0.0f32);
                    for (i, s) in seg.iter().enumerate() {
                        let w = 2.0 * std::f32::consts::PI * (k * i) as f32 / n as f32;
                        re += s * w.cos();
                        im += s * w.sin();
                    }
                    out[b] += re * re + im * im;
                    k += 7;
                }
            }
        }
        out.map(|e| 10.0 * e.max(1e-20).log10())
    }

    #[test]
    fn behind_is_clearly_darker_than_in_front() {
        // A stereo track straight ahead and straight behind: the same time
        // and level at the two ears, so the tone is all that tells them
        // apart — and it has to be plain, not a dB or two.
        let p = |y: f32| SourceParams {
            position: RoomPosition::new(0.0, y, 0.0),
            width: 1.0,
            ..SourceParams::default()
        };
        let (front, _) = render(p(1.0), dry());
        let (back, _) = render(p(-1.0), dry());
        let (f, b) = (bands(&front), bands(&back));
        assert!((b[0] - f[0]).abs() < 1.5, "low end moved: {f:?} {b:?}");
        assert!(b[2] - f[2] < -6.0, "2.5-6 kHz: {f:?} {b:?}");
        assert!(b[3] - f[3] < -6.0, "6-12 kHz: {f:?} {b:?}");
    }

    /// `cargo test -p solfege_spatialaudio --release -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, not correctness"]
    fn cost_of_a_moving_stereo_source() {
        let rate = 48_000;
        let block = 256;
        let mut source = BinauralSource::new(rate, block);
        let input: Vec<f32> = (0..block).map(|n| ((n as f32) * 0.37).sin()).collect();
        let (mut l, mut r) = (vec![0.0f32; block], vec![0.0f32; block]);
        let blocks = rate as usize * 10 / block;
        for moving in [false, true] {
            let start = std::time::Instant::now();
            for step in 0..blocks {
                let angle = if moving { step as f32 * 0.01 } else { 0.7 };
                let params = SourceParams {
                    position: RoomPosition::new(angle.sin(), angle.cos(), 0.2),
                    ..SourceParams::default()
                };
                source.process(
                    &input,
                    &input,
                    &params,
                    &RoomSettings::default(),
                    &mut l,
                    &mut r,
                );
            }
            let seconds = start.elapsed().as_secs_f64();
            println!(
                "moving {moving}: {:.2}% of one core for 10 s of audio",
                seconds / 10.0 * 100.0
            );
        }
    }

    #[test]
    fn a_silent_source_costs_nothing_and_stays_silent() {
        let mut source = BinauralSource::new(48_000, 256);
        let zeros = vec![0.0f32; 256];
        let mut l = vec![0.0f32; 256];
        let mut r = vec![0.0f32; 256];
        for _ in 0..200 {
            source.process(
                &zeros,
                &zeros,
                &at(0.5, 0.5),
                &RoomSettings::default(),
                &mut l,
                &mut r,
            );
        }
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.0));
    }
}
