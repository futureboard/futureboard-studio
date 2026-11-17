//! Bit-level readers and writers.
//!
//! [`BitReader`] reads MSB-first (MPEG audio, AAC, AC-3, FLAC, ALAC, Opus SILK headers);
//! [`LsbBitReader`] reads LSB-first (Vorbis). [`BitWriter`] writes MSB-first.
//!
//! Readers never panic on short input: reads past the end return [`Error::EndOfData`].

use crate::{Error, Result};

/// MSB-first bit reader over a byte slice.
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// The underlying bytes.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Bits consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Move to an absolute bit position (may be past the end; reads then fail).
    pub fn seek(&mut self, bit: usize) {
        self.pos = bit;
    }

    pub fn len_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// Bits left (0 when past the end).
    pub fn remaining(&self) -> usize {
        self.len_bits().saturating_sub(self.pos)
    }

    pub fn is_byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    /// Skip to the next byte boundary.
    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Byte offset of the current position (rounded down).
    pub fn byte_pos(&self) -> usize {
        self.pos / 8
    }

    /// The rest of the data from the current byte (the reader must be byte-aligned for this to
    /// be meaningful).
    pub fn rest(&self) -> &'a [u8] {
        self.data.get(self.pos / 8..).unwrap_or(&[])
    }

    /// Read `n` (0..=32) bits without consuming them; bits past the end read as zero.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let shift = self.pos & 7;
        // gather 8 bytes (zero-padded) big-endian
        let v = match self.data.get(byte..byte + 8) {
            Some(b) => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            None => {
                let mut v: u64 = 0;
                for i in 0..8 {
                    v = (v << 8) | u64::from(*self.data.get(byte + i).unwrap_or(&0));
                }
                v
            }
        };
        ((v << shift) >> (64 - n)) as u32
    }

    /// Read `n` (0..=32) bits.
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u32> {
        if self.pos + n as usize > self.len_bits() {
            self.pos = self.len_bits() + 1;
            return Err(Error::EndOfData);
        }
        let v = self.peek(n);
        self.pos += n as usize;
        Ok(v)
    }

    /// Read `n` (0..=64) bits.
    pub fn read_u64(&mut self, n: u32) -> Result<u64> {
        if n <= 32 {
            return self.read(n).map(u64::from);
        }
        let hi = u64::from(self.read(n - 32)?);
        let lo = u64::from(self.read(32)?);
        Ok((hi << 32) | lo)
    }

    #[inline]
    pub fn read_bit(&mut self) -> Result<bool> {
        self.read(1).map(|b| b != 0)
    }

    /// Read `n` bits as a two's-complement signed value.
    pub fn read_signed(&mut self, n: u32) -> Result<i32> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.read(n)?;
        Ok(((v << (32 - n)) as i32) >> (32 - n))
    }

    /// Skip `n` bits.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.len_bits() {
            self.pos = self.len_bits() + 1;
            return Err(Error::EndOfData);
        }
        self.pos += n;
        Ok(())
    }

    /// Count zero bits up to and including the next one bit (FLAC / ALAC Rice quotient):
    /// returns the number of zeros.
    pub fn read_unary_zeros(&mut self) -> Result<u32> {
        let mut n = 0;
        loop {
            // bits past the end peek as zero, so a one found is always real data
            let p = self.peek(32);
            if p != 0 {
                let z = p.leading_zeros();
                if self.pos + z as usize + 1 > self.len_bits() {
                    return Err(Error::EndOfData);
                }
                self.pos += z as usize + 1;
                return Ok(n + z);
            }
            n += 32;
            self.pos += 32;
            if self.pos >= self.len_bits() {
                self.pos = self.len_bits() + 1;
                return Err(Error::EndOfData);
            }
        }
    }

    /// Count one bits up to and including the next zero bit: returns the number of ones.
    pub fn read_unary_ones(&mut self) -> Result<u32> {
        let mut n = 0;
        while self.read_bit()? {
            n += 1;
        }
        Ok(n)
    }

    /// Unsigned Exp-Golomb code.
    pub fn read_ue(&mut self) -> Result<u32> {
        let z = self.read_unary_zeros()?;
        if z > 31 {
            return Err(Error::invalid("exp-golomb code too long"));
        }
        Ok((1u32 << z) - 1 + self.read(z)?)
    }
}

/// LSB-first bit reader (Vorbis packing).
#[derive(Clone, Debug)]
pub struct LsbBitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> LsbBitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Read `n` (0..=32) bits, first bit in the least significant position.
    pub fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.data.len() * 8 {
            self.pos = self.data.len() * 8;
            return Err(Error::EndOfData);
        }
        let byte = self.pos >> 3;
        let shift = self.pos & 7;
        let mut v: u64 = 0;
        for i in 0..5 {
            v |= u64::from(*self.data.get(byte + i).unwrap_or(&0)) << (8 * i);
        }
        self.pos += n as usize;
        Ok(((v >> shift) & ((1u64 << n) - 1)) as u32)
    }

    pub fn read_bit(&mut self) -> Result<bool> {
        self.read(1).map(|b| b != 0)
    }

    /// Read `n` bits as a float32 in Vorbis' packed format.
    pub fn read_u64(&mut self, n: u32) -> Result<u64> {
        if n <= 32 {
            return self.read(n).map(u64::from);
        }
        let lo = u64::from(self.read(32)?);
        let hi = u64::from(self.read(n - 32)?);
        Ok(lo | (hi << 32))
    }
}

/// MSB-first bit writer.
#[derive(Clone, Debug, Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    /// Pending bits, left-aligned in the low `nbits` of `acc`.
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            acc: 0,
            nbits: 0,
        }
    }

    /// Write the low `n` (0..=32) bits of `v`.
    #[inline]
    pub fn write(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        let v = u64::from(v) & ((1u64 << n) - 1);
        self.acc = (self.acc << n) | v;
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.buf.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1u64 << self.nbits) - 1;
    }

    pub fn write_u64(&mut self, v: u64, n: u32) {
        if n > 32 {
            self.write((v >> 32) as u32, n - 32);
            self.write(v as u32, 32);
        } else {
            self.write(v as u32, n);
        }
    }

    pub fn write_bit(&mut self, b: bool) {
        self.write(u32::from(b), 1);
    }

    /// Two's-complement signed value in `n` bits.
    pub fn write_signed(&mut self, v: i32, n: u32) {
        self.write(v as u32, n);
    }

    /// `n` zero bits followed by a one (Rice quotient).
    pub fn write_unary_zeros(&mut self, mut n: u32) {
        while n >= 32 {
            self.write(0, 32);
            n -= 32;
        }
        self.write(1, n + 1);
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        if self.nbits == 0 {
            self.buf.extend_from_slice(bytes);
        } else {
            for &b in bytes {
                self.write(u32::from(b), 8);
            }
        }
    }

    /// Pad with zero bits to a byte boundary.
    pub fn byte_align(&mut self) {
        if self.nbits > 0 {
            self.write(0, 8 - self.nbits);
        }
    }

    /// Pad with one bits to a byte boundary.
    pub fn byte_align_ones(&mut self) {
        if self.nbits > 0 {
            let n = 8 - self.nbits;
            self.write((1 << n) - 1, n);
        }
    }

    /// Bits written so far.
    pub fn bit_len(&self) -> usize {
        self.buf.len() * 8 + self.nbits as usize
    }

    pub fn is_byte_aligned(&self) -> bool {
        self.nbits == 0
    }

    /// Bytes completed so far (excluding a partial last byte).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Mutable access to completed bytes (patching headers / CRCs).
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    /// Finish (zero-padding to a byte boundary) and return the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.byte_align();
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_roundtrip() {
        let mut w = BitWriter::new();
        w.write(0b101, 3);
        w.write(0x1234_5678, 32);
        w.write_signed(-5, 7);
        w.write_unary_zeros(40);
        w.write(3, 2);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(32).unwrap(), 0x1234_5678);
        assert_eq!(r.read_signed(7).unwrap(), -5);
        assert_eq!(r.read_unary_zeros().unwrap(), 40);
        assert_eq!(r.read(2).unwrap(), 3);
    }

    #[test]
    fn end_of_data() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.read(8).unwrap(), 0xFF);
        assert_eq!(r.read(1), Err(Error::EndOfData));
    }

    #[test]
    fn lsb_reader() {
        let mut r = LsbBitReader::new(&[0b1010_0110, 0xFF]);
        assert_eq!(r.read(3).unwrap(), 0b110);
        assert_eq!(r.read(5).unwrap(), 0b10100);
        assert_eq!(r.read(8).unwrap(), 0xFF);
    }

    #[test]
    fn exp_golomb() {
        // 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(r.read_ue().unwrap(), 0);
        assert_eq!(r.read_ue().unwrap(), 1);
        assert_eq!(r.read_ue().unwrap(), 2);
        assert_eq!(r.read_ue().unwrap(), 3);
    }
}
