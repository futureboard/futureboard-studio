//! Layer III constant tables (ISO/IEC 11172-3 Annex B, ISO/IEC 13818-3 Annex B, MPEG-2.5).

/// Scale factor band boundaries for one sampling frequency.
pub(crate) struct SfBands {
    /// Long blocks: 23 boundaries (22 bands) in lines 0..576.
    pub long: [u16; 23],
    /// Short blocks: 14 boundaries (13 bands) in lines 0..192 of one window.
    pub short: [u16; 14],
}

/// Indexed by [`crate::header::sfreq_index`].
#[rustfmt::skip]
pub(crate) const SF_BANDS: [SfBands; 9] = [
    // 44.1 kHz
    SfBands {
        long: [0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 52, 62, 74, 90, 110, 134, 162, 196, 238, 288, 342, 418, 576],
        short: [0, 4, 8, 12, 16, 22, 30, 40, 52, 66, 84, 106, 136, 192],
    },
    // 48 kHz
    SfBands {
        long: [0, 4, 8, 12, 16, 20, 24, 30, 36, 42, 50, 60, 72, 88, 106, 128, 156, 190, 230, 276, 330, 384, 576],
        short: [0, 4, 8, 12, 16, 22, 28, 38, 50, 64, 80, 100, 126, 192],
    },
    // 32 kHz
    SfBands {
        long: [0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 54, 66, 82, 102, 126, 156, 194, 240, 296, 364, 448, 550, 576],
        short: [0, 4, 8, 12, 16, 22, 30, 42, 58, 78, 104, 138, 180, 192],
    },
    // 22.05 kHz
    SfBands {
        long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
        short: [0, 4, 8, 12, 18, 24, 32, 42, 56, 74, 100, 132, 174, 192],
    },
    // 24 kHz
    SfBands {
        long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 114, 136, 162, 194, 232, 278, 332, 394, 464, 540, 576],
        short: [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 136, 180, 192],
    },
    // 16 kHz
    SfBands {
        long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
        short: [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
    },
    // 11.025 kHz
    SfBands {
        long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
        short: [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
    },
    // 12 kHz
    SfBands {
        long: [0, 6, 12, 18, 24, 30, 36, 44, 54, 66, 80, 96, 116, 140, 168, 200, 238, 284, 336, 396, 464, 522, 576],
        short: [0, 4, 8, 12, 18, 26, 36, 48, 62, 80, 104, 134, 174, 192],
    },
    // 8 kHz
    SfBands {
        long: [0, 12, 24, 36, 48, 60, 72, 88, 108, 132, 160, 192, 232, 280, 336, 400, 476, 566, 568, 570, 572, 574, 576],
        short: [0, 8, 16, 24, 36, 52, 72, 96, 124, 160, 162, 164, 166, 192],
    },
];

/// Pre-emphasis added to long-block scale factors when `preflag` is set.
pub(crate) const PRETAB: [u8; 22] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 3, 2, 0,
];

/// MPEG-1 `scalefac_compress` → (slen1, slen2).
pub(crate) const SLEN: [(u8, u8); 16] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (0, 3),
    (3, 0),
    (1, 1),
    (1, 2),
    (1, 3),
    (2, 1),
    (2, 2),
    (2, 3),
    (3, 1),
    (3, 2),
    (3, 3),
    (4, 2),
    (4, 3),
];

/// MPEG-2 LSF `nr_of_sfb` per (scale factor table 0..6, block kind 0 long / 1 short / 2 mixed).
pub(crate) const NR_OF_SFB: [[[u8; 4]; 3]; 6] = [
    [[6, 5, 5, 5], [9, 9, 9, 9], [6, 9, 9, 9]],
    [[6, 5, 7, 3], [9, 9, 12, 6], [6, 9, 12, 6]],
    [[11, 10, 0, 0], [18, 18, 0, 0], [15, 18, 0, 0]],
    [[7, 7, 7, 0], [12, 12, 12, 0], [6, 15, 12, 0]],
    [[6, 6, 6, 3], [12, 9, 9, 6], [6, 12, 9, 6]],
    [[8, 8, 5, 0], [15, 12, 9, 0], [6, 18, 9, 0]],
];

/// Alias reduction coefficients `c_i` (Table B.9).
pub(crate) const ALIAS_C: [f64; 8] = [
    -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037,
];

/// (cs, ca) alias butterfly coefficients.
pub(crate) fn alias_coeffs() -> &'static ([f32; 8], [f32; 8]) {
    static T: std::sync::OnceLock<([f32; 8], [f32; 8])> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut cs = [0f32; 8];
        let mut ca = [0f32; 8];
        for i in 0..8 {
            let c = ALIAS_C[i];
            let sq = (1.0 + c * c).sqrt();
            cs[i] = (1.0 / sq) as f32;
            ca[i] = (c / sq) as f32;
        }
        (cs, ca)
    })
}

/// `|i|^(4/3)` for `i` in 0..8207.
pub(crate) fn pow43() -> &'static [f32] {
    static T: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        (0..8207)
            .map(|i| (i as f64).powf(4.0 / 3.0) as f32)
            .collect()
    })
}

/// The IMDCT windows for block types 0, 1, 3 (36 taps) and 2 (12 taps).
pub(crate) struct Windows {
    pub long: [[f32; 36]; 4],
    pub short: [f32; 12],
}

pub(crate) fn windows() -> &'static Windows {
    static W: std::sync::OnceLock<Windows> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        let pi = std::f64::consts::PI;
        let s36 = |i: usize| (pi / 36.0 * (i as f64 + 0.5)).sin() as f32;
        let s12 = |i: usize| (pi / 12.0 * (i as f64 + 0.5)).sin() as f32;
        let mut long = [[0f32; 36]; 4];
        for i in 0..36 {
            long[0][i] = s36(i);
        }
        for i in 0..36 {
            long[1][i] = match i {
                0..=17 => s36(i),
                18..=23 => 1.0,
                24..=29 => s12(i - 18),
                _ => 0.0,
            };
            long[3][i] = match i {
                0..=5 => 0.0,
                6..=11 => s12(i - 6),
                12..=17 => 1.0,
                _ => s36(i),
            };
        }
        long[2] = long[0];
        let mut short = [0f32; 12];
        for (i, v) in short.iter_mut().enumerate() {
            *v = s12(i);
        }
        Windows { long, short }
    })
}
