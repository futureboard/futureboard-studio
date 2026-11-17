//! RGBA8 → NV12 (BT.709, limited range), 2×2 chroma average, parallel over row pairs.

use rayon::prelude::*;

// Fixed-point BT.709 limited-range coefficients (×2^16), from Y' = 16 + 219·Y, C = 128 + 224·C.
const YR: i32 = (0.2126 * 219.0 / 255.0 * 65536.0) as i32;
const YG: i32 = (0.7152 * 219.0 / 255.0 * 65536.0) as i32;
const YB: i32 = (0.0722 * 219.0 / 255.0 * 65536.0) as i32;
const UR: i32 = -((0.2126 / 1.8556) * 224.0 / 255.0 * 65536.0) as i32;
const UG: i32 = -((0.7152 / 1.8556) * 224.0 / 255.0 * 65536.0) as i32;
const UB: i32 = ((0.5) * 224.0 / 255.0 * 65536.0) as i32;
const VR: i32 = ((0.5) * 224.0 / 255.0 * 65536.0) as i32;
const VG: i32 = -((0.7152 / 1.5748) * 224.0 / 255.0 * 65536.0) as i32;
const VB: i32 = -((0.0722 / 1.5748) * 224.0 / 255.0 * 65536.0) as i32;

/// Convert `w × h` RGBA8 to NV12 in `out` (resized to `w * h * 3 / 2`). `w`, `h` even.
pub fn rgba_to_nv12(rgba: &[u8], w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    y_plane
        .par_chunks_mut(w * 2)
        .zip(uv_plane.par_chunks_mut(w))
        .enumerate()
        .for_each(|(cy, (yrows, uvrow))| {
            let r0 = &rgba[cy * 2 * w * 4..(cy * 2 + 1) * w * 4];
            let r1 = &rgba[(cy * 2 + 1) * w * 4..(cy * 2 + 2) * w * 4];
            let (y0, y1) = yrows.split_at_mut(w);
            for cx in 0..w / 2 {
                let mut sr = 0i32;
                let mut sg = 0i32;
                let mut sb = 0i32;
                for (row, yrow) in [(r0, &mut *y0), (r1, &mut *y1)] {
                    for dx in 0..2 {
                        let x = cx * 2 + dx;
                        let p = &row[x * 4..x * 4 + 3];
                        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
                        yrow[x] = (((YR * r + YG * g + YB * b) >> 16) + 16).clamp(0, 255) as u8;
                        sr += r;
                        sg += g;
                        sb += b;
                    }
                }
                let (r, g, b) = (sr / 4, sg / 4, sb / 4);
                uvrow[cx * 2] = (((UR * r + UG * g + UB * b) >> 16) + 128).clamp(0, 255) as u8;
                uvrow[cx * 2 + 1] = (((VR * r + VG * g + VB * b) >> 16) + 128).clamp(0, 255) as u8;
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_black_grey() {
        let mut out = Vec::new();
        let px = |c: u8| [c, c, c, 255];
        let rgba: Vec<u8> = [px(255), px(0), px(255), px(0)].concat();
        rgba_to_nv12(&rgba, 2, 2, &mut out);
        assert!((out[0] as i32 - 235).abs() <= 1, "{out:?}");
        assert!((out[1] as i32 - 16).abs() <= 1);
        assert!((out[4] as i32 - 128).abs() <= 1 && (out[5] as i32 - 128).abs() <= 1);
    }

    #[test]
    fn red_chroma() {
        let mut out = Vec::new();
        let rgba: Vec<u8> = [[255u8, 0, 0, 255]; 4].concat();
        rgba_to_nv12(&rgba, 2, 2, &mut out);
        // BT.709 limited red: Y 63, Cb 102, Cr 240.
        assert!((out[0] as i32 - 63).abs() <= 1, "{out:?}");
        assert!((out[4] as i32 - 102).abs() <= 1 && (out[5] as i32 - 240).abs() <= 1, "{out:?}");
    }
}
