//! IVF: the minimal container libvpx / libaom tools write for VP8, VP9 and AV1 elementary
//! streams (32-byte file header, then a 12-byte header per frame).

use crate::{Error, Result, VideoCodec};

/// The IVF file header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IvfHeader {
    pub fourcc: [u8; 4],
    pub width: u16,
    pub height: u16,
    /// Timestamps are in units of `timebase_num / timebase_den` seconds… as written by libvpx:
    /// the header stores the rate (`den`) and scale (`num`).
    pub timebase_num: u32,
    pub timebase_den: u32,
    pub frame_count: u32,
}

impl IvfHeader {
    /// The codec named by the FourCC (`VP80`, `VP90`, `AV01`, plus `H264` / `HEVC` / `MJPG`).
    pub fn codec(&self) -> Option<VideoCodec> {
        Some(match &self.fourcc.map(|b| b.to_ascii_uppercase()) {
            b"VP80" => VideoCodec::Vp8,
            b"VP90" => VideoCodec::Vp9,
            b"AV01" => VideoCodec::Av1,
            b"H264" | b"AVC1" => VideoCodec::H264,
            b"HEVC" | b"H265" | b"HVC1" => VideoCodec::Hevc,
            b"MJPG" => VideoCodec::Mjpeg,
            _ => return None,
        })
    }
}

/// One IVF frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IvfFrame<'a> {
    pub pts: i64,
    pub data: &'a [u8],
}

/// Iterates the frames of an in-memory IVF file.
#[derive(Clone, Debug)]
pub struct IvfReader<'a> {
    header: IvfHeader,
    data: &'a [u8],
    pos: usize,
}

/// Whether `data` starts with an IVF signature.
pub fn sniff(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..4] == b"DKIF"
}

impl<'a> IvfReader<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if !sniff(data) || data.len() < 32 {
            return Err(Error::Unsupported("not an IVF file".into()));
        }
        let le16 = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        let le32 = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        let header_len = usize::from(le16(6)).max(32);
        let header = IvfHeader {
            fourcc: [data[8], data[9], data[10], data[11]],
            width: le16(12),
            height: le16(14),
            timebase_den: le32(16),
            timebase_num: le32(20),
            frame_count: le32(24),
        };
        Ok(Self {
            header,
            data,
            pos: header_len.min(data.len()),
        })
    }

    pub fn header(&self) -> &IvfHeader {
        &self.header
    }
}

impl<'a> Iterator for IvfReader<'a> {
    type Item = IvfFrame<'a>;

    fn next(&mut self) -> Option<IvfFrame<'a>> {
        let h = self.data.get(self.pos..self.pos + 12)?;
        let size = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let pts = i64::from_le_bytes([h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11]]);
        let start = self.pos + 12;
        let data = self.data.get(start..start.checked_add(size)?)?;
        self.pos = start + size;
        Some(IvfFrame { pts, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_header_and_frames() {
        let mut f = b"DKIF".to_vec();
        f.extend_from_slice(&0u16.to_le_bytes());
        f.extend_from_slice(&32u16.to_le_bytes());
        f.extend_from_slice(b"VP90");
        f.extend_from_slice(&64u16.to_le_bytes());
        f.extend_from_slice(&48u16.to_le_bytes());
        f.extend_from_slice(&30u32.to_le_bytes());
        f.extend_from_slice(&1u32.to_le_bytes());
        f.extend_from_slice(&2u32.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        for (pts, payload) in [(0i64, &b"abc"[..]), (1, &b"de"[..])] {
            f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            f.extend_from_slice(&pts.to_le_bytes());
            f.extend_from_slice(payload);
        }
        let r = IvfReader::new(&f).unwrap();
        assert_eq!(r.header().codec(), Some(VideoCodec::Vp9));
        assert_eq!(
            (r.header().width, r.header().height, r.header().timebase_den),
            (64, 48, 30)
        );
        let frames: Vec<_> = r.collect();
        assert_eq!(
            frames,
            vec![
                IvfFrame {
                    pts: 0,
                    data: b"abc"
                },
                IvfFrame {
                    pts: 1,
                    data: b"de"
                }
            ]
        );
    }

    #[test]
    fn truncated_frame_ends_iteration() {
        let mut f = b"DKIF".to_vec();
        f.resize(32, 0);
        f.extend_from_slice(&100u32.to_le_bytes());
        f.extend_from_slice(&0i64.to_le_bytes());
        f.extend_from_slice(b"short");
        assert_eq!(IvfReader::new(&f).unwrap().count(), 0);
    }
}
