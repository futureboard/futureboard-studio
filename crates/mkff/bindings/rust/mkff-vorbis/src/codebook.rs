//! Vorbis codebooks (spec §3): codeword lengths, the Huffman decode tables built from them and
//! the optional VQ lookup table.

use crate::bitread::{ilog, BitReader};
use mkff_audio_core::{Error, Result};

/// Number of bits resolved by the direct lookup table; longer codewords fall back to a binary
/// search over the long codes.
const FAST_BITS: u32 = 10;

/// Codebooks with more entries, or more `entries * dimensions` VQ values, are refused. The
/// specification allows 2^24 entries, but no real encoder comes close; the limits keep a few
/// malicious header bytes from allocating gigabytes.
pub(crate) const MAX_ENTRIES: usize = 1 << 20;
pub(crate) const MAX_VQ_VALUES: usize = 1 << 22;

#[derive(Clone, Debug)]
enum Huffman {
    /// No entry has a codeword: any decode from this book is an error.
    Empty,
    /// Exactly one used entry: decoding consumes its codeword length and returns it.
    Single { entry: u32, len: u32 },
    Table {
        /// Indexed by the next `fast_bits` stream bits: `entry << 8 | length`, 0 = not a short code.
        fast: Vec<u32>,
        fast_bits: u32,
        /// Codewords longer than `fast_bits`, MSB-aligned to 32 bits, sorted ascending.
        long_codes: Vec<u32>,
        /// `entry << 8 | length` for each of `long_codes`.
        long_info: Vec<u32>,
    },
}

/// One decoded codebook.
#[derive(Clone, Debug)]
pub(crate) struct Codebook {
    pub dimensions: usize,
    /// `entries * dimensions` VQ values (entry-major), present for lookup types 1 and 2.
    pub vq: Option<Vec<f32>>,
    huffman: Huffman,
}

fn float32_unpack(x: u32) -> f32 {
    let mantissa = f64::from(x & 0x1f_ffff);
    let exponent = ((x & 0x7fe0_0000) >> 21) as i32;
    let v = mantissa * 2f64.powi(exponent - 788);
    (if x & 0x8000_0000 != 0 { -v } else { v }) as f32
}

/// The greatest `r` with `r^dims <= entries` (spec §9.2.3).
fn lookup1_values(entries: usize, dims: usize) -> usize {
    if dims == 0 {
        return 0;
    }
    let fits = |r: usize| -> bool {
        let mut acc: u128 = 1;
        for _ in 0..dims {
            acc *= r as u128;
            if acc > entries as u128 {
                return false;
            }
        }
        true
    };
    let mut r = (entries as f64).powf(1.0 / dims as f64).floor() as usize;
    while r > 0 && !fits(r) {
        r -= 1;
    }
    while fits(r + 1) {
        r += 1;
    }
    r
}

impl Codebook {
    /// Parse one codebook from the setup header. `budget` is the remaining number of codebook
    /// entries + VQ values the setup header may still allocate.
    pub fn parse(r: &mut BitReader, budget: &mut usize) -> Result<Self> {
        if r.read(24)? != 0x56_4342 {
            return Err(Error::invalid("vorbis: bad codebook sync pattern"));
        }
        let dimensions = r.read(16)? as usize;
        let entries = r.read(24)? as usize;
        if entries > MAX_ENTRIES || entries > *budget {
            return Err(Error::unsupported("vorbis: codebooks too large"));
        }
        *budget -= entries;

        let mut lengths = Vec::new();
        let ordered = r.read_bit()?;
        if !ordered {
            let sparse = r.read_bit()?;
            // every entry costs at least one bit (five when not sparse)
            let min_bits = if sparse { entries } else { entries * 5 };
            if min_bits > r.remaining() {
                return Err(Error::invalid("vorbis: codebook truncated"));
            }
            lengths.reserve_exact(entries);
            for _ in 0..entries {
                let used = !sparse || r.read_bit()?;
                lengths.push(if used { r.read(5)? as u8 + 1 } else { 0 });
            }
        } else {
            lengths.reserve_exact(entries);
            let mut length = r.read(5)? + 1;
            while lengths.len() < entries {
                if length > 32 {
                    return Err(Error::invalid("vorbis: codeword length above 32"));
                }
                let left = (entries - lengths.len()) as u32;
                let number = r.read(ilog(left))? as usize;
                if number > left as usize {
                    return Err(Error::invalid(
                        "vorbis: ordered codebook overruns its entries",
                    ));
                }
                lengths.resize(lengths.len() + number, length as u8);
                length += 1;
            }
        }

        let lookup_type = r.read(4)?;
        let vq = match lookup_type {
            0 => None,
            1 | 2 => {
                if dimensions == 0 {
                    return Err(Error::invalid("vorbis: VQ codebook with zero dimensions"));
                }
                let minimum = float32_unpack(r.read(32)?);
                let delta = float32_unpack(r.read(32)?);
                let value_bits = r.read(4)? + 1;
                let sequence_p = r.read_bit()?;
                let lookup_values = if lookup_type == 1 {
                    lookup1_values(entries, dimensions)
                } else {
                    entries
                        .checked_mul(dimensions)
                        .ok_or_else(|| Error::invalid("vorbis: codebook too large"))?
                };
                if lookup_values.saturating_mul(value_bits as usize) > r.remaining() {
                    return Err(Error::invalid("vorbis: codebook lookup table truncated"));
                }
                let total = entries.saturating_mul(dimensions);
                if total > MAX_VQ_VALUES || total > *budget {
                    return Err(Error::unsupported("vorbis: VQ codebook too large"));
                }
                *budget -= total;
                let mut mult = Vec::with_capacity(lookup_values);
                for _ in 0..lookup_values {
                    mult.push(r.read(value_bits)? as f32);
                }
                let mut values = Vec::with_capacity(total);
                for e in 0..entries {
                    let mut last = 0.0f32;
                    let mut divisor = 1usize;
                    for i in 0..dimensions {
                        let off = if lookup_type == 1 {
                            if lookup_values == 0 {
                                return Err(Error::invalid("vorbis: empty VQ lookup table"));
                            }
                            let o = (e / divisor) % lookup_values;
                            divisor = divisor.saturating_mul(lookup_values);
                            o
                        } else {
                            e * dimensions + i
                        };
                        let v = mult[off] * delta + minimum + last;
                        if sequence_p {
                            last = v;
                        }
                        values.push(v);
                    }
                }
                Some(values)
            }
            _ => return Err(Error::invalid("vorbis: unknown codebook lookup type")),
        };

        let huffman = build_huffman(&lengths)?;
        Ok(Self {
            dimensions,
            vq,
            huffman,
        })
    }

    /// A codebook from codeword lengths and an optional VQ table (unit tests).
    #[cfg(test)]
    pub fn from_parts(lengths: &[u8], dimensions: usize, vq: Option<Vec<f32>>) -> Self {
        Self {
            dimensions,
            vq,
            huffman: build_huffman(lengths).unwrap(),
        }
    }

    /// Decode one entry number.
    #[inline]
    pub fn decode_scalar(&self, r: &mut BitReader) -> Result<u32> {
        match &self.huffman {
            Huffman::Table {
                fast,
                fast_bits,
                long_codes,
                long_info,
            } => {
                let bits = r.peek32();
                let e = fast[(bits & ((1u32 << fast_bits) - 1)) as usize];
                if e != 0 {
                    r.consume(e & 0xff)?;
                    return Ok(e >> 8);
                }
                let key = bits.reverse_bits();
                let idx = long_codes.partition_point(|&c| c <= key);
                if idx == 0 {
                    return Err(Error::invalid("vorbis: invalid codeword"));
                }
                let info = long_info[idx - 1];
                let len = info & 0xff;
                let diff = key ^ long_codes[idx - 1];
                let matches = if len >= 32 {
                    diff == 0
                } else {
                    diff >> (32 - len) == 0
                };
                if !matches {
                    return Err(Error::invalid("vorbis: invalid codeword"));
                }
                r.consume(len)?;
                Ok(info >> 8)
            }
            Huffman::Single { entry, len } => {
                r.consume(*len)?;
                Ok(*entry)
            }
            Huffman::Empty => Err(Error::invalid("vorbis: decode from an empty codebook")),
        }
    }

    /// Decode one VQ vector (`dimensions` values).
    #[inline]
    pub fn decode_vq(&self, r: &mut BitReader) -> Result<&[f32]> {
        let e = self.decode_scalar(r)? as usize;
        let vq = self
            .vq
            .as_deref()
            .ok_or_else(|| Error::invalid("vorbis: scalar codebook used for VQ"))?;
        let d = self.dimensions;
        vq.get(e * d..e * d + d)
            .ok_or_else(|| Error::invalid("vorbis: VQ entry out of range"))
    }
}

/// Assign codewords to `lengths` in entry order (spec §3.2.1) and build the decode tables.
fn build_huffman(lengths: &[u8]) -> Result<Huffman> {
    let used = lengths.iter().filter(|&&l| l > 0).count();
    if used == 0 {
        return Ok(Huffman::Empty);
    }
    if used == 1 {
        let (entry, &len) = lengths
            .iter()
            .enumerate()
            .find(|(_, &l)| l > 0)
            .unwrap_or((0, &1));
        return Ok(Huffman::Single {
            entry: entry as u32,
            len: u32::from(len),
        });
    }

    // `free[d]`: the one unassigned tree node at depth `d` (value read MSB-first), if any.
    let mut free: [Option<u64>; 33] = [None; 33];
    free[0] = Some(0);
    let max_len = u32::from(*lengths.iter().max().unwrap_or(&1));
    let fast_bits = max_len.min(FAST_BITS);
    let mut fast = vec![0u32; 1 << fast_bits];
    let mut long: Vec<(u32, u32)> = Vec::new();

    for (entry, &len) in lengths.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let l = usize::from(len);
        let mut d = l;
        let node = loop {
            if let Some(n) = free[d] {
                break n;
            }
            if d == 0 {
                return Err(Error::invalid("vorbis: overspecified Huffman tree"));
            }
            d -= 1;
        };
        free[d] = None;
        for (j, slot) in free.iter_mut().enumerate().take(l + 1).skip(d + 1) {
            *slot = Some((node << (j - d)) | 1);
        }
        let code = node << (l - d);
        let len = len as u32;
        let info = ((entry as u32) << 8) | len;
        if len <= fast_bits {
            // the stream delivers the codeword's first bit first: index by the reversed code
            let rev = (code as u32).reverse_bits() >> (32 - len);
            let mut i = rev as usize;
            while i < fast.len() {
                fast[i] = info;
                i += 1 << len;
            }
        } else {
            long.push(((code << (32 - len)) as u32, info));
        }
    }
    long.sort_unstable_by_key(|&(c, _)| c);
    Ok(Huffman::Table {
        fast,
        fast_bits,
        long_codes: long.iter().map(|&(c, _)| c).collect(),
        long_info: long.iter().map(|&(_, i)| i).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testutil::CodeWriter as W;

    #[test]
    fn spec_example_tree() {
        // spec §3.2.1: lengths 2 4 4 4 4 2 3 3 → 00 0100 0101 0110 0111 10 110 111
        let h = build_huffman(&[2, 4, 4, 4, 4, 2, 3, 3]).unwrap();
        let book = Codebook {
            dimensions: 1,
            vq: None,
            huffman: h,
        };
        let codes = ["00", "0100", "0101", "0110", "0111", "10", "110", "111"];
        let mut w = W::default();
        for c in codes.iter().rev() {
            w.code(c);
        }
        let mut r = BitReader::new(&w.bytes);
        for e in (0..8).rev() {
            assert_eq!(book.decode_scalar(&mut r).unwrap(), e);
        }
    }

    #[test]
    fn long_codes_and_sparse() {
        // lengths 1, 0 (unused), 2, 3, ..., 12, 12: a deep, sparse tree
        let mut lengths = vec![1u8, 0, 2];
        for l in 3..=12 {
            lengths.push(l);
        }
        lengths.push(12);
        let h = build_huffman(&lengths).unwrap();
        let book = Codebook {
            dimensions: 1,
            vq: None,
            huffman: h,
        };
        // codes: e0=0, e2=10, e3=110, e4=1110, ..., e12 = 1^11 0 (len 12), e13 = 1^12
        let mut w = W::default();
        w.code("111111111111");
        w.code("111111111110");
        w.code("10");
        w.code("0");
        w.code("11111110");
        let mut r = BitReader::new(&w.bytes);
        assert_eq!(book.decode_scalar(&mut r).unwrap(), 13);
        assert_eq!(book.decode_scalar(&mut r).unwrap(), 12);
        assert_eq!(book.decode_scalar(&mut r).unwrap(), 2);
        assert_eq!(book.decode_scalar(&mut r).unwrap(), 0);
        assert_eq!(book.decode_scalar(&mut r).unwrap(), 8);
    }

    #[test]
    fn overspecified_rejected() {
        assert!(build_huffman(&[1, 1, 1]).is_err());
        assert!(build_huffman(&[2, 2, 2, 2, 3]).is_err());
        // underspecified trees are accepted (the gaps fail at decode time)
        assert!(build_huffman(&[2, 2, 2]).is_ok());
    }

    #[test]
    fn lookup1() {
        assert_eq!(lookup1_values(81, 4), 3);
        assert_eq!(lookup1_values(80, 4), 2);
        assert_eq!(lookup1_values(625, 4), 5);
        assert_eq!(lookup1_values(1, 8), 1);
        assert_eq!(lookup1_values(16_777_215, 1), 16_777_215);
    }

    #[test]
    fn float_unpack() {
        // 1.0 = mantissa 1 << 20, exponent 788 - 20
        assert_eq!(float32_unpack((1 << 20) | ((788 - 20) << 21)), 1.0);
        assert_eq!(float32_unpack(0x8000_0000 | (3 | (788 << 21))), -3.0);
    }
}
