//! Floor decode and curve synthesis (spec §6 floor 0, §7 floor 1).

use crate::bitread::{ilog, BitReader};
use crate::codebook::Codebook;
use crate::setup::{Floor0, Floor1};
use mkff_audio_core::Result;

/// Floor 1 amplitude ranges per multiplier (spec §7.2.3).
const RANGES: [i32; 4] = [256, 128, 86, 64];

/// Maximum floor 1 X-list length (2 + 31 partitions × 8 dimensions).
const MAX_FLOOR1_VALUES: usize = 256;

/// The floor 1 inverse-dB table (spec §10.1): 256 steps of 140/256 dB up to 0 dB.
pub(crate) fn inverse_db_table() -> [f32; 256] {
    let mut t = [0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        *v = 10f64.powf((i as f64 - 255.0) * 7.0 / 256.0) as f32;
    }
    t
}

/// Decoded floor parameters of one channel, kept between floor decode and curve synthesis.
#[derive(Clone, Debug)]
pub(crate) struct FloorData {
    /// Floor 1: final Y values; floor 0: unused.
    y: [i32; MAX_FLOOR1_VALUES],
    step2: [bool; MAX_FLOOR1_VALUES],
    /// Floor 0 amplitude and `cos` of the LSP coefficients.
    amplitude: u64,
    cos_coeffs: Vec<f64>,
}

impl Default for FloorData {
    fn default() -> Self {
        Self {
            y: [0; MAX_FLOOR1_VALUES],
            step2: [false; MAX_FLOOR1_VALUES],
            amplitude: 0,
            cos_coeffs: Vec::new(),
        }
    }
}

fn render_point(x0: i32, y0: i32, x1: i32, y1: i32, x: i32) -> i32 {
    let dy = y1 - y0;
    let adx = x1 - x0;
    if adx == 0 {
        return y0;
    }
    let off = dy.abs() * (x - x0) / adx;
    if dy < 0 {
        y0 - off
    } else {
        y0 + off
    }
}

/// Multiply `v[x0..x1]` by the inverse-dB values along the integer line (spec §9.2.7).
#[inline]
fn render_line(x0: usize, y0: i32, x1: usize, y1: i32, v: &mut [f32], db: &[f32; 256]) {
    let end = x1.min(v.len());
    if x0 >= end {
        return;
    }
    let dy = y1 - y0;
    let adx = (x1 - x0) as i32;
    let base = dy / adx;
    let sy = if dy < 0 { base - 1 } else { base + 1 };
    let ady = dy.abs() - base.abs() * adx;
    let mut y = y0;
    let mut err = 0;
    v[x0] *= db[(y as usize) & 255];
    let step = sy - base;
    for s in &mut v[x0 + 1..end] {
        // branch-free form of: err += ady; if err >= adx { err -= adx; y += sy } else { y += base }
        err += ady;
        let over = i32::from(err >= adx);
        err -= adx & -over;
        y += base + (step & -over);
        *s *= db[(y as usize) & 255];
    }
}

impl Floor1 {
    /// Decode the floor (packet part and amplitude synthesis). Returns `false` when the floor is
    /// unused for this channel, including when the packet ends during the floor (spec §7.2.3).
    pub fn decode(&self, r: &mut BitReader, books: &[Codebook], d: &mut FloorData) -> bool {
        self.decode_inner(r, books, d).unwrap_or(false)
    }

    fn decode_inner(
        &self,
        r: &mut BitReader,
        books: &[Codebook],
        d: &mut FloorData,
    ) -> Result<bool> {
        if !r.read_bit()? {
            return Ok(false);
        }
        let range = RANGES[(self.multiplier - 1) as usize & 3];
        let bits = ilog(range as u32 - 1);
        let y = &mut d.y;
        y[0] = r.read(bits)? as i32;
        y[1] = r.read(bits)? as i32;
        let mut offset = 2usize;
        for &class in &self.partition_class {
            let class = usize::from(class);
            let cdim = usize::from(self.class_dimensions[class]);
            let cbits = u32::from(self.class_subclasses[class]);
            let csub = (1u32 << cbits) - 1;
            let mut cval = if cbits > 0 {
                books[usize::from(self.class_masterbook[class])].decode_scalar(r)?
            } else {
                0
            };
            for slot in &mut y[offset..offset + cdim] {
                let book = self.subclass_books[class][(cval & csub) as usize];
                cval >>= cbits;
                *slot = if book >= 0 {
                    books[book as usize].decode_scalar(r)? as i32
                } else {
                    0
                };
            }
            offset += cdim;
        }

        // amplitude value synthesis (spec §7.2.4 step 1)
        let n = self.xs.len();
        let step2 = &mut d.step2;
        y[0] = y[0].min(range - 1);
        y[1] = y[1].min(range - 1);
        step2[0] = true;
        step2[1] = true;
        for i in 2..n {
            let lo = usize::from(self.low[i]);
            let hi = usize::from(self.high[i]);
            let predicted = render_point(
                i32::from(self.xs[lo]),
                y[lo],
                i32::from(self.xs[hi]),
                y[hi],
                i32::from(self.xs[i]),
            );
            let val = y[i];
            let highroom = range - predicted;
            let lowroom = predicted;
            let room = highroom.min(lowroom) * 2;
            if val != 0 {
                step2[lo] = true;
                step2[hi] = true;
                step2[i] = true;
                let v = if val >= room {
                    if highroom > lowroom {
                        val - lowroom + predicted
                    } else {
                        predicted - val + highroom - 1
                    }
                } else if val & 1 == 1 {
                    predicted - (val + 1) / 2
                } else {
                    predicted + val / 2
                };
                y[i] = v.clamp(0, range - 1);
            } else {
                step2[i] = false;
                y[i] = predicted;
            }
        }
        Ok(true)
    }

    /// Multiply `spectrum` (`n/2` values) by the floor curve (spec §7.2.4 step 2).
    pub fn apply(&self, d: &FloorData, spectrum: &mut [f32], db: &[f32; 256]) {
        let mult = self.multiplier as i32;
        let n = spectrum.len();
        let first = usize::from(self.sorted[0]);
        let mut lx = usize::from(self.xs[first]);
        let mut ly = d.y[first] * mult;
        for &i in &self.sorted[1..] {
            let i = usize::from(i);
            if d.step2[i] {
                let hy = d.y[i] * mult;
                let hx = usize::from(self.xs[i]);
                render_line(lx, ly, hx, hy, spectrum, db);
                lx = hx;
                ly = hy;
            }
        }
        if lx < n {
            render_line(lx, ly, n, ly, spectrum, db);
        }
    }
}

impl Floor0 {
    /// Decode the floor 0 packet part. Returns `false` when the floor is unused.
    pub fn decode(&self, r: &mut BitReader, books: &[Codebook], d: &mut FloorData) -> bool {
        self.decode_inner(r, books, d).unwrap_or(false)
    }

    fn decode_inner(
        &self,
        r: &mut BitReader,
        books: &[Codebook],
        d: &mut FloorData,
    ) -> Result<bool> {
        let amplitude = r.read_u64(self.amplitude_bits)?;
        if amplitude == 0 {
            return Ok(false);
        }
        let booknumber = r.read(ilog(self.books.len() as u32))? as usize;
        let Some(&book) = self.books.get(booknumber) else {
            return Ok(false);
        };
        let book = &books[usize::from(book)];
        let c = &mut d.cos_coeffs;
        c.clear();
        let mut last = 0.0f32;
        while c.len() < self.order {
            let v = book.decode_vq(r)?;
            for &x in v {
                c.push(f64::from(x + last));
            }
            last = c.last().map_or(0.0, |&x| x as f32);
        }
        c.truncate(self.order);
        for x in c.iter_mut() {
            *x = x.cos();
        }
        d.amplitude = amplitude;
        Ok(true)
    }

    /// Multiply `spectrum` by the floor 0 curve (spec §6.2.3).
    pub fn apply(&self, d: &FloorData, spectrum: &mut [f32], long: bool) {
        let map = &self.map[usize::from(long)];
        let n = spectrum.len().min(map.len());
        let c = &d.cos_coeffs;
        let order = c.len();
        let max_amp = ((1u128 << self.amplitude_bits) - 1) as f64;
        let offset = f64::from(self.amplitude_offset);
        let amp = d.amplitude as f64;
        let mut i = 0;
        while i < n {
            let m = map[i];
            let cw = (std::f64::consts::PI * f64::from(m) / f64::from(self.bark_map_size)).cos();
            let mut p = 1.0f64;
            let mut q = 1.0f64;
            for (j, &cj) in c.iter().enumerate() {
                let t = 4.0 * (cj - cw) * (cj - cw);
                if j & 1 == 1 {
                    p *= t;
                } else {
                    q *= t;
                }
            }
            if order & 1 == 1 {
                p *= 1.0 - cw * cw;
                q *= 0.25;
            } else {
                p *= (1.0 - cw) / 2.0;
                q *= (1.0 + cw) / 2.0;
            }
            let lin =
                (0.115_129_25 * (amp * offset / (max_amp * (p + q).sqrt()) - offset)).exp() as f32;
            // degenerate coefficients (p + q = 0) only occur in broken streams
            let lin = if lin.is_finite() { lin } else { 0.0 };
            while i < n && map[i] == m {
                spectrum[i] *= lin;
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_db_matches_spec_endpoints() {
        let t = inverse_db_table();
        assert!((t[0] - 1.064_986_3e-7).abs() < 1e-13);
        assert!((t[1] - 1.134_195_1e-7).abs() < 1e-13);
        assert!((t[254] - 0.938_979_8).abs() < 1e-6);
        assert_eq!(t[255], 1.0);
    }

    #[test]
    fn line_rendering() {
        let mut db = [0f32; 256];
        for (i, v) in db.iter_mut().enumerate() {
            *v = i as f32;
        }
        let mut v = vec![1.0f32; 10];
        render_line(0, 0, 10, 5, &mut v, &db);
        assert_eq!(v, [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0]);
        let mut v = vec![1.0f32; 5];
        render_line(1, 20, 4, 11, &mut v, &db);
        assert_eq!(v, [1.0, 20.0, 17.0, 14.0, 1.0]);
        assert_eq!(render_point(0, 10, 10, 0, 5), 5);
        assert_eq!(render_point(0, 0, 3, 10, 1), 3);
    }
}
