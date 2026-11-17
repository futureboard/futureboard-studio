//! Constant tables of ATSC A/52 / ETSI TS 102 366 (clause 4, clause 6 and Annex E).

/// Sample rates for `fscod` 0..=2 (Table 4.1).
pub(crate) const SAMPLE_RATES: [u32; 3] = [48_000, 44_100, 32_000];

/// Nominal bit rates (kbit/s) for `frmsizecod >> 1` (Table 4.13).
pub(crate) const BIT_RATES: [u32; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

/// Syncframe size in 16-bit words for `fscod` and `frmsizecod` (Table 4.13).
pub(crate) fn ac3_frame_words(fscod: u8, frmsizecod: u8) -> Option<usize> {
    let kbps = *BIT_RATES.get(usize::from(frmsizecod >> 1))? as usize;
    match fscod {
        0 => Some(kbps * 2),
        // 1536 samples per frame at 44.1 kHz: kbps * 1536 / 44.1 / 16 words, rounded down, plus
        // one word in every second frame size code
        1 => Some(kbps * 320 / 147 + usize::from(frmsizecod & 1)),
        2 => Some(kbps * 3),
        _ => None,
    }
}

/// Slow decay table `slowdec[]` (Table 6.6).
pub(crate) const SLOWDEC: [i32; 4] = [0x0f, 0x11, 0x13, 0x15];
/// Fast decay table `fastdec[]` (Table 6.7).
pub(crate) const FASTDEC: [i32; 4] = [0x3f, 0x53, 0x67, 0x7b];
/// Slow gain table `slowgain[]` (Table 6.8).
pub(crate) const SLOWGAIN: [i32; 4] = [0x540, 0x4d8, 0x478, 0x410];
/// dB per bit table `dbpbtab[]` (Table 6.9).
pub(crate) const DBPBTAB: [i32; 4] = [0x000, 0x700, 0x900, 0xb00];
/// Masking floor table `floortab[]` (Table 6.10; the last entry is -2048).
pub(crate) const FLOORTAB: [i32; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -0x800];
/// Fast gain table `fastgain[]` (Table 6.11).
pub(crate) const FASTGAIN: [i32; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

/// First mantissa of each of the 50 bit allocation bands, plus the end (Table 6.12).
pub(crate) const BNDTAB: [u16; 51] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 31, 34, 37, 40, 43, 46, 49, 55, 61, 67, 73, 79, 85, 97, 109, 121, 133, 157, 181,
    205, 229, 253,
];

/// Mantissa number to bit allocation band (Table 6.13), derived from [`BNDTAB`].
pub(crate) const MASKTAB: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut band = 0;
    let mut bin = 0;
    while bin < 256 {
        while band < 49 && bin >= BNDTAB[band + 1] as usize {
            band += 1;
        }
        t[bin] = band as u8;
        bin += 1;
    }
    t
};

pub(crate) const LATAB: [u16; 256] = [
    64, 63, 62, 61, 60, 59, 58, 57, 56, 55, 54, 53, 52, 52, 51, 50, 49, 48, 47, 47, 46, 45, 44, 44,
    43, 42, 41, 41, 40, 39, 38, 38, 37, 36, 36, 35, 35, 34, 33, 33, 32, 32, 31, 30, 30, 29, 29, 28,
    28, 27, 27, 26, 26, 25, 25, 24, 24, 23, 23, 22, 22, 21, 21, 21, 20, 20, 19, 19, 19, 18, 18, 18,
    17, 17, 17, 16, 16, 16, 15, 15, 15, 14, 14, 14, 13, 13, 13, 13, 12, 12, 12, 12, 11, 11, 11, 11,
    10, 10, 10, 10, 10, 9, 9, 9, 9, 9, 8, 8, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 6,
    5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0,
];
pub(crate) const HTH: [[u16; 50]; 3] = [
    [
        1232, 1232, 1088, 1024, 992, 960, 944, 944, 928, 928, 928, 928, 928, 912, 912, 912, 896,
        896, 880, 880, 864, 864, 848, 848, 832, 832, 816, 800, 784, 768, 752, 752, 752, 752, 768,
        784, 832, 912, 992, 1056, 1120, 1168, 1184, 1120, 1088, 1088, 1312, 2048, 2112, 2112,
    ],
    [
        1264, 1264, 1120, 1040, 992, 976, 960, 944, 944, 928, 928, 928, 928, 928, 912, 912, 912,
        896, 896, 896, 880, 880, 864, 864, 848, 848, 832, 832, 800, 784, 768, 752, 752, 752, 752,
        768, 800, 848, 912, 992, 1056, 1104, 1184, 1168, 1120, 1088, 1152, 1584, 2112, 2112,
    ],
    [
        1408, 1408, 1200, 1104, 1056, 1008, 992, 976, 960, 944, 944, 944, 928, 928, 928, 928, 928,
        928, 928, 928, 912, 912, 912, 912, 896, 896, 896, 880, 864, 848, 832, 816, 800, 784, 768,
        752, 752, 752, 768, 784, 816, 848, 960, 1040, 1136, 1184, 1120, 1088, 1104, 1248,
    ],
];
pub(crate) const BAPTAB: [u8; 64] = [
    0, 1, 1, 1, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9,
    10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14, 14, 14, 14, 14, 14, 14,
    15, 15, 15, 15, 15, 15, 15, 15, 15,
];

/// High-efficiency bit allocation pointers for AHT channels (Table E.2.1).
pub(crate) const HEBAPTAB: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 8, 8, 9, 9, 9, 10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12,
    13, 13, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18, 18, 18,
    18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];

/// Index bits of the vector quantizers for `hebap` 1..=7 (Table E.2.2).
pub(crate) const VQ_BITS: [u32; 8] = [0, 2, 3, 4, 5, 7, 8, 9];

/// Mantissa bits `m` of the scalar / GAQ quantizers for `hebap` 8..=19 (Table E.2.2).
pub(crate) const GAQ_BITS: [u32; 12] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

/// Large mantissa remapping constants `(a, b(x >= 0), b(x < 0))` for `hebap` 8..=19 and the gains
/// 1, 2 and 4, as 16-bit fractions (Table E.2.6). Gains 2 and 4 only apply up to `hebap` 16.
pub(crate) const GAQ_REMAP: [[(i16, i16, i16); 3]; 12] = {
    const fn s(v: u16) -> i16 {
        v as i16
    }
    const fn row(a1: u16, a2: u16, b2n: u16, a4: u16, b4n: u16) -> [(i16, i16, i16); 3] {
        [
            (s(a1), 0, 0),
            (s(a2), 0x4000, s(b2n)),
            (s(a4), 0x2000, s(b4n)),
        ]
    }
    [
        row(0x1249, 0xd555, 0xeaab, 0xedb7, 0xfb6e),
        row(0x0889, 0xc925, 0xd249, 0xe666, 0xeccd),
        row(0x0421, 0xc444, 0xc889, 0xe319, 0xe632),
        row(0x0208, 0xc211, 0xc421, 0xe186, 0xe30c),
        row(0x0102, 0xc104, 0xc208, 0xe0c2, 0xe183),
        row(0x0081, 0xc081, 0xc102, 0xe060, 0xe0c1),
        row(0x0040, 0xc040, 0xc081, 0xe030, 0xe060),
        row(0x0020, 0xc020, 0xc040, 0xe018, 0xe030),
        row(0x0010, 0xc010, 0xc020, 0xe00c, 0xe018),
        row(0x0008, 0, 0, 0, 0),
        row(0x0002, 0, 0, 0, 0),
        row(0x0000, 0, 0, 0, 0),
    ]
};

/// Exponent strategy codes.
pub(crate) const EXP_REUSE: u8 = 0;
pub(crate) const EXP_D15: u8 = 1;
pub(crate) const EXP_D25: u8 = 2;
pub(crate) const EXP_D45: u8 = 3;

/// Frame exponent strategy combinations for `frmcplexpstr` / `frmchexpstr` (Table E.1.9).
pub(crate) const FRM_EXPSTR: [[u8; 6]; 32] = {
    const R: u8 = EXP_REUSE;
    const A: u8 = EXP_D15;
    const B: u8 = EXP_D25;
    const C: u8 = EXP_D45;
    [
        [A, R, R, R, R, R],
        [A, R, R, R, R, C],
        [A, R, R, R, B, R],
        [A, R, R, R, C, C],
        [B, R, R, B, R, R],
        [B, R, R, B, R, C],
        [B, R, R, C, B, R],
        [B, R, R, C, C, C],
        [B, R, A, R, R, R],
        [B, R, B, R, R, C],
        [B, R, B, R, B, R],
        [B, R, B, R, C, C],
        [B, R, C, B, R, R],
        [B, R, C, B, R, C],
        [B, R, C, C, B, R],
        [B, R, C, C, C, C],
        [C, A, R, R, R, R],
        [C, A, R, R, R, C],
        [C, B, R, R, B, R],
        [C, B, R, R, C, C],
        [C, B, R, B, R, R],
        [C, B, R, B, R, C],
        [C, B, R, C, B, R],
        [C, B, R, C, C, C],
        [C, C, A, R, R, R],
        [C, C, B, R, R, C],
        [C, C, B, R, B, R],
        [C, C, B, R, C, C],
        [C, C, C, B, R, R],
        [C, C, C, B, R, C],
        [C, C, C, C, B, R],
        [C, C, C, C, C, C],
    ]
};

/// Default coupling band structure (Table E.1.12).
pub(crate) const DEFAULT_CPL_BNDSTRC: [bool; 18] = {
    const O: bool = false;
    const I: bool = true;
    [O, O, O, O, O, O, O, O, I, O, I, I, O, I, I, I, I, I]
};

/// Default spectral extension band structure (Table E.1.10).
pub(crate) const DEFAULT_SPX_BNDSTRC: [bool; 17] = {
    const O: bool = false;
    const I: bool = true;
    [O, O, O, O, O, O, O, O, I, O, I, O, I, O, I, O, I]
};

/// First transform coefficient of spectral extension sub-band `n` (Table E.2.11).
pub(crate) const fn spx_band_start(n: usize) -> usize {
    25 + 12 * n
}

/// Rematrixing band edges (Tables 6.25 to 6.28; the last band ends where coupling or spectral
/// extension begins, or at the lower channel bandwidth).
pub(crate) const REMAT_BANDS: [usize; 5] = [13, 25, 37, 61, 253];

/// Centre mix levels for `cmixlev` (Table 4.4; the reserved code uses -4.5 dB).
pub(crate) const CMIXLEV: [f32; 4] = [0.707_106_77, 0.594_603_55, 0.5, 0.594_603_55];
/// Surround mix levels for `surmixlev` (Table 4.5; the reserved code uses -6 dB).
pub(crate) const SURMIXLEV: [f32; 4] = [0.707_106_77, 0.5, 0.0, 0.5];
/// Lo/Ro centre mix levels (Table D.1.4).
pub(crate) const LORO_CMIXLEV: [f32; 8] = [
    std::f32::consts::SQRT_2,
    1.189_207_1,
    1.0,
    0.840_896_4,
    0.707_106_77,
    0.594_603_55,
    0.5,
    0.0,
];
/// Lo/Ro surround mix levels (Table D.1.5; the reserved codes use 0.841).
pub(crate) const LORO_SURMIXLEV: [f32; 8] = [
    0.840_896_4,
    0.840_896_4,
    0.840_896_4,
    0.840_896_4,
    0.707_106_77,
    0.594_603_55,
    0.5,
    0.0,
];
