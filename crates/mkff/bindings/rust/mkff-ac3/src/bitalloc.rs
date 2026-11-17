//! Exponent decoding (clause 6.1) and the parametric bit allocation (clause 6.2), in the
//! specification's integer arithmetic.

use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

use crate::tables::{
    BAPTAB, BNDTAB, DBPBTAB, FASTDEC, FLOORTAB, HEBAPTAB, LATAB, MASKTAB, SLOWDEC, SLOWGAIN,
};

/// Decode one exponent set: `absexp` then `ngrps` 7-bit groups of three differentials, each
/// covering `grpsize` (1, 2 or 4) bins. Writes `exp[start..]` (the absolute exponent goes to
/// `exp[start]` only when `with_abs` is set; the coupling channel's reference exponent is not a
/// coefficient exponent). Exponents outside 0..=24 are an error.
pub(crate) fn decode_exponents(
    r: &mut BitReader,
    absexp: i32,
    ngrps: usize,
    grpsize: usize,
    exp: &mut [u8; 256],
    start: usize,
    with_abs: bool,
) -> Result<()> {
    let mut prev = absexp;
    let mut pos = start;
    if with_abs {
        exp[pos] = absexp as u8;
        pos += 1;
    }
    for _ in 0..ngrps {
        let g = r.read(7)? as i32;
        if g >= 125 {
            return Err(Error::invalid("invalid exponent group"));
        }
        for d in [g / 25, (g % 25) / 5, g % 5] {
            let e = prev + d - 2;
            if !(0..=24).contains(&e) {
                return Err(Error::invalid("exponent out of range"));
            }
            prev = e;
            for _ in 0..grpsize {
                if pos < 256 {
                    exp[pos] = e as u8;
                }
                pos += 1;
            }
        }
    }
    Ok(())
}

/// Delta bit allocation of one channel (clause 6.2.2.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Dba {
    pub active: bool,
    pub nseg: usize,
    pub offst: [u8; 8],
    pub len: [u8; 8],
    pub ba: [u8; 8],
}

impl Dba {
    /// Read `deltnseg` and the segments.
    pub(crate) fn read(&mut self, r: &mut BitReader) -> Result<()> {
        self.nseg = r.read(3)? as usize + 1;
        for s in 0..self.nseg {
            self.offst[s] = r.read(5)? as u8;
            self.len[s] = r.read(4)? as u8;
            self.ba[s] = r.read(3)? as u8;
        }
        self.active = true;
        Ok(())
    }
}

/// Parameters shared by the channels of one audio block.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BaParams {
    pub sdcycod: u8,
    pub fdcycod: u8,
    pub sgaincod: u8,
    pub dbpbcod: u8,
    pub floorcod: u8,
    /// Hearing threshold for the sample rate (Table 6.15).
    pub hth: &'static [u16; 50],
}

/// Per-channel inputs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChannelBa<'a> {
    pub start: usize,
    pub end: usize,
    pub fgain: i32,
    pub snroffset: i32,
    /// Coupling channel fast / slow leak initialisation.
    pub leak: Option<(i32, i32)>,
    pub dba: &'a Dba,
    /// Use the AHT high-efficiency pointer table.
    pub hebap: bool,
}

#[inline]
fn logadd(a: i32, b: i32) -> i32 {
    let c = a - b;
    let addr = ((c.abs() >> 1) as usize).min(255);
    if c >= 0 {
        a + i32::from(LATAB[addr])
    } else {
        b + i32::from(LATAB[addr])
    }
}

#[inline]
fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        if b0 + 256 == b1 {
            384
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else if bin < 20 {
        if b0 + 256 == b1 {
            320
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else {
        (a - 128).max(0)
    }
}

/// Compute `bap[start..end]` (or `hebap`) from the exponents (clause 6.2.2 / E.2.4.3.1).
pub(crate) fn allocate(exp: &[u8; 256], p: &BaParams, c: &ChannelBa, bap: &mut [u8; 256]) {
    let (start, end) = (c.start, c.end.min(253));
    if end <= start {
        return;
    }
    let sdecay = SLOWDEC[usize::from(p.sdcycod)];
    let fdecay = FASTDEC[usize::from(p.fdcycod)];
    let sgain = SLOWGAIN[usize::from(p.sgaincod)];
    let dbknee = DBPBTAB[usize::from(p.dbpbcod)];
    let floor = FLOORTAB[usize::from(p.floorcod)];
    let fgain = c.fgain;

    let mut psd = [0i32; 256];
    for bin in start..end {
        psd[bin] = 3072 - (i32::from(exp[bin]) << 7);
    }

    // psd integration over the 1/6 octave bands
    let mut bndpsd = [0i32; 51];
    let mut j = start;
    let mut k = usize::from(MASKTAB[start]);
    loop {
        let lastbin = usize::from(BNDTAB[k + 1]).min(end);
        let mut v = psd[j];
        j += 1;
        while j < lastbin {
            v = logadd(v, psd[j]);
            j += 1;
        }
        bndpsd[k] = v;
        k += 1;
        if end <= lastbin {
            break;
        }
    }

    // excitation function
    let bndstrt = usize::from(MASKTAB[start]);
    let bndend = usize::from(MASKTAB[end - 1]) + 1;
    let mut excite = [0i32; 50];
    let (mut fastleak, mut slowleak);
    let begin;
    if bndstrt == 0 {
        let lfe_skip = |bin: usize| bndend == 7 && bin == 6;
        let mut lowcomp = calc_lowcomp(0, bndpsd[0], bndpsd[1], 0);
        excite[0] = bndpsd[0] - fgain - lowcomp;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[1], bndpsd[2], 1);
        excite[1] = bndpsd[1] - fgain - lowcomp;
        let mut b = 7;
        fastleak = 0;
        slowleak = 0;
        for bin in 2..7 {
            if !lfe_skip(bin) {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = bndpsd[bin] - fgain;
            slowleak = bndpsd[bin] - sgain;
            excite[bin] = fastleak - lowcomp;
            if !lfe_skip(bin) && bndpsd[bin] <= bndpsd[bin + 1] {
                b = bin + 1;
                break;
            }
        }
        for bin in b..bndend.min(22) {
            if !lfe_skip(bin) {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = (fastleak - fdecay).max(bndpsd[bin] - fgain);
            slowleak = (slowleak - sdecay).max(bndpsd[bin] - sgain);
            excite[bin] = (fastleak - lowcomp).max(slowleak);
        }
        begin = 22;
    } else {
        let (f, s) = c.leak.unwrap_or((0, 0));
        fastleak = f;
        slowleak = s;
        begin = bndstrt;
    }
    for bin in begin..bndend {
        fastleak = (fastleak - fdecay).max(bndpsd[bin] - fgain);
        slowleak = (slowleak - sdecay).max(bndpsd[bin] - sgain);
        excite[bin] = fastleak.max(slowleak);
    }

    // masking curve
    let mut mask = [0i32; 50];
    for bin in bndstrt..bndend {
        let mut e = excite[bin];
        if bndpsd[bin] < dbknee {
            e += (dbknee - bndpsd[bin]) >> 2;
        }
        mask[bin] = e.max(i32::from(p.hth[bin]));
    }

    // delta bit allocation
    if c.dba.active {
        let mut band = 0usize;
        for s in 0..c.dba.nseg {
            band += usize::from(c.dba.offst[s]);
            let ba = i32::from(c.dba.ba[s]);
            let delta = if ba >= 4 {
                (ba - 3) << 7
            } else {
                (ba - 4) << 7
            };
            for _ in 0..c.dba.len[s] {
                if band < 50 {
                    mask[band] += delta;
                }
                band += 1;
            }
        }
    }

    // bit allocation pointers
    let tab: &[u8; 64] = if c.hebap { &HEBAPTAB } else { &BAPTAB };
    let mut i = start;
    let mut j = usize::from(MASKTAB[start]);
    loop {
        let lastbin = usize::from(BNDTAB[j + 1]).min(end);
        let mut m = mask[j] - c.snroffset - floor;
        if m < 0 {
            m = 0;
        }
        m &= 0x1fe0;
        m += floor;
        for k in i..lastbin {
            let addr = ((psd[k] - m) >> 5).clamp(0, 63);
            bap[k] = tab[addr as usize];
        }
        i = lastbin;
        j += 1;
        if end <= lastbin {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masktab_matches_table_6_13() {
        assert_eq!(MASKTAB[0], 0);
        assert_eq!(MASKTAB[28], 28);
        assert_eq!(MASKTAB[29], 28);
        assert_eq!(MASKTAB[31], 29);
        assert_eq!(MASKTAB[49], 35);
        assert_eq!(MASKTAB[60], 36);
        assert_eq!(MASKTAB[252], 49);
    }

    #[test]
    fn exponent_groups() {
        // abs 10, group (2+1, 2-1, 2+0): +1, -1, 0
        let g = (3 * 25 + 5 + 2) as u8;
        let data = [g << 1, 0];
        let mut r = BitReader::new(&data);
        let mut exp = [0u8; 256];
        decode_exponents(&mut r, 10, 1, 2, &mut exp, 0, true).unwrap();
        assert_eq!(&exp[..7], &[10, 11, 11, 10, 10, 10, 10]);
        let data = [124 << 1];
        let mut r = BitReader::new(&data);
        assert!(decode_exponents(&mut r, 24, 1, 1, &mut exp, 0, true).is_err());
    }
}
