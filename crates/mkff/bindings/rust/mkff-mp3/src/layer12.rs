//! Layer I and Layer II decoding (ISO/IEC 11172-3 2.4.3.2 / 2.4.3.3, ISO/IEC 13818-3 for the
//! low sampling frequencies).

use crate::header::{ChannelMode, FrameHeader, MpegVersion};
use crate::synth::Synth;
use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

/// Scale factor `i` of Table B.1: `2^(1 - i/3)`.
fn scalefactors() -> &'static [f32; 64] {
    static T: std::sync::OnceLock<[f32; 64]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0f32; 64];
        for (i, v) in t.iter_mut().enumerate().take(63) {
            *v = 2f64.powf(1.0 - i as f64 / 3.0) as f32;
        }
        t
    })
}

/// One row of a Layer II allocation table: subband count with this layout, `nbal`, and the
/// number of quantization levels per allocation code (0 = not transmitted).
struct AllocRow {
    count: usize,
    nbal: u32,
    levels: &'static [u32],
}

const L_A0: [u32; 16] = [
    0, 3, 7, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535,
];
const L_A1: [u32; 16] = [
    0, 3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 65535,
];
const L_A2: [u32; 8] = [0, 3, 5, 7, 9, 15, 31, 65535];
const L_A3: [u32; 4] = [0, 3, 5, 65535];
const L_C0: [u32; 16] = [
    0, 3, 5, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767,
];
const L_C1: [u32; 8] = [0, 3, 5, 9, 15, 31, 63, 127];
const L_L0: [u32; 16] = [
    0, 3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383,
];
const L_L1: [u32; 8] = [0, 3, 5, 9, 15, 31, 63, 127];
const L_L2: [u32; 4] = [0, 3, 5, 9];

const fn row(count: usize, nbal: u32, levels: &'static [u32]) -> AllocRow {
    AllocRow {
        count,
        nbal,
        levels,
    }
}

/// Table B.2a (27 subbands).
const TABLE_A: [AllocRow; 4] = [
    row(3, 4, &L_A0),
    row(8, 4, &L_A1),
    row(12, 3, &L_A2),
    row(4, 2, &L_A3),
];
/// Table B.2b (30 subbands).
const TABLE_B: [AllocRow; 4] = [
    row(3, 4, &L_A0),
    row(8, 4, &L_A1),
    row(12, 3, &L_A2),
    row(7, 2, &L_A3),
];
/// Table B.2c (8 subbands).
const TABLE_C: [AllocRow; 2] = [row(2, 4, &L_C0), row(6, 3, &L_C1)];
/// Table B.2d (12 subbands).
const TABLE_D: [AllocRow; 2] = [row(2, 4, &L_C0), row(10, 3, &L_C1)];
/// ISO/IEC 13818-3 Table B.1 (low sampling frequencies, 30 subbands).
const TABLE_LSF: [AllocRow; 3] = [row(4, 4, &L_L0), row(7, 3, &L_L1), row(19, 2, &L_L2)];

fn alloc_table(h: &FrameHeader) -> &'static [AllocRow] {
    if h.version != MpegVersion::Mpeg1 {
        return &TABLE_LSF;
    }
    let per_ch = if h.bitrate_kbps == 0 {
        1000
    } else {
        h.bitrate_kbps / h.channels as u32
    };
    match (h.sample_rate, per_ch) {
        (48000, 32 | 48) | (44100, 32 | 48) => &TABLE_C,
        (32000, 32 | 48) => &TABLE_D,
        (48000, _) => &TABLE_A,
        (_, 56..=80) => &TABLE_A,
        _ => &TABLE_B,
    }
}

/// (levels, grouped, bits per code) for a level count.
#[inline]
fn quant_class(levels: u32) -> (bool, u32) {
    match levels {
        3 => (true, 5),
        5 => (true, 7),
        9 => (true, 10),
        n => (false, 32 - n.leading_zeros()),
    }
}

/// CRC-16 (polynomial 0x8005) over `nbits` bits of `data` starting at bit `start`.
fn crc_bits(mut crc: u16, data: &[u8], start: usize, nbits: usize) -> u16 {
    for i in start..start + nbits {
        let bit = (data.get(i / 8).copied().unwrap_or(0) >> (7 - i % 8)) & 1;
        let top = (crc >> 15) as u8 & 1;
        crc <<= 1;
        if top ^ bit != 0 {
            crc ^= 0x8005;
        }
    }
    crc
}

fn check_crc(frame: &[u8], protected_end_bit: usize) -> Result<()> {
    // header bytes 2..4, then the protected bits after the CRC word
    let c = crc_bits(0xFFFF, frame, 16, 16);
    let c = crc_bits(c, frame, 48, protected_end_bit.saturating_sub(48));
    if frame.len() < 6 || c != u16::from_be_bytes([frame[4], frame[5]]) {
        return Err(Error::invalid("layer I/II CRC mismatch"));
    }
    Ok(())
}

fn joint_bound(h: &FrameHeader, limit: usize) -> usize {
    if h.channel_mode == ChannelMode::JointStereo {
        ((usize::from(h.mode_extension) + 1) * 4).min(limit)
    } else {
        limit
    }
}

/// Decode a Layer I frame into `pcm` (384 samples per channel).
pub(crate) fn decode_layer1(
    h: &FrameHeader,
    frame: &[u8],
    crc_check: bool,
    synth: &mut [Synth; 2],
    pcm: &mut [Vec<f32>; 2],
) -> Result<()> {
    let nch = h.channels;
    let mut r = BitReader::new(frame);
    r.skip(32 + if h.protection { 16 } else { 0 })?;
    let bound = joint_bound(h, 32);
    let mut alloc = [[0u8; 32]; 2];
    for sb in 0..32 {
        let n = if sb < bound { nch } else { 1 };
        for ch in 0..n {
            let a = r.read(4)? as u8;
            if a == 15 {
                return Err(Error::invalid("layer I bit allocation 15"));
            }
            alloc[ch][sb] = a;
        }
        if sb >= bound {
            alloc[1][sb] = alloc[0][sb];
        }
    }
    if crc_check && h.protection {
        check_crc(frame, r.position())?;
    }
    let sft = scalefactors();
    let mut sf = [[0f32; 32]; 2];
    for sb in 0..32 {
        for ch in 0..nch {
            if alloc[ch][sb] != 0 {
                sf[ch][sb] = sft[r.read(6)? as usize];
            }
        }
    }
    let mut sbs = [[0f32; 32]; 2];
    for s in 0..12 {
        for sb in 0..32 {
            let n = if sb < bound { nch } else { 1 };
            for ch in 0..n {
                let a = alloc[ch][sb];
                let v = if a != 0 {
                    let nb = u32::from(a) + 1;
                    let levels = (1u32 << nb) - 1;
                    let code = r.read(nb)?;
                    (2 * code as i32 + 1 - levels as i32) as f32 / levels as f32
                } else {
                    0.0
                };
                sbs[ch][sb] = v * sf[ch][sb];
                if sb >= bound && nch == 2 {
                    sbs[1][sb] = v * sf[1][sb];
                }
            }
        }
        for ch in 0..nch {
            synth[ch].process(&sbs[ch], &mut pcm[ch][s * 32..s * 32 + 32]);
        }
    }
    Ok(())
}

/// Decode a Layer II frame into `pcm` (1152 samples per channel).
pub(crate) fn decode_layer2(
    h: &FrameHeader,
    frame: &[u8],
    crc_check: bool,
    synth: &mut [Synth; 2],
    pcm: &mut [Vec<f32>; 2],
) -> Result<()> {
    let nch = h.channels;
    let table = alloc_table(h);
    let sblimit: usize = table.iter().map(|r| r.count).sum();
    let bound = joint_bound(h, sblimit);
    let mut r = BitReader::new(frame);
    r.skip(32 + if h.protection { 16 } else { 0 })?;
    // per subband: levels of each channel
    let mut levels = [[0u32; 32]; 2];
    let mut sb = 0;
    for row in table {
        for _ in 0..row.count {
            let n = if sb < bound { nch } else { 1 };
            for ch in 0..n {
                levels[ch][sb] = row.levels[r.read(row.nbal)? as usize];
            }
            if sb >= bound {
                levels[1][sb] = levels[0][sb];
            }
            sb += 1;
        }
    }
    let mut scfsi = [[0u8; 32]; 2];
    for sb in 0..sblimit {
        for ch in 0..nch {
            if levels[ch][sb] != 0 {
                scfsi[ch][sb] = r.read(2)? as u8;
            }
        }
    }
    if crc_check && h.protection {
        check_crc(frame, r.position())?;
    }
    let sft = scalefactors();
    let mut sf = [[[0f32; 3]; 32]; 2];
    for sb in 0..sblimit {
        for ch in 0..nch {
            if levels[ch][sb] == 0 {
                continue;
            }
            let mut rd = || -> Result<f32> { Ok(sft[r.read(6)? as usize]) };
            sf[ch][sb] = match scfsi[ch][sb] {
                0 => [rd()?, rd()?, rd()?],
                1 => {
                    let a = rd()?;
                    let b = rd()?;
                    [a, a, b]
                }
                2 => {
                    let a = rd()?;
                    [a, a, a]
                }
                _ => {
                    let a = rd()?;
                    let b = rd()?;
                    [a, b, b]
                }
            };
        }
    }
    let mut sbs = [[[0f32; 32]; 3]; 2];
    for gr in 0..12 {
        let part = gr / 4;
        for sb in 0..sblimit {
            let n = if sb < bound { nch } else { 1 };
            for ch in 0..n {
                let lv = levels[ch][sb];
                let mut v = [0f32; 3];
                if lv != 0 {
                    let (grouped, bits) = quant_class(lv);
                    let mut codes = [0u32; 3];
                    if grouped {
                        let mut c = r.read(bits)?;
                        for code in &mut codes {
                            *code = c % lv;
                            c /= lv;
                        }
                    } else {
                        for code in &mut codes {
                            *code = r.read(bits)?;
                        }
                    }
                    for (x, &c) in v.iter_mut().zip(&codes) {
                        *x = (2.0 * c as f32 + 1.0 - lv as f32) / lv as f32;
                    }
                }
                for (s, &x) in v.iter().enumerate() {
                    sbs[ch][s][sb] = x * sf[ch][sb][part];
                    if sb >= bound && nch == 2 {
                        sbs[1][s][sb] = x * sf[1][sb][part];
                    }
                }
            }
        }
        for ch in 0..nch {
            for s in 0..3 {
                let o = (gr * 3 + s) * 32;
                synth[ch].process(&sbs[ch][s], &mut pcm[ch][o..o + 32]);
            }
        }
    }
    Ok(())
}
