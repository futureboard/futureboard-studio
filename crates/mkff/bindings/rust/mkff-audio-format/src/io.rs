//! Buffered random-access reading over a [`ReadSeek`] and bounds-checked byte cursors.

use std::io::{Read, Seek, SeekFrom};

use mkff_audio_core::{Error, Result};

use crate::ReadSeek;

const WINDOW: usize = 64 * 1024;

/// A buffered reader with a logical position. Seeking only moves the position; the underlying
/// reader is touched when bytes outside the cached window are needed. Short reads happen only at
/// the end of the stream.
pub(crate) struct Source {
    inner: Box<dyn ReadSeek>,
    len: u64,
    buf: Vec<u8>,
    buf_pos: u64,
    pos: u64,
    inner_pos: Option<u64>,
}

impl Source {
    pub fn new(mut inner: Box<dyn ReadSeek>) -> Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            len,
            buf: Vec::new(),
            buf_pos: 0,
            pos: 0,
            inner_pos: Some(0),
        })
    }

    /// Total length of the stream in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    /// Bytes from the position to the end of the stream.
    pub fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.pos)
    }

    fn raw_read(&mut self, at: u64, out: &mut [u8]) -> Result<usize> {
        if self.inner_pos != Some(at) {
            self.inner.seek(SeekFrom::Start(at))?;
        }
        let mut done = 0;
        while done < out.len() {
            match self.inner.read(&mut out[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.inner_pos = None;
                    return Err(e.into());
                }
            }
        }
        self.inner_pos = Some(at + done as u64);
        Ok(done)
    }

    /// Read up to `out.len()` bytes at the position (fewer only at the end of the stream).
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < out.len() {
            let pos = self.pos;
            if pos >= self.buf_pos && pos < self.buf_pos + self.buf.len() as u64 {
                let off = (pos - self.buf_pos) as usize;
                let n = (self.buf.len() - off).min(out.len() - done);
                out[done..done + n].copy_from_slice(&self.buf[off..off + n]);
                done += n;
                self.pos += n as u64;
                continue;
            }
            if pos >= self.len {
                break;
            }
            let want = out.len() - done;
            if want >= WINDOW {
                let n = self.raw_read(pos, &mut out[done..])?;
                done += n;
                self.pos += n as u64;
                break;
            }
            let mut buf = std::mem::take(&mut self.buf);
            buf.resize(WINDOW, 0);
            let n = self.raw_read(pos, &mut buf)?;
            buf.truncate(n);
            self.buf = buf;
            self.buf_pos = pos;
            if n == 0 {
                break;
            }
        }
        Ok(done)
    }

    /// Read exactly `out.len()` bytes or fail with [`Error::EndOfData`].
    pub fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        if self.read(out)? == out.len() {
            Ok(())
        } else {
            Err(Error::EndOfData)
        }
    }

    /// Read `n` bytes into a new vector; fails before allocating when fewer remain.
    pub fn read_vec(&mut self, n: u64) -> Result<Vec<u8>> {
        if n > self.remaining() || n > usize::MAX as u64 {
            return Err(Error::EndOfData);
        }
        let mut v = vec![0; n as usize];
        self.read_exact(&mut v)?;
        Ok(v)
    }

    /// Read up to `n` bytes (fewer at the end of the stream).
    pub fn read_upto(&mut self, n: usize) -> Result<Vec<u8>> {
        let n = (n as u64).min(self.remaining()) as usize;
        let mut v = vec![0; n];
        let got = self.read(&mut v)?;
        v.truncate(got);
        Ok(v)
    }

    /// Read up to `out.len()` bytes at `pos`, leaving the position after them.
    pub fn read_at(&mut self, pos: u64, out: &mut [u8]) -> Result<usize> {
        self.seek(pos);
        self.read(out)
    }
}

/// A bounds-checked cursor over a byte slice (big-endian unless the method says `le`).
#[derive(Clone, Debug)]
pub(crate) struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn rest(&self) -> &'a [u8] {
        self.data.get(self.pos..).unwrap_or(&[])
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            self.pos = self.data.len();
            return Err(Error::EndOfData);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn u24(&mut self) -> Result<u32> {
        let b = self.bytes(3)?;
        Ok(u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub fn le_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn le_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
}

/// Big-endian / little-endian helpers that read 0 past the end of a slice.
pub(crate) fn be16(d: &[u8], at: usize) -> u16 {
    u16::from(get(d, at)) << 8 | u16::from(get(d, at + 1))
}

pub(crate) fn be32(d: &[u8], at: usize) -> u32 {
    u32::from(be16(d, at)) << 16 | u32::from(be16(d, at + 2))
}

pub(crate) fn le16(d: &[u8], at: usize) -> u16 {
    u16::from(get(d, at)) | u16::from(get(d, at + 1)) << 8
}

pub(crate) fn le32(d: &[u8], at: usize) -> u32 {
    u32::from(le16(d, at)) | u32::from(le16(d, at + 2)) << 16
}

pub(crate) fn get(d: &[u8], at: usize) -> u8 {
    d.get(at).copied().unwrap_or(0)
}

/// Write `data` at `pos` of a `Write + Seek`, then return to where the writer was.
pub(crate) fn patch<W: std::io::Write + Seek>(w: &mut W, pos: u64, data: &[u8]) -> Result<()> {
    let here = w.stream_position()?;
    w.seek(SeekFrom::Start(pos))?;
    w.write_all(data)?;
    w.seek(SeekFrom::Start(here))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn source_reads_across_windows() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut s = Source::new(Box::new(Cursor::new(data.clone()))).unwrap();
        let mut b = [0u8; 10];
        s.read_at(65_530, &mut b).unwrap();
        assert_eq!(&b[..], &data[65_530..65_540]);
        let v = s.read_vec(100_000).unwrap();
        assert_eq!(&v[..], &data[65_540..165_540]);
        assert!(s.read_vec(1_000_000).is_err());
        s.seek(199_995);
        assert_eq!(s.read_upto(100).unwrap().len(), 5);
    }
}
