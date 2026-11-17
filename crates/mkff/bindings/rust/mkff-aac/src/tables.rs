//! Constant tables of ISO/IEC 14496-3 subpart 4 (scalefactor band offsets, TNS limits) and the
//! computed tables shared by the decoder and the encoder (windows, `x^(4/3)`).

use std::sync::OnceLock;

/// Sampling frequencies by `sampling_frequency_index` (0..=12).
pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// The `sampling_frequency_index` of `rate`, or `None` when it is not one of the 13 table rates.
pub fn sampling_index(rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// The table index whose frequency is nearest to `rate` (the mapping rule of 14496-3
/// 4.5.1.2.1 for explicit rates: the band tables of the nearest standard rate are used).
pub fn nearest_sampling_index(rate: u32) -> u8 {
    // thresholds of table 4.82 (lower bounds of each range)
    const BOUNDS: [u32; 12] = [
        92017, 75132, 55426, 46009, 37566, 27713, 23004, 18783, 13856, 11502, 9391, 0,
    ];
    for (i, &b) in BOUNDS.iter().enumerate() {
        if rate >= b {
            return i as u8;
        }
    }
    11
}

const SWB_LONG_96: [u16; 42] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 156, 172, 188, 212, 240, 276, 320, 384, 448, 512, 576, 640, 704, 768, 832, 896, 960, 1024,
];
const SWB_LONG_64: [u16; 48] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 100, 112, 124, 140,
    156, 172, 192, 216, 240, 268, 304, 344, 384, 424, 464, 504, 544, 584, 624, 664, 704, 744, 784,
    824, 864, 904, 944, 984, 1024,
];
const SWB_LONG_48: [u16; 50] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
];
const SWB_LONG_32: [u16; 52] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960, 992, 1024,
];
const SWB_LONG_24: [u16; 48] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108, 116, 124, 136,
    148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396, 432, 468, 508, 552, 600, 652,
    704, 768, 832, 896, 960, 1024,
];
const SWB_LONG_16: [u16; 44] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 100, 112, 124, 136, 148, 160, 172, 184, 196, 212,
    228, 244, 260, 280, 300, 320, 344, 368, 396, 424, 456, 492, 532, 572, 616, 664, 716, 772, 832,
    896, 960, 1024,
];
const SWB_LONG_8: [u16; 41] = [
    0, 12, 24, 36, 48, 60, 72, 84, 96, 108, 120, 132, 144, 156, 172, 188, 204, 220, 236, 252, 268,
    288, 308, 328, 348, 372, 396, 420, 448, 476, 508, 544, 580, 620, 664, 712, 764, 820, 880, 944,
    1024,
];

const SWB_SHORT_96: [u16; 13] = [0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128];
const SWB_SHORT_48: [u16; 15] = [0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 128];
const SWB_SHORT_24: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 128,
];
const SWB_SHORT_16: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 128,
];
const SWB_SHORT_8: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 60, 72, 88, 108, 128,
];

/// Scalefactor band offsets of long windows (1024 lines) for a sampling index.
pub fn swb_offset_long(sf_index: u8) -> &'static [u16] {
    match sf_index {
        0 | 1 => &SWB_LONG_96,
        2 => &SWB_LONG_64,
        3 | 4 => &SWB_LONG_48,
        5 => &SWB_LONG_32,
        6 | 7 => &SWB_LONG_24,
        8..=10 => &SWB_LONG_16,
        _ => &SWB_LONG_8,
    }
}

/// Scalefactor band offsets of short windows (128 lines).
pub fn swb_offset_short(sf_index: u8) -> &'static [u16] {
    match sf_index {
        0..=2 => &SWB_SHORT_96,
        3..=5 => &SWB_SHORT_48,
        6 | 7 => &SWB_SHORT_24,
        8..=10 => &SWB_SHORT_16,
        _ => &SWB_SHORT_8,
    }
}

/// `TNS_MAX_BANDS` for AAC-LC long windows (table 4.156).
const TNS_MAX_BANDS_LONG: [u8; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];
/// `TNS_MAX_BANDS` for AAC-LC short windows.
const TNS_MAX_BANDS_SHORT: [u8; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];

pub fn tns_max_bands(sf_index: u8, short: bool) -> usize {
    let i = usize::from(sf_index.min(12));
    usize::from(if short {
        TNS_MAX_BANDS_SHORT[i]
    } else {
        TNS_MAX_BANDS_LONG[i]
    })
}

/// `TNS_MAX_ORDER` for AAC-LC.
pub const TNS_MAX_ORDER_LONG: usize = 12;
pub const TNS_MAX_ORDER_SHORT: usize = 7;

/// Window sequences (`window_sequence`).
pub const ONLY_LONG: u8 = 0;
pub const LONG_START: u8 = 1;
pub const EIGHT_SHORT: u8 = 2;
pub const LONG_STOP: u8 = 3;

/// Special codebooks.
pub const ZERO_HCB: u8 = 0;
pub const ESC_HCB: u8 = 11;
pub const NOISE_HCB: u8 = 13;
pub const INTENSITY_HCB2: u8 = 14;
pub const INTENSITY_HCB: u8 = 15;

/// Computed tables.
pub struct Shared {
    /// `|q|^(4/3)` for `q` in 0..8192.
    pub pow43: Vec<f32>,
    /// Rising halves of the windows: `[shape][n]`, shape 0 = sine, 1 = KBD.
    pub long_win: [Vec<f32>; 2],
    pub short_win: [Vec<f32>; 2],
}

pub fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| Shared {
        pow43: (0..8192)
            .map(|i| (i as f64).powf(4.0 / 3.0) as f32)
            .collect(),
        long_win: [sine_half(2048), kbd_half(2048, 4.0)],
        short_win: [sine_half(256), kbd_half(256, 6.0)],
    })
}

fn sine_half(n: usize) -> Vec<f32> {
    (0..n / 2)
        .map(|i| (std::f64::consts::PI * (i as f64 + 0.5) / n as f64).sin() as f32)
        .collect()
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..100 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

/// Rising half of the Kaiser-Bessel-derived window of length `n` (14496-3 4.6.11.3.2).
fn kbd_half(n: usize, alpha: f64) -> Vec<f32> {
    let half = n / 2;
    let quarter = n as f64 / 4.0;
    let w: Vec<f64> = (0..=half)
        .map(|p| {
            let r = (p as f64 - quarter) / quarter;
            bessel_i0(std::f64::consts::PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = w.iter().sum();
    let mut acc = 0.0;
    (0..half)
        .map(|i| {
            acc += w[i];
            (acc / total).sqrt() as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_tables_are_consistent() {
        let counts = [41, 41, 47, 49, 49, 51, 47, 47, 43, 43, 43, 40, 40];
        let short_counts = [12, 12, 12, 14, 14, 14, 15, 15, 15, 15, 15, 15, 15];
        for i in 0..13u8 {
            let l = swb_offset_long(i);
            assert_eq!(l.len() - 1, counts[i as usize]);
            assert_eq!(*l.last().unwrap(), 1024);
            assert!(l.windows(2).all(|w| w[0] < w[1] && (w[1] - w[0]) % 4 == 0));
            let s = swb_offset_short(i);
            assert_eq!(s.len() - 1, short_counts[i as usize]);
            assert_eq!(*s.last().unwrap(), 128);
            assert!(s.windows(2).all(|w| w[0] < w[1] && (w[1] - w[0]) % 4 == 0));
        }
    }

    #[test]
    fn windows_are_princen_bradley() {
        let s = shared();
        for shape in 0..2 {
            for w in [&s.long_win[shape], &s.short_win[shape]] {
                let n = w.len();
                for i in 0..n {
                    let a = w[i];
                    let b = w[n - 1 - i];
                    assert!((a * a + b * b - 1.0).abs() < 1e-5);
                }
            }
        }
    }
}
