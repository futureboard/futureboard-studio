//! Binaural rendering with a structural head model.
//!
//! No measured HRIR set: each source is rendered through the structural model
//! of Brown & Duda ("A structural model for binaural sound synthesis", IEEE
//! Trans. Speech and Audio Processing, 1998), which builds the head-related
//! response from the parts that make it:
//!
//! * **Interaural time difference** — a spherical head of radius
//!   [`HEAD_RADIUS_M`]: each ear hears the source late by the path around the
//!   head (Woodworth), read from a fractional delay line with cubic
//!   interpolation.
//! * **Head shadow** — per ear, the one-pole/one-zero filter
//!   `H(s) = (1 + α s / 2ω₀) / (1 + s / 2ω₀)`, `ω₀ = c/a`, with `α` running
//!   from 2 (+6 dB of treble, the ear facing the source) to 0.1 (−20 dB, the
//!   ear in the head's shadow) with the angle of incidence.
//! * **Pinna** — five short echoes whose delays move with azimuth and
//!   elevation: the notches that tell above from level and front from back.
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
//! a fast path with no interpolation at all.

use std::f32::consts::FRAC_PI_2;

use crate::dsp::{DelayLine, PoleZero, PoleZeroState, smoothstep};
use crate::position::{RoomPosition, RoomSettings, SourceParams};

/// Radius of the model head, metres (Brown & Duda's 8.75 cm).
pub const HEAD_RADIUS_M: f32 = 0.0875;
/// Speed of sound, metres per second.
pub const SPEED_OF_SOUND_M_S: f32 = 343.0;

/// `α` at the shadowed ear, and the incidence angle it is reached at.
const ALPHA_MIN: f32 = 0.1;
const THETA_MIN_DEG: f32 = 150.0;

/// Brown & Duda, Table 1: reflection coefficients and the delay law's
/// constants (samples at 44.1 kHz).
const PINNA_RHO: [f32; 5] = [0.5, -1.0, 0.5, -0.25, 0.25];
const PINNA_A: [f32; 5] = [1.0, 5.0, 5.0, 5.0, 5.0];
const PINNA_B: [f32; 5] = [2.0, 4.0, 7.0, 11.0, 13.0];
const PINNA_D: [f32; 5] = [1.0, 0.5, 0.5, 0.5, 0.5];
/// How much of the pinna's echoes to add. The model's full depth carves
/// notches deep enough to colour a mix; half keeps the cues and the tone.
const PINNA_DEPTH: f32 = 0.5;

/// Level of a wall's first reflection at full room amount, before its longer
/// path is accounted for. Early reflections around 10–12 dB under the direct
/// sound are what put a headphone source outside the head; at the default
/// room amount (0.35) each image lands there.
const WALL_REFLECTANCE: f32 = 1.4;
/// Walls darken what they reflect.
const WALL_LOWPASS_HZ: f32 = 6_000.0;
/// The longest room the delay line is sized for, metres (half-size 10 m).
const MAX_HALF_SIZE_M: f32 = 10.0;

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
    itd_l: f32,
    itd_r: f32,
    shadow_l: PoleZero,
    shadow_r: PoleZero,
    pinna_tau: [f32; 5],
    /// Direct-path gain (distance, spatialised share).
    direct: f32,
    air: PoleZero,
    /// Share of the source heard inside the head, per ear.
    inside_l: f32,
    inside_r: f32,
    reflections: [Reflection; 4],
}

impl PathParams {
    fn lerp(&self, to: &Self, t: f32) -> Self {
        let mut pinna_tau = self.pinna_tau;
        for (tau, target) in pinna_tau.iter_mut().zip(to.pinna_tau) {
            *tau = lerp(*tau, target, t);
        }
        let mut reflections = self.reflections;
        for (r, target) in reflections.iter_mut().zip(to.reflections) {
            *r = r.lerp(target, t);
        }
        Self {
            itd_l: lerp(self.itd_l, to.itd_l, t),
            itd_r: lerp(self.itd_r, to.itd_r, t),
            shadow_l: self.shadow_l.lerp(to.shadow_l, t),
            shadow_r: self.shadow_r.lerp(to.shadow_r, t),
            pinna_tau,
            direct: lerp(self.direct, to.direct, t),
            air: self.air.lerp(to.air, t),
            inside_l: lerp(self.inside_l, to.inside_l, t),
            inside_r: lerp(self.inside_r, to.inside_r, t),
            reflections,
        }
    }
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Unit direction `(x right, y front, z up)` of azimuth/elevation (radians).
fn direction(azimuth: f32, elevation: f32) -> (f32, f32, f32) {
    let (sa, ca) = azimuth.sin_cos();
    let (se, ce) = elevation.sin_cos();
    (sa * ce, ca * ce, se)
}

/// Brown & Duda's ITD for an ear, seconds, offset so the nearer ear's delay is
/// never negative: `0` for a source straight at the ear.
fn ear_delay_seconds(incidence: f32) -> f32 {
    let a_over_c = HEAD_RADIUS_M / SPEED_OF_SOUND_M_S;
    let t = if incidence < FRAC_PI_2 {
        -a_over_c * incidence.cos()
    } else {
        a_over_c * (incidence - FRAC_PI_2)
    };
    t + a_over_c
}

/// The head-shadow filter for an ear at `incidence` (radians between the
/// source and the ear's axis), bilinear-transformed at `sample_rate`.
fn head_shadow(incidence: f32, spread: f32, sample_rate: f32) -> PoleZero {
    let theta_deg = incidence.to_degrees();
    let mut alpha = (1.0 + ALPHA_MIN / 2.0)
        + (1.0 - ALPHA_MIN / 2.0) * (theta_deg / THETA_MIN_DEG * 180.0).to_radians().cos();
    // Spread softens the shadow toward an unshaded ear.
    alpha = lerp(alpha, 1.0, spread * 0.8);
    let omega0 = SPEED_OF_SOUND_M_S / HEAD_RADIUS_M;
    let k = 2.0 * sample_rate;
    let two_w0 = 2.0 * omega0;
    let norm = two_w0 + k;
    PoleZero {
        b0: (two_w0 + alpha * k) / norm,
        b1: (two_w0 - alpha * k) / norm,
        a1: (two_w0 - k) / norm,
    }
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
    // Broadband level difference in place of the full shadow filter: the
    // reflections are there for space, not for precise direction.
    let level = |incidence: f32| (0.5 * (1.0 + 0.6 * (1.0 - spread) * incidence.cos())).sqrt();
    let attenuation = gain * WALL_REFLECTANCE * (direct_distance.max(0.5) / distance);
    Reflection {
        delay_l: extra + ear_delay_seconds(incidence_l) * sample_rate,
        delay_r: extra + ear_delay_seconds(incidence_r) * sample_rate,
        gain_l: attenuation * level(incidence_l),
        gain_r: attenuation * level(incidence_r),
    }
}

/// One point source rendered binaurally: a delay line, and the per-ear
/// filters that read it.
#[derive(Debug, Clone)]
struct Emitter {
    line: DelayLine,
    shadow_l: PoleZeroState,
    shadow_r: PoleZeroState,
    air_l: PoleZeroState,
    air_r: PoleZeroState,
    wall_l: PoleZeroState,
    wall_r: PoleZeroState,
    wall: PoleZero,
    current: Option<PathParams>,
    /// Consecutive silent input samples. Once it exceeds the delay line, the
    /// emitter's output is silent too and processing is skipped.
    silent_run: usize,
}

impl Emitter {
    fn new(sample_rate: f32) -> Self {
        let max_extra = (4.0 * MAX_HALF_SIZE_M / SPEED_OF_SOUND_M_S * sample_rate) as usize;
        let max_itd = (3.0 * HEAD_RADIUS_M / SPEED_OF_SOUND_M_S * sample_rate) as usize;
        let max_pinna = (20.0 * sample_rate / 44_100.0) as usize;
        Self {
            line: DelayLine::new(max_extra + max_itd + max_pinna + 8),
            shadow_l: PoleZeroState::default(),
            shadow_r: PoleZeroState::default(),
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
        for state in [
            &mut self.shadow_l,
            &mut self.shadow_r,
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

    /// The direct path, both ears, reading the line at `p`.
    #[inline]
    fn direct(&mut self, p: &PathParams) -> (f32, f32) {
        let read_ear = |line: &DelayLine, itd: f32| {
            let mut s = line.read_cubic(itd);
            for k in 0..5 {
                s += PINNA_DEPTH * PINNA_RHO[k] * line.read_linear(itd + p.pinna_tau[k]);
            }
            s
        };
        let l = read_ear(&self.line, p.itd_l);
        let r = read_ear(&self.line, p.itd_r);
        let l = self.air_l.tick(p.air, self.shadow_l.tick(p.shadow_l, l)) * p.direct;
        let r = self.air_r.tick(p.air, self.shadow_r.tick(p.shadow_r, r)) * p.direct;
        let dry = self.line.read_linear(0.0);
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
            if self.silent_run > self.line.max_delay() as usize + 64 {
                // Nothing in the line, nothing in the tails: skip, and hold
                // the target so the next sound starts where the source is.
                self.current = Some(target);
                return;
            }
        } else {
            self.silent_run = 0;
        }
        let from = self.current.unwrap_or(target);
        let has_reflections = target
            .reflections
            .iter()
            .chain(from.reflections.iter())
            .any(|r| r.gain_l.max(r.gain_r) > 1.0e-6);
        if from == target {
            for n in 0..frames {
                self.line.push(input[n]);
                let (mut l, mut r) = self.direct(&target);
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
                let p = from.lerp(&target, (n + 1) as f32 * inv);
                let (mut l, mut r) = self.direct(&p);
                if has_reflections {
                    let (rl, rr) = self.reflections(&p);
                    l += rl;
                    r += rr;
                }
                out_l[n] += l;
                out_r[n] += r;
            }
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
    emitters: [Emitter; 2],
    mono: Vec<f32>,
}

impl BinauralSource {
    /// Allocates the delay lines for rooms up to 10 m half-size at
    /// `sample_rate`, and scratch for blocks of up to `max_block`.
    pub fn new(sample_rate: u32, max_block: usize) -> Self {
        let rate = sample_rate.max(8_000) as f32;
        Self {
            sample_rate: rate,
            emitters: [Emitter::new(rate), Emitter::new(rate)],
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
        let (ux, _, _) = direction(azimuth, elevation);
        let incidence_r = ux.clamp(-1.0, 1.0).acos();
        let incidence_l = (-ux).clamp(-1.0, 1.0).acos();

        // Into the head as the source reaches the listener.
        let outside = smoothstep(0.05, 0.35, distance / half);
        let spatial = outside * emitter_gain;
        let inside_share = (1.0 - outside) * emitter_gain;

        // Spread blurs the direction: less time difference, softer shadow.
        let itd_scale = 1.0 - 0.8 * spread;
        let base = HEAD_RADIUS_M / SPEED_OF_SOUND_M_S;
        let itd =
            |incidence: f32| ((ear_delay_seconds(incidence) - base) * itd_scale + base) * rate;

        let mut pinna_tau = [0.0f32; 5];
        let az_deg = azimuth.to_degrees();
        let el_deg = elevation.to_degrees();
        for k in 0..5 {
            let tau = PINNA_A[k]
                * (az_deg / 2.0).to_radians().cos()
                * (PINNA_D[k] * (90.0 - el_deg)).to_radians().sin()
                + PINNA_B[k];
            pinna_tau[k] = tau.max(1.0) * rate / 44_100.0;
        }

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
            for (slot, image) in reflections.iter_mut().zip(images) {
                *slot = reflection_for(image, distance, gain, spread, rate);
            }
        }

        PathParams {
            itd_l: itd(incidence_l),
            itd_r: itd(incidence_r),
            shadow_l: head_shadow(incidence_l, spread, rate),
            shadow_r: head_shadow(incidence_r, spread, rate),
            pinna_tau,
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
