//! Huffman decoding (table-driven, with a canonical-code fallback for long codewords) and the
//! codeword / length access the encoder needs.

use crate::huffman_tables::*;
use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};
use std::sync::OnceLock;

const LUT_BITS: u32 = 10;
const MAX_LEN: usize = 20;

/// A decoder for one codebook.
pub(crate) struct Codebook {
    /// `(symbol, length)` for every `LUT_BITS`-bit prefix; length 0 means "longer codeword".
    lut: Vec<(u16, u8)>,
    /// Canonical decoding of long codewords: per length, the first codeword, the count and the
    /// offset into `syms`.
    first: [u32; MAX_LEN + 1],
    count: [u32; MAX_LEN + 1],
    offset: [u32; MAX_LEN + 1],
    syms: Vec<u16>,
    max_len: u32,
}

impl Codebook {
    fn new(codes: &[u32], bits: &[u8]) -> Self {
        let mut lut = vec![(0u16, 0u8); 1 << LUT_BITS];
        let mut first = [u32::MAX; MAX_LEN + 1];
        let mut count = [0u32; MAX_LEN + 1];
        let mut offset = [0u32; MAX_LEN + 1];
        let max_len = u32::from(*bits.iter().max().unwrap_or(&1));
        // symbols sorted by (length, codeword)
        let mut order: Vec<usize> = (0..codes.len()).collect();
        order.sort_by_key(|&i| (bits[i], codes[i]));
        let syms: Vec<u16> = order.iter().map(|&i| i as u16).collect();
        for (pos, &i) in order.iter().enumerate() {
            let l = usize::from(bits[i]);
            if count[l] == 0 {
                first[l] = codes[i];
                offset[l] = pos as u32;
            }
            count[l] += 1;
            let len = u32::from(bits[i]);
            if len <= LUT_BITS {
                let base = (codes[i] << (LUT_BITS - len)) as usize;
                for e in &mut lut[base..base + (1 << (LUT_BITS - len))] {
                    *e = (i as u16, bits[i]);
                }
            }
        }
        Self {
            lut,
            first,
            count,
            offset,
            syms,
            max_len,
        }
    }

    /// Decode one symbol (codebook index).
    #[inline]
    pub(crate) fn decode(&self, br: &mut BitReader) -> Result<usize> {
        let (sym, len) = self.lut[br.peek(LUT_BITS) as usize];
        if len != 0 {
            br.skip(usize::from(len))?;
            return Ok(usize::from(sym));
        }
        for l in LUT_BITS + 1..=self.max_len {
            let li = l as usize;
            if self.count[li] == 0 {
                continue;
            }
            let code = br.peek(l);
            if code >= self.first[li] && code - self.first[li] < self.count[li] {
                br.skip(li)?;
                return Ok(usize::from(
                    self.syms[(self.offset[li] + code - self.first[li]) as usize],
                ));
            }
        }
        Err(Error::invalid("invalid Huffman codeword"))
    }
}

/// Spectral codebook description.
pub(crate) struct SpectralBook {
    pub book: Codebook,
    /// Unpacked values of each codebook index (4 for quads, the first 2 used for pairs).
    pub values: Vec<[i8; 4]>,
    pub quad: bool,
    pub signed: bool,
}

pub(crate) struct Books {
    pub sf: Codebook,
    /// Spectral codebooks 1..=11 at indices 0..=10.
    pub spec: Vec<SpectralBook>,
}

/// Codebook parameters: (quad, signed, lav).
pub(crate) const BOOK_PARAMS: [(bool, bool, i32); 11] = [
    (true, true, 1),
    (true, true, 1),
    (true, false, 2),
    (true, false, 2),
    (false, true, 4),
    (false, true, 4),
    (false, false, 7),
    (false, false, 7),
    (false, false, 12),
    (false, false, 12),
    (false, false, 16),
];

/// Codewords and lengths of spectral codebook `cb` (1..=11).
pub(crate) fn spectral_codes(cb: usize) -> (&'static [u16], &'static [u8]) {
    match cb {
        1 => (&CB1_CODES, &CB1_BITS),
        2 => (&CB2_CODES, &CB2_BITS),
        3 => (&CB3_CODES, &CB3_BITS),
        4 => (&CB4_CODES, &CB4_BITS),
        5 => (&CB5_CODES, &CB5_BITS),
        6 => (&CB6_CODES, &CB6_BITS),
        7 => (&CB7_CODES, &CB7_BITS),
        8 => (&CB8_CODES, &CB8_BITS),
        9 => (&CB9_CODES, &CB9_BITS),
        10 => (&CB10_CODES, &CB10_BITS),
        _ => (&CB11_CODES, &CB11_BITS),
    }
}

/// Codebook index of a quad / pair (the inverse of [`SpectralBook::values`]); unsigned books
/// take absolute values.
#[inline]
pub(crate) fn book_index(cb: usize, v: &[i32]) -> usize {
    let (quad, signed, lav) = BOOK_PARAMS[cb - 1];
    if quad {
        if signed {
            ((v[0] + 1) * 27 + (v[1] + 1) * 9 + (v[2] + 1) * 3 + (v[3] + 1)) as usize
        } else {
            (v[0].abs() * 27 + v[1].abs() * 9 + v[2].abs() * 3 + v[3].abs()) as usize
        }
    } else if signed {
        ((v[0] + 4) * 9 + (v[1] + 4)) as usize
    } else {
        let m = lav + 1;
        (v[0].abs().min(16) * m + v[1].abs().min(16)) as usize
    }
}

pub(crate) fn books() -> &'static Books {
    static B: OnceLock<Books> = OnceLock::new();
    B.get_or_init(|| {
        let sf = Codebook::new(&SF_CODES, &SF_BITS);
        let spec = (1..=11)
            .map(|cb| {
                let (codes, bits) = spectral_codes(cb);
                let codes32: Vec<u32> = codes.iter().map(|&c| u32::from(c)).collect();
                let (quad, signed, lav) = BOOK_PARAMS[cb - 1];
                let values = (0..codes.len())
                    .map(|i| {
                        let i = i as i32;
                        if quad {
                            let (off, m) = if signed { (1, 3) } else { (0, 3) };
                            [
                                (i / 27 - off) as i8,
                                ((i / 9) % m - off) as i8,
                                ((i / 3) % m - off) as i8,
                                (i % m - off) as i8,
                            ]
                        } else if signed {
                            [(i / 9 - 4) as i8, (i % 9 - 4) as i8, 0, 0]
                        } else {
                            let m = lav + 1;
                            [(i / m) as i8, (i % m) as i8, 0, 0]
                        }
                    })
                    .collect();
                SpectralBook {
                    book: Codebook::new(&codes32, bits),
                    values,
                    quad,
                    signed,
                }
            })
            .collect();
        Books { sf, spec }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mkff_audio_core::bits::BitWriter;

    fn check_prefix_complete(codes: &[u32], bits: &[u8]) {
        let kraft: f64 = bits.iter().map(|&b| 0.5f64.powi(i32::from(b))).sum();
        assert!((kraft - 1.0).abs() < 1e-12, "Kraft sum {kraft}");
        for i in 0..codes.len() {
            assert!(codes[i] >> bits[i] == 0);
            for j in 0..codes.len() {
                if i != j && bits[i] <= bits[j] {
                    assert_ne!(codes[j] >> (bits[j] - bits[i]), codes[i], "prefix {i} {j}");
                }
            }
        }
    }

    #[test]
    fn codebooks_are_complete_prefix_codes() {
        check_prefix_complete(&SF_CODES, &SF_BITS);
        for cb in 1..=11 {
            let (c, b) = spectral_codes(cb);
            let c32: Vec<u32> = c.iter().map(|&x| u32::from(x)).collect();
            check_prefix_complete(&c32, b);
        }
    }

    #[test]
    fn every_symbol_round_trips() {
        let b = books();
        for cb in 1..=11 {
            let (codes, bits) = spectral_codes(cb);
            let mut w = BitWriter::new();
            for i in 0..codes.len() {
                w.write(u32::from(codes[i]), u32::from(bits[i]));
            }
            let data = w.finish();
            let mut r = BitReader::new(&data);
            for i in 0..codes.len() {
                assert_eq!(b.spec[cb - 1].book.decode(&mut r).unwrap(), i);
                let v = b.spec[cb - 1].values[i];
                let vi = [
                    i32::from(v[0]),
                    i32::from(v[1]),
                    i32::from(v[2]),
                    i32::from(v[3]),
                ];
                assert_eq!(book_index(cb, &vi), i, "cb {cb}");
            }
        }
        let mut w = BitWriter::new();
        for i in 0..121 {
            w.write(SF_CODES[i], u32::from(SF_BITS[i]));
        }
        let data = w.finish();
        let mut r = BitReader::new(&data);
        for i in 0..121 {
            assert_eq!(b.sf.decode(&mut r).unwrap(), i);
        }
    }
}
