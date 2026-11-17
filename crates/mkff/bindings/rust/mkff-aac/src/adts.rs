//! ADTS framing (13818-7 6.2 / 14496-3 1.A.2).

use crate::asc::AudioSpecificConfig;
use crate::tables::{nearest_sampling_index, SAMPLE_RATES};

/// A parsed ADTS frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdtsHeader {
    /// `ID`: false = MPEG-4, true = MPEG-2.
    pub mpeg2: bool,
    /// `protection_absent` is 0: a CRC follows the fixed header.
    pub has_crc: bool,
    /// Audio object type (`profile` + 1; 2 = AAC-LC).
    pub object_type: u8,
    /// `sampling_frequency_index` (0..=12).
    pub sampling_index: u8,
    /// Sampling rate in Hz.
    pub sample_rate: u32,
    /// `channel_configuration` (0: a PCE in the payload).
    pub channel_config: u8,
    /// Whole frame length in bytes, header included.
    pub frame_length: usize,
    /// `adts_buffer_fullness` (0x7FF = variable rate).
    pub buffer_fullness: u16,
    /// Raw data blocks in the frame (`number_of_raw_data_blocks_in_frame` + 1).
    pub raw_blocks: usize,
    /// Header length in bytes: 7, plus the CRC (2) and, with several blocks, the block
    /// positions (2 each) when `has_crc`.
    pub header_length: usize,
}

impl AdtsHeader {
    /// The equivalent `AudioSpecificConfig` (AAC-LC core; implicit SBR stays undetected).
    pub fn to_asc(&self) -> AudioSpecificConfig {
        AudioSpecificConfig {
            object_type: self.object_type,
            sampling_index: self.sampling_index,
            sample_rate: self.sample_rate,
            channel_config: self.channel_config,
            ..Default::default()
        }
    }

    /// Payload length (frame minus header) in bytes.
    pub fn payload_length(&self) -> usize {
        self.frame_length.saturating_sub(self.header_length)
    }
}

/// Parse the ADTS header at the start of `data` (at least 7 bytes). Returns `None` when there
/// is no valid header (no sync word, layer not 0, reserved sampling index, a frame length
/// shorter than its header).
pub fn parse_adts(data: &[u8]) -> Option<AdtsHeader> {
    if data.len() < 7 || data[0] != 0xFF || data[1] & 0xF6 != 0xF0 {
        return None;
    }
    let mpeg2 = data[1] & 0x08 != 0;
    let has_crc = data[1] & 0x01 == 0;
    let profile = data[2] >> 6;
    let sampling_index = (data[2] >> 2) & 0x0F;
    if usize::from(sampling_index) >= SAMPLE_RATES.len() {
        return None;
    }
    let channel_config = ((data[2] & 1) << 2) | (data[3] >> 6);
    let frame_length =
        (usize::from(data[3] & 3) << 11) | (usize::from(data[4]) << 3) | usize::from(data[5] >> 5);
    let buffer_fullness = (u16::from(data[5] & 0x1F) << 6) | u16::from(data[6] >> 2);
    let raw_blocks = usize::from(data[6] & 3) + 1;
    let header_length = if has_crc { 7 + 2 * raw_blocks } else { 7 };
    if frame_length < header_length {
        return None;
    }
    Some(AdtsHeader {
        mpeg2,
        has_crc,
        object_type: profile + 1,
        sampling_index,
        sample_rate: SAMPLE_RATES[usize::from(sampling_index)],
        channel_config,
        frame_length,
        buffer_fullness,
        raw_blocks,
        header_length,
    })
}

/// A 7-byte ADTS header (MPEG-4, no CRC, one raw data block, variable-rate buffer fullness)
/// for a raw access unit of `payload_len` bytes. The profile is the core object type; SBR is
/// left implicit, as ADTS requires. A non-standard rate uses the nearest table index.
pub fn adts_header(asc: &AudioSpecificConfig, payload_len: usize) -> [u8; 7] {
    let len = (payload_len + 7).min(0x1FFF);
    let profile = asc.object_type.clamp(1, 4) - 1;
    let sf = if asc.sampling_index < 13 {
        asc.sampling_index
    } else {
        nearest_sampling_index(asc.sample_rate)
    };
    let ch = asc.channel_config & 7;
    let fullness: usize = 0x7FF;
    [
        0xFF,
        0xF1,
        (profile << 6) | (sf << 2) | (ch >> 2),
        ((ch & 3) << 6) | (len >> 11) as u8,
        (len >> 3) as u8,
        (((len & 7) << 5) | (fullness >> 6)) as u8,
        ((fullness & 0x3F) << 2) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let asc = AudioSpecificConfig::lc(48000, 6);
        let h = adts_header(&asc, 300);
        let p = parse_adts(&h).unwrap();
        assert_eq!(p.frame_length, 307);
        assert_eq!(p.header_length, 7);
        assert_eq!(p.payload_length(), 300);
        assert_eq!(p.sample_rate, 48000);
        assert_eq!(p.channel_config, 6);
        assert_eq!(p.object_type, 2);
        assert_eq!(p.raw_blocks, 1);
        assert_eq!(p.buffer_fullness, 0x7FF);
        assert!(!p.has_crc && !p.mpeg2);
        assert_eq!(p.to_asc(), asc);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_adts(&[0xFF, 0xF1, 0x50]).is_none());
        assert!(parse_adts(&[0, 0, 0, 0, 0, 0, 0]).is_none());
        // sampling index 13
        assert!(parse_adts(&[0xFF, 0xF1, 0x74, 0x80, 0x10, 0x1F, 0xFC]).is_none());
        // frame length 3 < header
        assert!(parse_adts(&[0xFF, 0xF1, 0x50, 0x80, 0x00, 0x7F, 0xFC]).is_none());
    }
}
