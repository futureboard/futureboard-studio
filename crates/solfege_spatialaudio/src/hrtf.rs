//! The measured head: the MIT KEMAR set as minimum-phase filters and
//! interaural time differences.
//!
//! Data: Bill Gardner and Keith Martin, "HRTF Measurements of a KEMAR
//! Dummy-Head Microphone", MIT Media Lab Perceptual Computing Technical Report
//! #280, 1994 (see `data/MIT_KEMAR_NOTICE.md`). `tools/kemar_to_hrir.py`
//! turned the "compact" set into `data/mit_kemar.hrir`: each direction's two
//! ears as diffuse-field-equalised minimum-phase filters, with the time
//! between the ears kept apart. Filters and delays interpolate separately
//! between the measured directions, so a moving source sweeps smoothly
//! instead of comb filtering between two differently delayed responses.
//!
//! The converter also sharpens front against behind. On ears that are not
//! KEMAR's, the dummy head's few dB of difference between a direction and its
//! mirror image behind the ears' axis mostly go unheard, and everything
//! behind sounds in front; that difference is scaled up above 1 kHz (behind
//! darker, in front barely brighter), leaving the time and level between the
//! ears as measured.
//!
//! The set is measured on the right half (azimuth `0..=180°`, clockwise);
//! the left half is its mirror image, ears swapped. Elevations run from −40°
//! to 90°; anything lower is heard at −40°.

use std::sync::{Arc, Mutex, OnceLock};

static KEMAR: &[u8] = include_bytes!("../data/mit_kemar.hrir");

/// One measured direction.
#[derive(Debug, Clone, Copy)]
struct Entry {
    azimuth_deg: f32,
    /// Left-ear delay minus right-ear delay, seconds.
    itd_s: f32,
}

#[derive(Debug, Clone)]
struct Ring {
    elevation_deg: f32,
    /// Index of the ring's first entry; its entries are contiguous and
    /// sorted by azimuth from 0.
    first: usize,
    count: usize,
}

/// A measured set at one sample rate.
#[derive(Debug)]
pub(crate) struct HrirSet {
    taps: usize,
    /// Samples every filter starts late by: resampling spreads a response's
    /// first tap into a little ringing ahead of it, kept rather than cut off.
    latency: usize,
    rings: Vec<Ring>,
    entries: Vec<Entry>,
    /// `[entry][ear (0 left, 1 right)][tap]`.
    coeffs: Vec<f32>,
}

/// Which measured responses make one direction, and how much of each.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct HrirPick {
    /// `(entry, ears swapped, weight)`; unused slots weigh nothing.
    parts: [(u16, bool, f32); 4],
    /// Left-ear delay minus right-ear delay, seconds.
    pub itd_s: f32,
}

/// The broadband level a fully spread source reaches each ear at: the same
/// -3 dB the set is normalised to on average.
const DIFFUSE_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

impl HrirSet {
    /// The set at `sample_rate`, built once per rate and shared. Allocates
    /// and locks: call it where the renderer is built, never while rendering.
    pub fn shared(sample_rate: u32) -> Arc<HrirSet> {
        static SETS: Mutex<Vec<(u32, Arc<HrirSet>)>> = Mutex::new(Vec::new());
        let mut sets = SETS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, set)) = sets.iter().find(|(rate, _)| *rate == sample_rate) {
            return set.clone();
        }
        let set = Arc::new(measured().resampled(sample_rate as f32));
        sets.push((sample_rate, set.clone()));
        set
    }

    /// Filter length, taps.
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// Samples every filter starts late by.
    pub fn latency(&self) -> usize {
        self.latency
    }

    fn response(&self, entry: usize, ear: usize) -> &[f32] {
        let start = (entry * 2 + ear) * self.taps;
        &self.coeffs[start..start + self.taps]
    }

    /// The measured responses around `azimuth` (radians, clockwise from the
    /// front) and `elevation` (radians up).
    pub fn pick(&self, azimuth: f32, elevation: f32) -> HrirPick {
        let mut az = azimuth.to_degrees().rem_euclid(360.0);
        if !az.is_finite() {
            az = 0.0;
        }
        // The left half is the right half's mirror image.
        let mirrored = az > 180.0;
        if mirrored {
            az = 360.0 - az;
        }
        let first = self.rings.first().map_or(0.0, |r| r.elevation_deg);
        let last = self.rings.last().map_or(0.0, |r| r.elevation_deg);
        let el = elevation.to_degrees();
        let el = if el.is_finite() {
            el.clamp(first, last)
        } else {
            0.0
        };

        let upper = self
            .rings
            .iter()
            .position(|ring| ring.elevation_deg >= el)
            .unwrap_or(self.rings.len() - 1);
        let lower = upper.saturating_sub(1);
        let (lower, upper, w_upper) = if upper == 0 || self.rings[upper].elevation_deg == el {
            (upper, upper, 0.0)
        } else {
            let (a, b) = (
                self.rings[lower].elevation_deg,
                self.rings[upper].elevation_deg,
            );
            (lower, upper, (el - a) / (b - a))
        };

        let mut pick = HrirPick::default();
        let mut slot = 0;
        for (ring, ring_weight) in [(lower, 1.0 - w_upper), (upper, w_upper)] {
            if ring_weight <= 0.0 && slot > 0 {
                continue;
            }
            for (entry, swapped, weight) in self.ring_pair(&self.rings[ring], az) {
                let weight = weight * ring_weight;
                let itd = self.entries[entry].itd_s;
                pick.itd_s += weight * if swapped { -itd } else { itd };
                pick.parts[slot] = (entry as u16, swapped, weight);
                slot += 1;
            }
        }
        if mirrored {
            pick.itd_s = -pick.itd_s;
            for part in &mut pick.parts {
                part.1 = !part.1;
            }
        }
        pick
    }

    /// The two responses of `ring` either side of `az` (`0..=180`), with
    /// their weights. Past the ring's last measured azimuth the other side is
    /// that response's mirror image.
    fn ring_pair(&self, ring: &Ring, az: f32) -> [(usize, bool, f32); 2] {
        let entries = &self.entries[ring.first..ring.first + ring.count];
        let next = entries
            .iter()
            .position(|e| e.azimuth_deg > az)
            .unwrap_or(entries.len());
        if next == 0 {
            return [(ring.first, false, 1.0), (ring.first, false, 0.0)];
        }
        let below = next - 1;
        let a = entries[below].azimuth_deg;
        let (upper, upper_az, swapped) = if next < entries.len() {
            (next, entries[next].azimuth_deg, false)
        } else {
            (below, 360.0 - a, true)
        };
        let span = upper_az - a;
        let w = if span > 1.0e-6 {
            ((az - a) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        [
            (ring.first + below, false, 1.0 - w),
            (ring.first + upper, swapped, w),
        ]
    }

    /// The filters of `pick`, time-reversed (newest input sample last), into
    /// `left` and `right`, blurred toward a plain level by `spread`
    /// (`0..=1`).
    pub fn fill_reversed(&self, pick: &HrirPick, spread: f32, left: &mut [f32], right: &mut [f32]) {
        let taps = self.taps.min(left.len()).min(right.len());
        left[..taps].fill(0.0);
        right[..taps].fill(0.0);
        let keep = 1.0 - spread.clamp(0.0, 1.0);
        for &(entry, swapped, weight) in &pick.parts {
            let weight = weight * keep;
            if weight <= 0.0 {
                continue;
            }
            let (from_l, from_r) = if swapped { (1, 0) } else { (0, 1) };
            let (hl, hr) = (
                self.response(entry as usize, from_l),
                self.response(entry as usize, from_r),
            );
            for k in 0..taps {
                left[taps - 1 - k] += weight * hl[k];
                right[taps - 1 - k] += weight * hr[k];
            }
        }
        let diffuse = (1.0 - keep) * DIFFUSE_GAIN;
        left[taps - 1] += diffuse;
        right[taps - 1] += diffuse;
    }

    /// The set at another rate: each filter band-limited and re-read with a
    /// windowed sinc, scaled so its frequency response keeps its level.
    fn resampled(&self, rate: f32) -> HrirSet {
        const SOURCE_RATE: f32 = 44_100.0;
        const HALF_WINDOW: f32 = 16.0;
        if (rate - SOURCE_RATE).abs() < 0.5 {
            return HrirSet {
                taps: self.taps,
                latency: 0,
                rings: self.rings.clone(),
                entries: self.entries.clone(),
                coeffs: self.coeffs.clone(),
            };
        }
        let ratio = SOURCE_RATE / rate;
        let cutoff = (rate / SOURCE_RATE).min(1.0);
        // The sinc's ringing ahead of the first tap, as far as it matters.
        let latency = (8.0 / ratio).ceil() as usize;
        let taps = ((self.taps as f32 / ratio).ceil() as usize).max(1) + latency;
        let mut coeffs = vec![0.0f32; self.entries.len() * 2 * taps];
        for (index, out) in coeffs.chunks_exact_mut(taps).enumerate() {
            let src = &self.coeffs[index * self.taps..(index + 1) * self.taps];
            for (n, value) in out.iter_mut().enumerate() {
                let t = (n as f32 - latency as f32) * ratio;
                let mut acc = 0.0;
                for (k, h) in src.iter().enumerate() {
                    let x = t - k as f32;
                    if x.abs() >= HALF_WINDOW {
                        continue;
                    }
                    let arg = std::f32::consts::PI * cutoff * x;
                    let sinc = if arg.abs() < 1.0e-6 {
                        1.0
                    } else {
                        arg.sin() / arg
                    };
                    // Blackman window over ±HALF_WINDOW source samples.
                    let phase = std::f32::consts::PI * (x / HALF_WINDOW + 1.0);
                    let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
                    acc += h * cutoff * sinc * window;
                }
                *value = acc * ratio;
            }
        }
        HrirSet {
            taps,
            latency,
            rings: self.rings.clone(),
            entries: self.entries.clone(),
            coeffs,
        }
    }
}

/// The set as measured, at 44.1 kHz.
fn measured() -> &'static HrirSet {
    static SET: OnceLock<HrirSet> = OnceLock::new();
    SET.get_or_init(|| parse(KEMAR).expect("the embedded KEMAR set is well formed"))
}

fn parse(bytes: &[u8]) -> Option<HrirSet> {
    struct Cursor<'a>(&'a [u8]);
    impl Cursor<'_> {
        fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
            let (head, rest) = self.0.split_first_chunk::<N>()?;
            self.0 = rest;
            Some(*head)
        }
        fn u32(&mut self) -> Option<u32> {
            self.take().map(u32::from_le_bytes)
        }
        fn i32(&mut self) -> Option<i32> {
            self.take().map(i32::from_le_bytes)
        }
        fn f32(&mut self) -> Option<f32> {
            self.take().map(f32::from_le_bytes)
        }
        fn i16(&mut self) -> Option<i16> {
            self.take().map(i16::from_le_bytes)
        }
    }

    let mut c = Cursor(bytes);
    if &c.take::<8>()? != b"SHRIR1\0\0" {
        return None;
    }
    let rate = c.u32()?;
    let taps = c.u32()? as usize;
    let ring_count = c.u32()? as usize;
    if rate != 44_100 || taps == 0 || ring_count == 0 {
        return None;
    }
    let mut rings = Vec::with_capacity(ring_count);
    let mut entries = Vec::new();
    let mut raw = Vec::new();
    for _ in 0..ring_count {
        let elevation_deg = c.i32()? as f32;
        let count = c.u32()? as usize;
        rings.push(Ring {
            elevation_deg,
            first: entries.len(),
            count,
        });
        for _ in 0..count {
            let azimuth_deg = c.f32()?;
            let itd_s = c.f32()?;
            entries.push(Entry { azimuth_deg, itd_s });
            for _ in 0..2 * taps {
                raw.push(c.i16()?);
            }
        }
    }
    let scale = c.f32()?;
    let coeffs = raw.into_iter().map(|q| q as f32 * scale).collect();
    Some(HrirSet {
        taps,
        latency: 0,
        rings,
        entries,
        coeffs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn energy(h: &[f32]) -> f32 {
        h.iter().map(|x| x * x).sum()
    }

    fn filters(set: &HrirSet, az_deg: f32, el_deg: f32) -> (Vec<f32>, Vec<f32>, f32) {
        let pick = set.pick(az_deg.to_radians(), el_deg.to_radians());
        let mut l = vec![0.0; set.taps()];
        let mut r = vec![0.0; set.taps()];
        set.fill_reversed(&pick, 0.0, &mut l, &mut r);
        (l, r, pick.itd_s)
    }

    #[test]
    fn the_embedded_set_is_all_there() {
        let set = measured();
        assert_eq!(set.entries.len(), 368);
        assert_eq!(set.rings.len(), 14);
        assert!(set.coeffs.iter().all(|c| c.is_finite()));
    }

    #[test]
    fn a_source_on_the_right_reaches_the_right_ear_first_and_louder() {
        let set = HrirSet::shared(48_000);
        let (l, r, itd) = filters(&set, 90.0, 0.0);
        // About 0.7 ms: KEMAR's head.
        assert!((0.00055..0.0008).contains(&itd), "itd {itd}");
        assert!(energy(&r) > 8.0 * energy(&l));
    }

    #[test]
    fn the_halves_mirror_each_other_exactly() {
        let set = HrirSet::shared(48_000);
        for (az, el) in [(37.0, 0.0), (123.0, 25.0), (178.0, 55.0), (91.0, -30.0)] {
            let (l, r, itd) = filters(&set, az, el);
            let (ml, mr, mitd) = filters(&set, 360.0 - az, el);
            let close = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1.0e-5);
            assert!(close(&l, &mr) && close(&r, &ml), "{az},{el}");
            assert!((itd + mitd).abs() < 1.0e-9);
        }
    }

    #[test]
    fn straight_ahead_and_behind_have_no_time_difference() {
        let set = HrirSet::shared(44_100);
        for az in [0.0, 180.0] {
            let (l, r, itd) = filters(&set, az, 0.0);
            assert!(itd.abs() < 2.0e-5, "{az}: {itd}");
            assert!((energy(&l) - energy(&r)).abs() < 1.0e-4 * energy(&l));
        }
    }

    #[test]
    fn behind_sounds_darker_than_in_front() {
        // The pinna shades what comes from behind: less treble, the cue
        // that tells behind from in front.
        let set = HrirSet::shared(48_000);
        let treble = |h: &[f32]| {
            let diff: Vec<f32> = h.windows(2).map(|w| w[1] - w[0]).collect();
            energy(&diff) / energy(h)
        };
        let (front, _, _) = filters(&set, 0.0, 0.0);
        let (back, _, _) = filters(&set, 180.0, 0.0);
        assert!(treble(&back) < treble(&front) * 0.8);
    }

    #[test]
    fn resampling_keeps_the_level() {
        let at_44 = HrirSet::shared(44_100);
        let at_96 = HrirSet::shared(96_000);
        let (l44, _, _) = filters(&at_44, 30.0, 10.0);
        let (l96, _, _) = filters(&at_96, 30.0, 10.0);
        // DC gain: the sum of the taps.
        let (dc44, dc96) = (l44.iter().sum::<f32>(), l96.iter().sum::<f32>());
        assert!((dc44 - dc96).abs() < 0.05 * dc44.abs(), "{dc44} {dc96}");
        assert!(at_96.taps() > 2 * at_44.taps() - 4);
    }

    #[test]
    fn neighbouring_directions_blend_smoothly() {
        let set = HrirSet::shared(48_000);
        let (mut prev, _, _) = filters(&set, 0.0, 12.0);
        for step in 1..=360 {
            let (l, _, _) = filters(&set, step as f32, 12.0);
            let diff: f32 = l.iter().zip(&prev).map(|(a, b)| (a - b) * (a - b)).sum();
            assert!(
                diff < 0.25 * energy(&l).max(energy(&prev)),
                "jump at {step}°"
            );
            prev = l;
        }
    }
}
