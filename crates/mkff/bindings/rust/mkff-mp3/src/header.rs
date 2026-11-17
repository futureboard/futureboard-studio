//! MPEG audio frame headers (ISO/IEC 11172-3 2.4.2.3, ISO/IEC 13818-3 2.4.2.3 and the
//! MPEG-2.5 extension).

/// MPEG audio version (the `ID` bit plus the MPEG-2.5 extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MpegVersion {
    /// ISO/IEC 11172-3: 32, 44.1 and 48 kHz.
    Mpeg1,
    /// ISO/IEC 13818-3 low sampling frequencies: 16, 22.05 and 24 kHz.
    Mpeg2,
    /// The unofficial MPEG-2.5 extension: 8, 11.025 and 12 kHz.
    Mpeg25,
}

impl MpegVersion {
    /// `true` for MPEG-2 and MPEG-2.5 (one granule per Layer III frame, LSF tables).
    pub fn is_lsf(self) -> bool {
        self != MpegVersion::Mpeg1
    }
}

/// The `mode` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelMode {
    Stereo,
    /// Intensity and / or M/S stereo, selected per frame by `mode_extension`.
    JointStereo,
    /// Two independent mono channels.
    DualChannel,
    Mono,
}

/// A parsed 4-byte MPEG audio frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: MpegVersion,
    /// 1, 2 or 3.
    pub layer: u8,
    /// `true` when a 16-bit CRC follows the header (`protection_bit == 0`).
    pub protection: bool,
    /// Bit rate in kbit/s; 0 for free format.
    pub bitrate_kbps: u32,
    /// Sampling frequency in Hz.
    pub sample_rate: u32,
    pub padding: bool,
    pub channel_mode: ChannelMode,
    /// `mode_extension`: the joint stereo bound (layers I/II) or the M/S (bit 1) and intensity
    /// (bit 0) flags (layer III). Meaningful only in joint stereo mode.
    pub mode_extension: u8,
    /// Frame length in bytes including the header; 0 for free format (unknown from the header).
    pub frame_len_bytes: usize,
    /// PCM samples per channel the frame decodes to (384, 1152 or 576).
    pub samples_per_frame: usize,
    /// 1 or 2.
    pub channels: usize,
    pub copyright: bool,
    pub original: bool,
    /// The `emphasis` field (0 none, 1 50/15 µs, 3 CCITT J.17).
    pub emphasis: u8,
}

const BITRATES_V1: [[u16; 15]; 3] = [
    [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ],
    [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ],
];
const BITRATES_V2: [[u16; 15]; 3] = [
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];
const SAMPLE_RATES: [[u32; 3]; 3] = [
    [44100, 48000, 32000],
    [22050, 24000, 16000],
    [11025, 12000, 8000],
];

/// The bit-rate table (index 1..=14, kbit/s) for a version / layer.
pub(crate) fn bitrate_table(version: MpegVersion, layer: u8) -> &'static [u16; 15] {
    let l = usize::from(layer.clamp(1, 3) - 1);
    if version == MpegVersion::Mpeg1 {
        &BITRATES_V1[l]
    } else {
        &BITRATES_V2[l]
    }
}

pub(crate) fn sample_rate_index(version: MpegVersion, rate: u32) -> Option<usize> {
    let v = match version {
        MpegVersion::Mpeg1 => 0,
        MpegVersion::Mpeg2 => 1,
        MpegVersion::Mpeg25 => 2,
    };
    SAMPLE_RATES[v].iter().position(|&r| r == rate)
}

/// Index 0..9 over all nine sampling frequencies (44.1, 48, 32, 22.05, 24, 16, 11.025, 12,
/// 8 kHz), used by the Layer III scale factor band tables.
pub(crate) fn sfreq_index(rate: u32) -> usize {
    match rate {
        44100 => 0,
        48000 => 1,
        32000 => 2,
        22050 => 3,
        24000 => 4,
        16000 => 5,
        11025 => 6,
        12000 => 7,
        _ => 8,
    }
}

/// Frame length in bytes (including the header) for a bit rate; `bitrate_kbps` 0 gives 0.
pub(crate) fn frame_length(
    version: MpegVersion,
    layer: u8,
    bitrate_kbps: u32,
    sample_rate: u32,
    padding: bool,
) -> usize {
    if bitrate_kbps == 0 || sample_rate == 0 {
        return 0;
    }
    let br = bitrate_kbps as usize * 1000;
    let sr = sample_rate as usize;
    let pad = usize::from(padding);
    match layer {
        1 => (12 * br / sr + pad) * 4,
        3 if version.is_lsf() => 72 * br / sr + pad,
        _ => 144 * br / sr + pad,
    }
}

/// Parse the frame header at the start of `bytes` (at least 4 bytes). Returns `None` when the
/// bytes are not a valid header (no sync, reserved version / layer / rate, bad bit-rate index).
pub fn parse_header(bytes: &[u8]) -> Option<FrameHeader> {
    let b = bytes.get(..4)?;
    let h = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    if h >> 21 != 0x7FF {
        return None;
    }
    let version = match (h >> 19) & 3 {
        0 => MpegVersion::Mpeg25,
        2 => MpegVersion::Mpeg2,
        3 => MpegVersion::Mpeg1,
        _ => return None,
    };
    let layer = match (h >> 17) & 3 {
        1 => 3,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let protection = (h >> 16) & 1 == 0;
    let br_index = ((h >> 12) & 0xF) as usize;
    if br_index == 15 {
        return None;
    }
    let sr_index = ((h >> 10) & 3) as usize;
    if sr_index == 3 {
        return None;
    }
    let vi = match version {
        MpegVersion::Mpeg1 => 0,
        MpegVersion::Mpeg2 => 1,
        MpegVersion::Mpeg25 => 2,
    };
    let sample_rate = SAMPLE_RATES[vi][sr_index];
    let bitrate_kbps = u32::from(bitrate_table(version, layer)[br_index]);
    let padding = (h >> 9) & 1 != 0;
    let channel_mode = match (h >> 6) & 3 {
        0 => ChannelMode::Stereo,
        1 => ChannelMode::JointStereo,
        2 => ChannelMode::DualChannel,
        _ => ChannelMode::Mono,
    };
    let mode_extension = ((h >> 4) & 3) as u8;
    let samples_per_frame = match layer {
        1 => 384,
        3 if version.is_lsf() => 576,
        _ => 1152,
    };
    Some(FrameHeader {
        version,
        layer,
        protection,
        bitrate_kbps,
        sample_rate,
        padding,
        channel_mode,
        mode_extension,
        frame_len_bytes: frame_length(version, layer, bitrate_kbps, sample_rate, padding),
        samples_per_frame,
        channels: if channel_mode == ChannelMode::Mono {
            1
        } else {
            2
        },
        copyright: (h >> 3) & 1 != 0,
        original: (h >> 2) & 1 != 0,
        emphasis: (h & 3) as u8,
    })
}

impl FrameHeader {
    /// Size of the Layer III side information in bytes (17 / 32 for MPEG-1, 9 / 17 for LSF).
    pub fn side_info_len(&self) -> usize {
        match (self.version.is_lsf(), self.channels) {
            (false, 1) => 17,
            (false, _) => 32,
            (true, 1) => 9,
            (true, _) => 17,
        }
    }

    /// Whether `other` belongs to the same stream (same version, layer, rate and channel count).
    pub fn same_stream(&self, other: &FrameHeader) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate == other.sample_rate
            && self.channels == other.channels
    }

    /// Duration of the frame in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.samples_per_frame as f64 / f64::from(self.sample_rate)
    }

    /// The 4 header bytes.
    pub fn to_bytes(&self) -> [u8; 4] {
        let v = match self.version {
            MpegVersion::Mpeg25 => 0u32,
            MpegVersion::Mpeg2 => 2,
            MpegVersion::Mpeg1 => 3,
        };
        let l = 4 - u32::from(self.layer);
        let br = bitrate_table(self.version, self.layer)
            .iter()
            .position(|&b| u32::from(b) == self.bitrate_kbps)
            .unwrap_or(0) as u32;
        let sr = sample_rate_index(self.version, self.sample_rate).unwrap_or(0) as u32;
        let mode = match self.channel_mode {
            ChannelMode::Stereo => 0u32,
            ChannelMode::JointStereo => 1,
            ChannelMode::DualChannel => 2,
            ChannelMode::Mono => 3,
        };
        let h = (0x7FFu32 << 21)
            | (v << 19)
            | (l << 17)
            | (u32::from(!self.protection) << 16)
            | (br << 12)
            | (sr << 10)
            | (u32::from(self.padding) << 9)
            | (mode << 6)
            | (u32::from(self.mode_extension & 3) << 4)
            | (u32::from(self.copyright) << 3)
            | (u32::from(self.original) << 2)
            | u32::from(self.emphasis & 3);
        h.to_be_bytes()
    }
}

/// Find the first plausible frame header in `bytes` and return its offset.
///
/// A candidate is accepted when the header parses and either the next frame's header (at
/// `frame_len_bytes`) parses with the same version / layer / rate / channel count, or the next
/// header lies beyond the end of `bytes` (so it cannot be checked), or the stream is free format.
pub fn find_sync(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i] == 0xFF && bytes[i + 1] & 0xE0 == 0xE0 {
            if let Some(h) = parse_header(&bytes[i..]) {
                if h.frame_len_bytes == 0 {
                    // free format: accept when the next matching free-format header exists or
                    // the data ends before any other header
                    match next_free_format_header(bytes, i, &h) {
                        Some(_) => return Some(i),
                        None if !bytes[i + 4..]
                            .windows(2)
                            .any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0) =>
                        {
                            return Some(i)
                        }
                        None => {}
                    }
                    i += 1;
                    continue;
                }
                let next = i + h.frame_len_bytes;
                if next + 4 > bytes.len() {
                    return Some(i);
                }
                if let Some(n) = parse_header(&bytes[next..]) {
                    if n.same_stream(&h) {
                        return Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Offset of the next free-format header of the same stream after the one at `pos`.
pub(crate) fn next_free_format_header(bytes: &[u8], pos: usize, h: &FrameHeader) -> Option<usize> {
    // the smallest legal frame is far larger than the header plus side information
    let mut j = pos + 4 + if h.layer == 3 { h.side_info_len() } else { 4 };
    while j + 4 <= bytes.len() {
        if bytes[j] == 0xFF && bytes[j + 1] & 0xE0 == 0xE0 {
            if let Some(n) = parse_header(&bytes[j..]) {
                if n.bitrate_kbps == 0 && n.same_stream(h) {
                    return Some(j);
                }
            }
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_headers() {
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz, joint stereo, no CRC
        let h = parse_header(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        assert_eq!(h.version, MpegVersion::Mpeg1);
        assert_eq!(h.layer, 3);
        assert!(!h.protection);
        assert_eq!(h.bitrate_kbps, 128);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.channel_mode, ChannelMode::JointStereo);
        assert_eq!(h.mode_extension, 2);
        assert_eq!(h.frame_len_bytes, 417);
        assert_eq!(h.samples_per_frame, 1152);
        assert_eq!(h.channels, 2);
        assert_eq!(h.to_bytes(), [0xFF, 0xFB, 0x90, 0x64]);
        // padded
        let h = parse_header(&[0xFF, 0xFB, 0x92, 0x64]).unwrap();
        assert_eq!(h.frame_len_bytes, 418);
        // MPEG-2 layer III 64 kbit/s 22.05 kHz mono with CRC
        let h = parse_header(&[0xFF, 0xF2, 0x80, 0xC0]).unwrap();
        assert_eq!(h.version, MpegVersion::Mpeg2);
        assert!(h.protection);
        assert_eq!(h.bitrate_kbps, 64);
        assert_eq!(h.sample_rate, 22050);
        assert_eq!(h.channels, 1);
        assert_eq!(h.samples_per_frame, 576);
        assert_eq!(h.frame_len_bytes, 72 * 64000 / 22050);
        // MPEG-2.5 8 kHz
        let h = parse_header(&[0xFF, 0xE3, 0x18, 0xC4]).unwrap();
        assert_eq!(h.version, MpegVersion::Mpeg25);
        assert_eq!(h.sample_rate, 8000);
        assert_eq!(h.bitrate_kbps, 8);
        // layer I 448 kbit/s 48 kHz
        let h = parse_header(&[0xFF, 0xFF, 0xE4, 0x00]).unwrap();
        assert_eq!(h.layer, 1);
        assert_eq!(h.frame_len_bytes, 12 * 448000 / 48000 * 4);
        assert_eq!(h.samples_per_frame, 384);
        // layer II 384 kbit/s 48 kHz
        let h = parse_header(&[0xFF, 0xFD, 0xE4, 0x00]).unwrap();
        assert_eq!(h.layer, 2);
        assert_eq!(h.frame_len_bytes, 1152);
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(parse_header(&[0xFF, 0xFB, 0x90]).is_none());
        assert!(parse_header(&[0xFF, 0x7B, 0x90, 0x64]).is_none()); // no sync
        assert!(parse_header(&[0xFF, 0xEB, 0x90, 0x64]).is_none()); // reserved version
        assert!(parse_header(&[0xFF, 0xF9, 0x90, 0x64]).is_none()); // reserved layer
        assert!(parse_header(&[0xFF, 0xFB, 0xF0, 0x64]).is_none()); // bad bit rate
        assert!(parse_header(&[0xFF, 0xFB, 0x9C, 0x64]).is_none()); // reserved rate
    }

    #[test]
    fn sync_search() {
        let mut v = vec![0u8, 0xFF, 0xFB, 0x00];
        let start = v.len();
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        v.resize(start + 417, 0);
        v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        assert_eq!(find_sync(&v), Some(start));
        // a fake sync followed by a mismatching header is skipped
        let mut w = vec![0xFF, 0xFB, 0x10, 0x64];
        w.resize(200, 0);
        assert_eq!(find_sync(&w), None);
    }
}
