//! The `STREAMINFO` metadata block (RFC 9639 section 8.2).

use mkff_audio_core::{Error, Result};

/// Length of the `STREAMINFO` body in bytes.
pub const STREAMINFO_LEN: usize = 34;

/// FLAC stream parameters from the mandatory `STREAMINFO` metadata block.
///
/// Zero in `min_frame` / `max_frame` / `total_samples` means "unknown", and an all-zero `md5`
/// means the encoder did not compute the signature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StreamInfo {
    /// Smallest block size in samples (the last block of a stream may be smaller).
    pub min_block: u16,
    /// Largest block size in samples.
    pub max_block: u16,
    /// Smallest frame in bytes (24 bits; 0 = unknown).
    pub min_frame: u32,
    /// Largest frame in bytes (24 bits; 0 = unknown).
    pub max_frame: u32,
    /// Sample rate in Hz (20 bits).
    pub sample_rate: u32,
    /// Channel count, 1..=8.
    pub channels: u8,
    /// Bits per sample, 4..=32.
    pub bits_per_sample: u8,
    /// Samples per channel in the stream (36 bits; 0 = unknown).
    pub total_samples: u64,
    /// MD5 of the decoded samples: interleaved, signed little-endian, `ceil(bps / 8)` bytes each.
    pub md5: [u8; 16],
}

impl StreamInfo {
    /// Parse `STREAMINFO` from any of:
    ///
    /// - the bare 34-byte body (what MP4 `dfLa` minus its block header, and most APIs, carry);
    /// - a metadata block: the 4-byte block header (type 0, length 34) and the body;
    /// - the start of a native FLAC stream: `fLaC`, then the first metadata block.
    ///
    /// A slice of exactly 34 bytes is always taken as the bare body.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let body = if data.starts_with(b"fLaC") {
            let h = data.get(4..8).ok_or(Error::EndOfData)?;
            if h[0] & 0x7F != 0 {
                return Err(Error::invalid(
                    "first FLAC metadata block is not STREAMINFO",
                ));
            }
            if be24(&h[1..4]) < STREAMINFO_LEN as u32 {
                return Err(Error::invalid("STREAMINFO block too short"));
            }
            data.get(8..8 + STREAMINFO_LEN).ok_or(Error::EndOfData)?
        } else if data.len() >= STREAMINFO_LEN + 4
            && data[0] & 0x7F == 0
            && be24(&data[1..4]) == STREAMINFO_LEN as u32
        {
            &data[4..4 + STREAMINFO_LEN]
        } else {
            data.get(..STREAMINFO_LEN).ok_or(Error::EndOfData)?
        };
        let b = body;
        let packed = u64::from_be_bytes([b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
        let mut md5 = [0u8; 16];
        md5.copy_from_slice(&b[18..34]);
        let info = StreamInfo {
            min_block: u16::from_be_bytes([b[0], b[1]]),
            max_block: u16::from_be_bytes([b[2], b[3]]),
            min_frame: be24(&b[4..7]),
            max_frame: be24(&b[7..10]),
            sample_rate: (packed >> 44) as u32,
            channels: ((packed >> 41) & 7) as u8 + 1,
            bits_per_sample: ((packed >> 36) & 31) as u8 + 1,
            total_samples: packed & ((1 << 36) - 1),
            md5,
        };
        if info.bits_per_sample < 4 {
            return Err(Error::invalid("FLAC bits per sample below 4"));
        }
        Ok(info)
    }

    /// The 34-byte `STREAMINFO` body. Fields wider than their bit field are truncated.
    pub fn to_bytes(&self) -> [u8; STREAMINFO_LEN] {
        let mut b = [0u8; STREAMINFO_LEN];
        b[0..2].copy_from_slice(&self.min_block.to_be_bytes());
        b[2..4].copy_from_slice(&self.max_block.to_be_bytes());
        b[4..7].copy_from_slice(&self.min_frame.to_be_bytes()[1..]);
        b[7..10].copy_from_slice(&self.max_frame.to_be_bytes()[1..]);
        let packed = (u64::from(self.sample_rate) & 0xF_FFFF) << 44
            | (u64::from(self.channels.clamp(1, 8) - 1) << 41)
            | (u64::from(self.bits_per_sample.clamp(1, 32) - 1) << 36)
            | (self.total_samples & ((1 << 36) - 1));
        b[10..18].copy_from_slice(&packed.to_be_bytes());
        b[18..34].copy_from_slice(&self.md5);
        b
    }

    /// `fLaC` followed by this `STREAMINFO` as the only (last) metadata block: the start of a
    /// native FLAC file, ready for frames to be appended.
    pub fn to_stream_header(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(42);
        v.extend_from_slice(b"fLaC");
        v.extend_from_slice(&[0x80, 0, 0, STREAMINFO_LEN as u8]);
        v.extend_from_slice(&self.to_bytes());
        v
    }
}

fn be24(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_forms() {
        let si = StreamInfo {
            min_block: 4096,
            max_block: 4096,
            min_frame: 14,
            max_frame: 0xABCDEF,
            sample_rate: 96000,
            channels: 6,
            bits_per_sample: 24,
            total_samples: (1 << 36) - 5,
            md5: [7; 16],
        };
        let body = si.to_bytes();
        assert_eq!(StreamInfo::parse(&body).unwrap(), si);
        let file = si.to_stream_header();
        assert_eq!(StreamInfo::parse(&file).unwrap(), si);
        assert_eq!(StreamInfo::parse(&file[4..]).unwrap(), si);
        assert!(StreamInfo::parse(&body[..20]).is_err());
        assert!(StreamInfo::parse(&file[..20]).is_err());
        assert!(StreamInfo::parse(b"fLaC\x01\0\0\x22").is_err());
    }
}
