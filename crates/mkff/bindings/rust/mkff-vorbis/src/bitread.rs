//! LSB-first bit reader tuned for Vorbis packet decode.
//!
//! `mkff_audio_core::bits::LsbBitReader` reads byte by byte and has no peek, which the table-driven
//! Huffman decoder needs; this reader gathers eight bytes at a time and supports peeking 32 bits
//! (bits past the end of the packet read as zero).

use mkff_audio_core::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[inline]
    fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// Bits left before the end of the packet.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.total_bits().saturating_sub(self.pos)
    }

    /// The next 32 bits (first bit in bit 0), zero-padded past the end.
    #[inline]
    pub fn peek32(&self) -> u32 {
        let byte = self.pos >> 3;
        let shift = (self.pos & 7) as u32;
        if let Some(b) = self.data.get(byte..byte + 8) {
            let v = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            (v >> shift) as u32
        } else {
            let mut v: u64 = 0;
            for i in 0..5 {
                v |= u64::from(self.data.get(byte + i).copied().unwrap_or(0)) << (8 * i);
            }
            (v >> shift) as u32
        }
    }

    /// Skip `n` bits that were peeked. Fails (and moves to the end) if fewer remain.
    #[inline]
    pub fn consume(&mut self, n: u32) -> Result<()> {
        let end = self.pos + n as usize;
        if end > self.total_bits() {
            self.pos = self.total_bits();
            return Err(Error::EndOfData);
        }
        self.pos = end;
        Ok(())
    }

    /// Read `n` (0..=32) bits.
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        let v = self.peek32();
        self.consume(n)?;
        Ok(if n == 32 { v } else { v & ((1u32 << n) - 1) })
    }

    /// Read `n` (0..=64) bits.
    pub fn read_u64(&mut self, n: u32) -> Result<u64> {
        if n <= 32 {
            return self.read(n).map(u64::from);
        }
        let lo = u64::from(self.read(32)?);
        let hi = u64::from(self.read(n - 32)?);
        Ok(lo | (hi << 32))
    }

    #[inline]
    pub fn read_bit(&mut self) -> Result<bool> {
        self.read(1).map(|b| b != 0)
    }
}

/// Vorbis `ilog`: the position of the highest set bit, counting from 1 (`ilog(0) = 0`).
#[inline]
pub(crate) fn ilog(x: u32) -> u32 {
    32 - x.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lsb_first() {
        let data = [0b1010_1100u8, 0xff, 0x01];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(2).unwrap(), 0b00);
        assert_eq!(r.read(3).unwrap(), 0b011);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(9).unwrap(), 0x1ff);
        assert_eq!(r.remaining(), 7);
        assert!(r.read(8).is_err());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn read_32_and_64() {
        let data: Vec<u8> = (1..=12).collect();
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(32).unwrap(), 0x0403_0201);
        assert_eq!(r.read_u64(40).unwrap(), 0x09_0807_0605);
        assert_eq!(r.read(4).unwrap(), 0xa);
    }

    #[test]
    fn ilog_values() {
        assert_eq!(ilog(0), 0);
        assert_eq!(ilog(1), 1);
        assert_eq!(ilog(2), 2);
        assert_eq!(ilog(3), 2);
        assert_eq!(ilog(4), 3);
        assert_eq!(ilog(7), 3);
        assert_eq!(ilog(u32::MAX), 32);
    }
}
