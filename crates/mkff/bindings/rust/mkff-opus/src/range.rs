//! The Opus range decoder (RFC 6716 §4.1).
//!
//! Follows the reference implementation's `ec_dec` (see `NOTICE`): bytes are consumed from the
//! front for range-coded symbols and from the back for raw bits.

/// Bits of fractional precision used by [`RangeDecoder::tell_frac`].
pub const BITRES: i32 = 3;

const SYM_BITS: u32 = 8;
const CODE_BITS: i32 = 32;
const SYM_MAX: u32 = (1 << SYM_BITS) - 1;
const CODE_TOP: u32 = 1 << (CODE_BITS - 1);
const CODE_BOT: u32 = CODE_TOP >> SYM_BITS;
const CODE_EXTRA: u32 = ((CODE_BITS - 2) % SYM_BITS as i32 + 1) as u32;
const UINT_BITS: u32 = 8;
const WINDOW_SIZE: i32 = 32;

/// `ilog(x)`: the number of bits needed to represent `x` (0 for 0).
#[inline]
pub fn ilog(x: u32) -> i32 {
    32 - x.leading_zeros() as i32
}

/// A range decoder over one Opus frame.
#[derive(Clone, Debug)]
pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    storage: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    offs: usize,
    rng: u32,
    val: u32,
    ext: u32,
    rem: u32,
}

impl<'a> RangeDecoder<'a> {
    /// Start decoding `buf` (`ec_dec_init`).
    pub fn new(buf: &'a [u8]) -> Self {
        let mut d = RangeDecoder {
            buf,
            storage: buf.len(),
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: CODE_BITS + 1
                - ((CODE_BITS - CODE_EXTRA as i32) / SYM_BITS as i32) * SYM_BITS as i32,
            offs: 0,
            rng: 1 << CODE_EXTRA,
            val: 0,
            ext: 0,
            rem: 0,
        };
        d.rem = d.read_byte();
        d.val = d.rng - 1 - (d.rem >> (SYM_BITS - CODE_EXTRA));
        d.normalize();
        d
    }

    #[inline]
    fn read_byte(&mut self) -> u32 {
        if self.offs < self.storage {
            let b = self.buf[self.offs];
            self.offs += 1;
            u32::from(b)
        } else {
            0
        }
    }

    #[inline]
    fn read_byte_from_end(&mut self) -> u32 {
        if self.end_offs < self.storage {
            self.end_offs += 1;
            u32::from(self.buf[self.storage - self.end_offs])
        } else {
            0
        }
    }

    #[inline]
    fn normalize(&mut self) {
        while self.rng <= CODE_BOT {
            self.nbits_total += SYM_BITS as i32;
            self.rng <<= SYM_BITS;
            let mut sym = self.rem;
            self.rem = self.read_byte();
            sym = ((sym << SYM_BITS) | self.rem) >> (SYM_BITS - CODE_EXTRA);
            self.val = ((self.val << SYM_BITS).wrapping_add(SYM_MAX & !sym)) & (CODE_TOP - 1);
        }
    }

    /// Size of the buffer in bytes (as the decoder sees it).
    pub fn storage(&self) -> usize {
        self.storage
    }

    /// Remove `bytes` from the end of the buffer (the Opus layer does this for redundancy).
    pub fn shrink_storage(&mut self, bytes: usize) {
        self.storage = self.storage.saturating_sub(bytes);
    }

    /// The final range (`rng`), used to verify decoding against an encoder.
    pub fn range(&self) -> u32 {
        self.rng
    }

    /// `ec_decode`: the cumulative frequency of the next symbol, for a total of `ft`.
    #[inline]
    pub fn decode(&mut self, ft: u32) -> u32 {
        self.ext = self.rng / ft.max(1);
        let s = self.val / self.ext.max(1);
        ft - (s + 1).min(ft)
    }

    /// `ec_decode_bin`: [`decode`](Self::decode) with `ft = 1 << bits`.
    #[inline]
    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        self.ext = self.rng >> bits;
        let s = self.val / self.ext.max(1);
        (1u32 << bits) - (s + 1).min(1u32 << bits)
    }

    /// `ec_dec_update`: consume the symbol `[fl, fh)` of `ft`.
    #[inline]
    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let s = self.ext.wrapping_mul(ft - fh);
        self.val = self.val.wrapping_sub(s);
        self.rng = if fl > 0 {
            self.ext.wrapping_mul(fh - fl)
        } else {
            self.rng.wrapping_sub(s)
        };
        self.normalize();
    }

    /// `ec_dec_bit_logp`: a bit whose probability of being one is `1 / (1 << logp)`.
    #[inline]
    pub fn bit_logp(&mut self, logp: u32) -> bool {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = d < s;
        if !ret {
            self.val = d - s;
        }
        self.rng = if ret { s } else { r - s };
        self.normalize();
        ret
    }

    /// `ec_dec_icdf`: a symbol coded with an inverse CDF table of `1 << ftb` total.
    #[inline]
    pub fn icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut ret = 0usize;
        let mut t;
        loop {
            t = s;
            s = r.wrapping_mul(u32::from(icdf.get(ret).copied().unwrap_or(0)));
            if d >= s {
                break;
            }
            ret += 1;
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        ret
    }

    /// `ec_dec_uint`: a uniformly distributed integer in `0..ft`.
    pub fn uint(&mut self, ft: u32) -> u32 {
        if ft <= 1 {
            return 0;
        }
        let ft = ft - 1;
        let mut ftb = ilog(ft);
        if ftb > UINT_BITS as i32 {
            ftb -= UINT_BITS as i32;
            let ft1 = (ft >> ftb) + 1;
            let s = self.decode(ft1);
            self.update(s, s + 1, ft1);
            let t = (s << ftb) | self.bits(ftb as u32);
            // Out of range values (corrupt data) saturate, as in the reference decoder.
            t.min(ft)
        } else {
            let ft1 = ft + 1;
            let s = self.decode(ft1);
            self.update(s, s + 1, ft1);
            s
        }
    }

    /// `ec_dec_bits`: `bits` raw bits (at most 25) from the end of the buffer.
    pub fn bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;
        if (available as u32) < bits {
            loop {
                window |= self.read_byte_from_end() << available;
                available += SYM_BITS as i32;
                if available > WINDOW_SIZE - SYM_BITS as i32 {
                    break;
                }
            }
        }
        let ret = window & ((1u32 << bits) - 1);
        window = if bits >= 32 { 0 } else { window >> bits };
        available -= bits as i32;
        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits as i32;
        ret
    }

    /// `ec_tell`: bits used so far (rounded up).
    #[inline]
    pub fn tell(&self) -> i32 {
        self.nbits_total - ilog(self.rng)
    }

    /// `ec_tell_frac`: bits used so far in 1/8 bit units (rounded up).
    pub fn tell_frac(&self) -> u32 {
        let nbits = (self.nbits_total as u32) << BITRES;
        let mut l = ilog(self.rng);
        let mut r = self.rng >> (l - 16);
        for _ in 0..BITRES {
            r = (r * r) >> 15;
            let b = (r >> 16) as i32;
            l = (l << 1) | b;
            r >>= b;
        }
        nbits.wrapping_sub(l as u32)
    }

    /// Pretend all remaining bits were consumed (CELT silence frames).
    pub(crate) fn consume_all(&mut self) {
        let total = (self.storage * 8) as i32;
        self.nbits_total += total - self.tell();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_decodes_zeros() {
        let mut d = RangeDecoder::new(&[]);
        assert_eq!(d.tell(), 1);
        for _ in 0..100 {
            let _ = d.bit_logp(1);
            let _ = d.uint(1000);
            let _ = d.bits(7);
            let _ = d.icdf(&[128, 64, 0], 8);
        }
    }

    #[test]
    fn tell_starts_at_one_bit() {
        let d = RangeDecoder::new(&[0x55, 0xAA]);
        assert_eq!(d.tell(), 1);
        assert_eq!(d.tell_frac(), 8);
    }

    #[test]
    fn raw_bits_come_from_the_end() {
        let data = [0u8, 0, 0, 0b1010_0110];
        let mut d = RangeDecoder::new(&data);
        assert_eq!(d.bits(1), 0);
        assert_eq!(d.bits(2), 0b11);
        assert_eq!(d.bits(5), 0b10100);
    }

    /// A tiny range encoder (RFC 6716 §5.1) used to round-trip symbols through the decoder.
    struct Enc {
        out: Vec<u8>,
        rng: u32,
        val: u32,
        rem: i32,
        ext: u32,
        end: Vec<u8>,
        end_window: u32,
        nend_bits: i32,
    }

    impl Enc {
        fn new() -> Self {
            Enc {
                out: Vec::new(),
                rng: CODE_TOP,
                val: 0,
                rem: -1,
                ext: 0,
                end: Vec::new(),
                end_window: 0,
                nend_bits: 0,
            }
        }
        fn carry_out(&mut self, c: i32) {
            if c as u32 != SYM_MAX {
                let carry = c >> SYM_BITS;
                if self.rem >= 0 {
                    self.out.push((self.rem + carry) as u8);
                }
                if self.ext > 0 {
                    let sym = (SYM_MAX as i32 + carry) as u8 & SYM_MAX as u8;
                    for _ in 0..self.ext {
                        self.out.push(sym);
                    }
                    self.ext = 0;
                }
                self.rem = c & SYM_MAX as i32;
            } else {
                self.ext += 1;
            }
        }
        fn normalize(&mut self) {
            while self.rng <= CODE_BOT {
                self.carry_out((self.val >> (CODE_BITS as u32 - SYM_BITS - 1)) as i32);
                self.val = (self.val << SYM_BITS) & (CODE_TOP - 1);
                self.rng <<= SYM_BITS;
            }
        }
        fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
            let r = self.rng / ft;
            if fl > 0 {
                self.val += self.rng - r * (ft - fl);
                self.rng = r * (fh - fl);
            } else {
                self.rng -= r * (ft - fh);
            }
            self.normalize();
        }
        fn bit_logp(&mut self, bit: bool, logp: u32) {
            let r = self.rng;
            let l = self.val;
            let s = r >> logp;
            let r = r - s;
            if bit {
                self.val = l + r;
            }
            self.rng = if bit { s } else { r };
            self.normalize();
        }
        fn bits(&mut self, v: u32, n: u32) {
            self.end_window |= v << self.nend_bits;
            self.nend_bits += n as i32;
            while self.nend_bits >= 8 {
                self.end.push(self.end_window as u8);
                self.end_window >>= 8;
                self.nend_bits -= 8;
            }
        }
        fn finish(mut self) -> Vec<u8> {
            let mut l = CODE_BITS - ilog(self.rng);
            let mut msk = (CODE_TOP - 1) >> l;
            let mut end = (self.val + msk) & !msk;
            if (end | msk) >= self.val + self.rng {
                l += 1;
                msk >>= 1;
                end = (self.val + msk) & !msk;
            }
            while l > 0 {
                self.carry_out((end >> (CODE_BITS as u32 - SYM_BITS - 1)) as i32);
                end = (end << SYM_BITS) & (CODE_TOP - 1);
                l -= SYM_BITS as i32;
            }
            if self.rem >= 0 || self.ext > 0 {
                self.carry_out(0);
            }
            if self.nend_bits > 0 {
                self.end.push(self.end_window as u8);
            }
            let mut out = self.out;
            // Leave room so the front and back never overlap.
            out.resize(out.len() + 4, 0);
            self.end.reverse();
            out.extend_from_slice(&self.end);
            out
        }
    }

    #[test]
    fn round_trip_symbols() {
        let mut e = Enc::new();
        let mut want = Vec::new();
        let mut x = 12345u32;
        for i in 0..2000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let ft = 2 + (x >> 20) % 300;
            let s = (x >> 8) % ft;
            match i % 3 {
                0 => e.encode(s, s + 1, ft),
                1 => e.bit_logp(s & 1 == 1, 1 + s % 15),
                _ => e.bits(s & 0x7f, 7),
            }
            want.push((ft, s));
        }
        let data = e.finish();
        let mut d = RangeDecoder::new(&data);
        for (i, &(ft, s)) in want.iter().enumerate() {
            match i % 3 {
                0 => {
                    let f = d.decode(ft);
                    assert_eq!(f, s, "symbol {i}");
                    d.update(f, f + 1, ft);
                }
                1 => assert_eq!(d.bit_logp(1 + s % 15), s & 1 == 1, "bit {i}"),
                _ => assert_eq!(d.bits(7), s & 0x7f, "raw {i}"),
            }
        }
        assert!(d.tell() as usize <= data.len() * 8);
    }
}
