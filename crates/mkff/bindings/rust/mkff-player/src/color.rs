//! Y'CbCr → R'G'B' conversion coefficients for the GPU shader.

use mkff_codecs::Matrix;

/// Luma coefficients (Kr, Kb) of a matrix.
fn kr_kb(matrix: Matrix) -> (f32, f32) {
    match matrix {
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020Ncl => (0.2627, 0.0593),
    }
}

/// An affine 3×4 matrix taking normalised 8-bit samples `[Y, Cb, Cr, 1]` (code / 255, as a
/// `Unorm` texture returns them) to gamma-encoded R'G'B' in 0..1. Rows are R, G, B.
pub fn yuv_to_rgb_matrix(matrix: Matrix, full_range: bool) -> [[f32; 4]; 3] {
    let (kr, kb) = kr_kb(matrix);
    let kg = 1.0 - kr - kb;
    // Y' = ys * Y + yo, C = cs * C + co: undo the range scaling and the chroma offset.
    let (ys, yo, cs, co) = if full_range {
        (1.0, 0.0, 1.0, -128.0 / 255.0)
    } else {
        (255.0 / 219.0, -16.0 / 219.0, 255.0 / 224.0, -128.0 / 224.0)
    };
    let cr_r = 2.0 * (1.0 - kr);
    let cb_b = 2.0 * (1.0 - kb);
    let cb_g = -2.0 * kb * (1.0 - kb) / kg;
    let cr_g = -2.0 * kr * (1.0 - kr) / kg;
    [
        [ys, 0.0, cr_r * cs, yo + cr_r * co],
        [ys, cb_g * cs, cr_g * cs, yo + (cb_g + cr_g) * co],
        [ys, cb_b * cs, 0.0, yo + cb_b * co],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &[[f32; 4]; 3], y: u8, u: u8, v: u8) -> [f32; 3] {
        let p = [y as f32 / 255.0, u as f32 / 255.0, v as f32 / 255.0, 1.0];
        m.map(|row| row.iter().zip(p).map(|(a, b)| a * b).sum())
    }

    fn close_to(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < tol)
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        close_to(a, b, 2e-3)
    }

    #[test]
    fn video_range_black_and_white() {
        for m in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020Ncl] {
            let mx = yuv_to_rgb_matrix(m, false);
            assert!(close(apply(&mx, 16, 128, 128), [0.0; 3]), "{m:?}");
            assert!(close(apply(&mx, 235, 128, 128), [1.0; 3]), "{m:?}");
        }
    }

    #[test]
    fn full_range_black_and_white() {
        let mx = yuv_to_rgb_matrix(Matrix::Bt709, true);
        assert!(close(apply(&mx, 0, 128, 128), [0.0; 3]));
        assert!(close(apply(&mx, 255, 128, 128), [1.0; 3]));
    }

    #[test]
    fn bt709_primaries() {
        // BT.709 video-range 100 % red, green and blue, as rounded 8-bit codes.
        let mx = yuv_to_rgb_matrix(Matrix::Bt709, false);
        assert!(close_to(apply(&mx, 63, 102, 240), [1.0, 0.0, 0.0], 0.01));
        assert!(close_to(apply(&mx, 173, 42, 26), [0.0, 1.0, 0.0], 0.01));
        assert!(close_to(apply(&mx, 32, 240, 118), [0.0, 0.0, 1.0], 0.01));
    }
}
