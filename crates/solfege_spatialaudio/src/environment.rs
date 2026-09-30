//! The spaces Virtual Speaker's systems are heard in, as impulse responses
//! worked out from their geometry.
//!
//! Each [`Room`] is a box — a car cabin, a bedroom, a club, a hall — with a
//! listener and loudspeakers at real positions in it and three kinds of
//! surface (walls, floor, ceiling), each absorbing low, mid and high
//! frequencies by its own amount. From that:
//!
//! * **Early response by the image-source method** (Allen & Berkley, 1979).
//!   Every reflection path up to the room's early window becomes a mirror
//!   image of the loudspeaker: delayed by its length, weakened by distance,
//!   by each surface it bounced off (per band) and by the air. The direct
//!   sound and the first two orders of reflection are heard through the
//!   measured head, from their own directions — a car's floor and windscreen
//!   bounces arrive from below and ahead; later, denser orders carry their
//!   time and level difference between the ears only.
//! * **Late reverberation** from the room's own numbers: Sabine's decay time
//!   `T = 0.161·V / A`, and a level from the reverberant-to-direct energy
//!   ratio `16π·r²·(1 − ᾱ) / A` at the listener's distance, taken over by
//!   the shared [`crate::RoomTail`] once the early window ends.
//!
//! Two things keep a room from sounding like a tiled box, as the plain method
//! would: every loudspeaker **beams** (a cardioid-like pattern per band, so a
//! room's reflections sit under its direct sound the way a real speaker's
//! directivity index puts them), and every surface **scatters** (each bounce
//! leaves less of a reflection as a clean image; the rest is the diffuse
//! tail). The rooms themselves are calibrated against measurements — see
//! the Virtual Speaker models and their `rooms_measure_like_real_rooms` test.
//!
//! On speakers (not headphones) the same paths are laid out between the two
//! real loudspeakers by their direction instead of through the head.
//!
//! Everything here runs off the audio thread, once per sample rate.

use crate::hrtf::HrirSet;
use crate::room_tail::RoomTail;

const SPEED_OF_SOUND_M_S: f32 = 343.0;
/// Reflection orders heard through the measured head; higher orders are
/// dense and diffuse, and get time and level only.
const HRTF_ORDERS: u32 = 2;
/// Band edges: low / mid / high.
const LOW_MID_HZ: f32 = 400.0;
const MID_HIGH_HZ: f32 = 3_000.0;
/// Air absorption per metre, dB, for the mid and high bands.
const AIR_DB_PER_M: [f32; 3] = [0.0, 0.01, 0.1];
/// The reverberant field's share, capped: past this the numbers stop
/// describing a listener anyone would put in that seat.
const MAX_REVERB_RATIO: f32 = 3.0;

/// Absorption of a kind of surface in the low, mid and high bands, `0..1`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Surface(pub [f32; 3]);

/// A loudspeaker: where it stands, which channel feeds it (`0` left, `1`
/// right; a mono system has only `0`), how much of each band it reproduces
/// (a door woofer has no treble, a dash tweeter no bass), and how it beams.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Loudspeaker {
    pub position: [f32; 3],
    pub channel: usize,
    pub bands: [f32; 3],
    /// Directivity per band, as the exponent `p` of the pressure pattern
    /// `((1 + cos a) / 2)^p` about the axis aimed at the listener: `0` is
    /// omnidirectional, `1` a cardioid (directivity factor `Q = 2p + 1`).
    /// Every real loudspeaker beams more as the frequency rises, which is
    /// what keeps a room's reflections under its direct sound.
    pub directivity: [f32; 3],
}

/// A box of a room, metres: `x` left to right, `y` back to front (the
/// listener faces `+y`), `z` up from the floor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Room {
    pub size: [f32; 3],
    /// The listener's head.
    pub listener: [f32; 3],
    pub walls: Surface,
    pub floor: Surface,
    pub ceiling: Surface,
    /// How long the image-source response runs before the late tail takes
    /// over, seconds.
    pub early_s: f32,
    /// Mid-band scattering coefficient, `0..1`: the share of the energy a
    /// surface sends off in every direction instead of mirror-like.
    /// Furniture, shelves, people and seats scatter; each bounce leaves less
    /// of a reflection as a clean image and more of it in the diffuse tail.
    pub scattering: f32,
    pub loudspeakers: &'static [Loudspeaker],
}

/// A room's response, ready to be partitioned.
#[derive(Debug, Clone)]
pub(crate) struct RoomResponse {
    /// `[channel][ear or output]`.
    pub irs: Vec<[Vec<f32>; 2]>,
    /// For the late tail: its size (half the room's cube root), decay time
    /// and level.
    pub tail_half_size_m: f32,
    pub rt60_s: f32,
    pub rt60_high_s: f32,
    pub tail_level: f32,
}

impl Room {
    fn channels(&self) -> usize {
        if self.loudspeakers.iter().any(|l| l.channel == 1) {
            2
        } else {
            1
        }
    }

    fn volume(&self) -> f32 {
        self.size[0] * self.size[1] * self.size[2]
    }

    /// Sabine absorption area in the mid band, m², and total surface, m².
    fn absorption(&self) -> (f32, f32) {
        let [w, l, h] = self.size;
        let floor = w * l;
        let walls = 2.0 * h * (w + l);
        (
            floor * (self.floor.0[1] + self.ceiling.0[1]) + walls * self.walls.0[1],
            2.0 * floor + walls,
        )
    }

    /// Sabine's reverberation time, seconds.
    pub fn rt60(&self) -> f32 {
        let (a, _) = self.absorption();
        (0.161 * self.volume() / a.max(1.0e-3)).clamp(0.08, 4.0)
    }

    /// The same in the high band, with the air's own absorption (`4mV`,
    /// `m` ≈ 0.01 per metre around 8 kHz) — which is what darkens a big
    /// room's tail.
    pub fn rt60_high(&self) -> f32 {
        let [w, l, h] = self.size;
        let floor = w * l;
        let walls = 2.0 * h * (w + l);
        let a = floor * (self.floor.0[2] + self.ceiling.0[2])
            + walls * self.walls.0[2]
            + 4.0 * 0.01 * self.volume();
        (0.161 * self.volume() / a.max(1.0e-3)).clamp(0.05, 4.0)
    }

    fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    /// The response of every channel at both ears (`headphones`) or both
    /// real loudspeakers.
    pub fn response(&self, headphones: bool, hrirs: &HrirSet, rate: f32) -> RoomResponse {
        let channels = self.channels();
        let nearest = self
            .loudspeakers
            .iter()
            .map(|l| Self::distance(l.position, self.listener))
            .fold(f32::INFINITY, f32::min)
            .max(0.05);
        let farthest = self
            .loudspeakers
            .iter()
            .map(|l| Self::distance(l.position, self.listener))
            .fold(0.0f32, f32::max);
        // The nearest loudspeaker's direct sound lands a few samples in, so
        // the interpolation and the ear farther from it have room.
        let lead = 4.0 + hrirs.latency() as f32;
        let window_m = self.early_s * SPEED_OF_SOUND_M_S;
        let len = ((farthest - nearest + window_m) / SPEED_OF_SOUND_M_S * rate).ceil() as usize
            + hrirs.taps()
            + 16;
        let mut bands = vec![
            [
                [vec![0.0f32; len], vec![0.0f32; len], vec![0.0f32; len]],
                [vec![0.0f32; len], vec![0.0f32; len], vec![0.0f32; len],]
            ];
            channels
        ];

        let taps = hrirs.taps();
        let mut hrir_l = vec![0.0f32; taps];
        let mut hrir_r = vec![0.0f32; taps];
        let reflect = |s: Surface| s.0.map(|a| (1.0 - a.clamp(0.0, 0.99)).sqrt());
        let (wall, floor, ceiling) = (
            reflect(self.walls),
            reflect(self.floor),
            reflect(self.ceiling),
        );
        let limit = nearest + window_m;

        // What of each bounce stays a mirror image: treble scatters most.
        let scattering = self.scattering.clamp(0.0, 0.95);
        let specular = [
            (1.0 - 0.5 * scattering).sqrt(),
            (1.0 - scattering).sqrt(),
            (1.0 - (1.3 * scattering).min(0.95)).sqrt(),
        ];

        for speaker in self.loudspeakers {
            let ch = speaker.channel.min(channels - 1);
            let to_listener = Self::distance(self.listener, speaker.position).max(1.0e-3);
            let aim = [
                (self.listener[0] - speaker.position[0]) / to_listener,
                (self.listener[1] - speaker.position[1]) / to_listener,
                (self.listener[2] - speaker.position[2]) / to_listener,
            ];
            let reach: [i32; 3] =
                std::array::from_fn(|a| (limit / (2.0 * self.size[a])).ceil() as i32 + 1);
            for u in 0..2 {
                for v in 0..2 {
                    for w in 0..2 {
                        for nx in -reach[0]..=reach[0] {
                            for ny in -reach[1]..=reach[1] {
                                for nz in -reach[2]..=reach[2] {
                                    let image = [
                                        image_coordinate(speaker.position[0], u, nx, self.size[0]),
                                        image_coordinate(speaker.position[1], v, ny, self.size[1]),
                                        image_coordinate(speaker.position[2], w, nz, self.size[2]),
                                    ];
                                    let d = Self::distance(image, self.listener).max(0.05);
                                    if d > limit {
                                        continue;
                                    }
                                    let hits_x = (nx - u as i32).unsigned_abs() + nx.unsigned_abs();
                                    let hits_y = (ny - v as i32).unsigned_abs() + ny.unsigned_abs();
                                    let hits_floor = (nz - w as i32).unsigned_abs();
                                    let hits_ceiling = nz.unsigned_abs();
                                    let order = hits_x + hits_y + hits_floor + hits_ceiling;
                                    // Which way the sound left the loudspeaker:
                                    // the path from the image, mirrored back on
                                    // every axis it bounced an odd number of
                                    // times along.
                                    let emitted = [
                                        (self.listener[0] - image[0]) / d * (1 - 2 * u) as f32,
                                        (self.listener[1] - image[1]) / d * (1 - 2 * v) as f32,
                                        (self.listener[2] - image[2]) / d * (1 - 2 * w) as f32,
                                    ];
                                    let cos_off_axis = (emitted[0] * aim[0]
                                        + emitted[1] * aim[1]
                                        + emitted[2] * aim[2])
                                        .clamp(-1.0, 1.0);
                                    let mut gain = [0.0f32; 3];
                                    for b in 0..3 {
                                        let beam = (0.5 * (1.0 + cos_off_axis))
                                            .powf(speaker.directivity[b]);
                                        gain[b] = speaker.bands[b]
                                            * beam
                                            * specular[b].powi(order as i32)
                                            * wall[b].powi((hits_x + hits_y) as i32)
                                            * floor[b].powi(hits_floor as i32)
                                            * ceiling[b].powi(hits_ceiling as i32)
                                            * (nearest / d)
                                            * 10f32.powf(-AIR_DB_PER_M[b] * d / 20.0);
                                    }
                                    let at = lead + (d - nearest) / SPEED_OF_SOUND_M_S * rate;
                                    let dir = [
                                        (image[0] - self.listener[0]) / d,
                                        (image[1] - self.listener[1]) / d,
                                        (image[2] - self.listener[2]) / d,
                                    ];
                                    let target = &mut bands[ch];
                                    if headphones {
                                        if order <= HRTF_ORDERS {
                                            let azimuth = dir[0].atan2(dir[1]);
                                            let elevation = dir[2].clamp(-1.0, 1.0).asin();
                                            let pick = hrirs.pick(azimuth, elevation);
                                            hrirs.fill_reversed(
                                                &pick,
                                                0.0,
                                                &mut hrir_l,
                                                &mut hrir_r,
                                            );
                                            hrir_l.reverse();
                                            hrir_r.reverse();
                                            // Positive: the left ear hears it later.
                                            let itd = pick.itd_s * rate;
                                            for b in 0..3 {
                                                add_filter(
                                                    &mut target[0][b],
                                                    at + itd.max(0.0),
                                                    &hrir_l,
                                                    gain[b],
                                                );
                                                add_filter(
                                                    &mut target[1][b],
                                                    at + (-itd).max(0.0),
                                                    &hrir_r,
                                                    gain[b],
                                                );
                                            }
                                        } else {
                                            // Woodworth's time difference, and the
                                            // head's broadband shading.
                                            let lateral = dir[0].clamp(-1.0, 1.0).asin();
                                            let itd = 0.0875 / SPEED_OF_SOUND_M_S
                                                * (lateral + lateral.sin())
                                                * rate;
                                            let level = |toward: f32| {
                                                (0.5 * (1.0 + 0.6 * toward)).sqrt()
                                                    * std::f32::consts::FRAC_1_SQRT_2
                                            };
                                            let (gl, gr) = (level(-dir[0]), level(dir[0]));
                                            for b in 0..3 {
                                                add_impulse(
                                                    &mut target[0][b],
                                                    at + itd.max(0.0) + hrirs.latency() as f32,
                                                    gain[b] * gl,
                                                );
                                                add_impulse(
                                                    &mut target[1][b],
                                                    at + (-itd).max(0.0) + hrirs.latency() as f32,
                                                    gain[b] * gr,
                                                );
                                            }
                                        }
                                    } else {
                                        // Real loudspeakers: the direct sound
                                        // plays from its own side; reflections
                                        // are laid between the two by
                                        // direction.
                                        let (gl, gr) = if order == 0 {
                                            match (channels, ch) {
                                                (1, _) => (
                                                    std::f32::consts::FRAC_1_SQRT_2,
                                                    std::f32::consts::FRAC_1_SQRT_2,
                                                ),
                                                (_, 0) => (1.0, 0.0),
                                                _ => (0.0, 1.0),
                                            }
                                        } else {
                                            let angle = (dir[0].clamp(-1.0, 1.0) + 1.0)
                                                * std::f32::consts::FRAC_PI_4;
                                            (angle.cos(), angle.sin())
                                        };
                                        for b in 0..3 {
                                            add_impulse(&mut target[0][b], at, gain[b] * gl);
                                            add_impulse(&mut target[1][b], at, gain[b] * gr);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Bands back into one response: the mid band as it is, the others
        // only where they differ from it — so a surface that treats every
        // band alike costs no filtering at all.
        let fade = ((0.004 * rate) as usize).min(len / 2).max(1);
        let irs = bands
            .into_iter()
            .map(|ears| {
                ears.map(|[low, mid, high]| {
                    let mut ir = mid.clone();
                    let mut lp = Biquad::new(false, LOW_MID_HZ, rate);
                    let mut hp = Biquad::new(true, MID_HIGH_HZ, rate);
                    for n in 0..len {
                        ir[n] += lp.tick(low[n] - mid[n]) + hp.tick(high[n] - mid[n]);
                    }
                    for (i, s) in ir[len - fade..].iter_mut().enumerate() {
                        *s *= 1.0 - i as f32 / fade as f32;
                    }
                    ir
                })
            })
            .collect();

        // The reverberant field past the early window.
        let rt60 = self.rt60();
        let (area, surface) = self.absorption();
        let mean_absorption = (area / surface).clamp(0.01, 0.99);
        // A beaming loudspeaker puts `Q` times less of its power into the
        // room than an omnidirectional one would for the same direct sound.
        let q = self
            .loudspeakers
            .iter()
            .map(|l| 2.0 * l.directivity[1] + 1.0)
            .sum::<f32>()
            / self.loudspeakers.len().max(1) as f32;
        let ratio = (16.0 * std::f32::consts::PI * nearest * nearest * (1.0 - mean_absorption)
            / (area.max(1.0e-3) * q))
            .min(MAX_REVERB_RATIO);
        let tail_half_size_m = self.volume().cbrt() / 2.0;
        let late = ratio * (-13.8 * self.early_s * 0.7 / rt60).exp();
        // The tail's own output for a unit impulse, so its level can be set
        // in energy against the direct sound (−3 dB per ear through the
        // head, as the measured set is normalised).
        let tail_energy = unit_tail_energy(tail_half_size_m, rt60, rate);
        let direct_energy = if headphones { 0.5 } else { 1.0 };
        RoomResponse {
            irs,
            tail_half_size_m,
            rt60_s: rt60,
            rt60_high_s: self.rt60_high(),
            tail_level: (late * direct_energy / tail_energy.max(1.0e-9)).sqrt(),
        }
    }
}

/// Allen & Berkley's image coordinate along one axis.
fn image_coordinate(source: f32, flip: i32, n: i32, size: f32) -> f32 {
    (1 - 2 * flip) as f32 * source + 2.0 * n as f32 * size
}

/// Four-point Lagrange weights for a fractional position `t` in `0..1`,
/// for samples at `-1, 0, 1, 2`.
fn lagrange(t: f32) -> [f32; 4] {
    [
        -t * (t - 1.0) * (t - 2.0) / 6.0,
        (t + 1.0) * (t - 1.0) * (t - 2.0) / 2.0,
        -(t + 1.0) * t * (t - 2.0) / 2.0,
        (t + 1.0) * t * (t - 1.0) / 6.0,
    ]
}

fn add_impulse(ir: &mut [f32], at: f32, gain: f32) {
    if gain == 0.0 || !at.is_finite() {
        return;
    }
    let whole = at.floor();
    let weights = lagrange(at - whole);
    for (k, w) in weights.iter().enumerate() {
        let i = whole as isize + k as isize - 1;
        if i >= 0 && (i as usize) < ir.len() {
            ir[i as usize] += gain * w;
        }
    }
}

fn add_filter(ir: &mut [f32], at: f32, filter: &[f32], gain: f32) {
    if gain == 0.0 || !at.is_finite() {
        return;
    }
    let whole = at.floor();
    let weights = lagrange(at - whole);
    for (k, w) in weights.iter().enumerate() {
        let start = whole as isize + k as isize - 1;
        let g = gain * w;
        for (j, h) in filter.iter().enumerate() {
            let i = start + j as isize;
            if i >= 0 && (i as usize) < ir.len() {
                ir[i as usize] += g * h;
            }
        }
    }
}

/// The late tail's output energy (one ear) for a unit impulse, at level 1.
fn unit_tail_energy(half_size_m: f32, rt60_s: f32, rate: f32) -> f32 {
    let mut tail = RoomTail::with_capacity(half_size_m, rate as u32);
    tail.set_room(half_size_m, rt60_s, 1.0);
    let len = ((rt60_s * 1.5 + 0.2) * rate) as usize;
    let mut energy = 0.0f64;
    let mut block_l = [0.0f32; 512];
    let mut block_r = [0.0f32; 512];
    let mut done = 0;
    while done < len {
        block_l.fill(0.0);
        block_r.fill(0.0);
        if done == 0 {
            block_l[0] = 1.0;
            block_r[0] = 1.0;
        }
        tail.process(&mut block_l, &mut block_r);
        if done == 0 {
            block_l[0] -= 1.0;
        }
        energy += block_l.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
        done += block_l.len();
    }
    energy as f32
}

/// A second-order Butterworth low- or high-pass, for the band merge.
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    fn new(highpass: bool, hz: f32, rate: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * hz / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = if highpass {
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0]
        } else {
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0]
        };
        Self {
            b: b.map(|v| v / a0),
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn tick(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ONE: [Loudspeaker; 1] = [Loudspeaker {
        position: [2.0, 4.0, 1.2],
        channel: 0,
        bands: [1.0; 3],
        directivity: [0.0; 3],
    }];

    fn room(absorption: f32) -> Room {
        Room {
            size: [4.0, 5.0, 3.0],
            listener: [2.0, 2.0, 1.2],
            walls: Surface([absorption; 3]),
            floor: Surface([absorption; 3]),
            ceiling: Surface([absorption; 3]),
            early_s: 0.08,
            scattering: 0.0,
            loudspeakers: &ONE,
        }
    }

    #[test]
    fn a_harder_room_rings_longer_and_reflects_more() {
        let hrirs = HrirSet::shared(48_000);
        let energy = |absorption: f32| {
            let r = room(absorption).response(true, &hrirs, 48_000.0);
            let ir = &r.irs[0][0];
            // After the direct sound: 2 m away is ~280 samples in.
            let late: f32 = ir[400..].iter().map(|s| s * s).sum();
            (late, r.rt60_s)
        };
        let (dead, dead_rt) = energy(0.8);
        let (live, live_rt) = energy(0.1);
        assert!(live > dead * 4.0, "live {live} dead {dead}");
        assert!(live_rt > dead_rt * 4.0);
    }

    #[test]
    fn the_direct_sound_arrives_first_and_straight_ahead() {
        let hrirs = HrirSet::shared(48_000);
        let r = room(0.3).response(true, &hrirs, 48_000.0);
        let first = |ir: &[f32]| ir.iter().position(|s| s.abs() > 1.0e-3).unwrap_or(ir.len());
        let (l, r_) = (&r.irs[0][0], &r.irs[0][1]);
        // Straight ahead: both ears at once, alike.
        assert!((first(l) as i32 - first(r_) as i32).abs() <= 1);
        assert!(first(l) < 64);
    }
}
