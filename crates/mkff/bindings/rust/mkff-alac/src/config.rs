//! The ALAC magic cookie (`ALACSpecificConfig` and optional channel layout).

use mkff_audio_core::{Error, Result};

/// Length of the `ALACSpecificConfig` structure.
pub const CONFIG_LEN: usize = 24;

/// Largest frame length the decoder accepts (Apple's encoder uses 4096; the field is 32 bits).
pub const MAX_FRAME_LENGTH: u32 = 1 << 20;

/// ALAC stream parameters from the magic cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AlacConfig {
    /// Samples per channel in a full packet (normally 4096).
    pub frame_length: u32,
    /// Bitstream version; only 0 exists.
    pub compatible_version: u8,
    /// Bits per sample: 16, 20, 24 or 32.
    pub bit_depth: u8,
    /// Rice history multiplier (`pb`, normally 40).
    pub pb: u8,
    /// Initial Rice history (`mb`, normally 10).
    pub mb: u8,
    /// Rice parameter limit (`kb`, normally 14).
    pub kb: u8,
    /// Channels, 1..=8.
    pub num_channels: u8,
    /// Longest zero run (normally 255; unused by the decoder).
    pub max_run: u16,
    /// Largest packet in bytes (0 = unknown).
    pub max_frame_bytes: u32,
    /// Average bit rate in bits per second (0 = unknown).
    pub avg_bit_rate: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Core Audio channel layout tag from an optional `chan` atom after the config
    /// (informational: the decoder always uses ALAC's default layout for the channel count).
    pub channel_layout_tag: Option<u32>,
}

impl Default for AlacConfig {
    /// Apple's encoder defaults for 44.1 kHz 16-bit stereo.
    fn default() -> Self {
        Self {
            frame_length: 4096,
            compatible_version: 0,
            bit_depth: 16,
            pb: 40,
            mb: 10,
            kb: 14,
            num_channels: 2,
            max_run: 255,
            max_frame_bytes: 0,
            avg_bit_rate: 0,
            sample_rate: 44100,
            channel_layout_tag: None,
        }
    }
}

impl AlacConfig {
    /// Parse a magic cookie: the 24-byte `ALACSpecificConfig`, optionally preceded by a 12-byte
    /// `frma` atom and / or the 12-byte header of an `alac` atom (size, `alac`, version/flags,
    /// as MP4 sample entries and ffmpeg's extradata carry it), optionally followed by a `chan`
    /// atom (`ALACChannelLayoutInfo`).
    pub fn parse(cookie: &[u8]) -> Result<Self> {
        let mut d = cookie;
        if d.get(4..8) == Some(b"frma") {
            d = &d[12.min(d.len())..];
        }
        if d.get(4..8) == Some(b"alac") {
            d = d.get(12..).ok_or(Error::EndOfData)?;
        }
        let c = d.get(..CONFIG_LEN).ok_or(Error::EndOfData)?;
        let be32 = |i: usize| u32::from_be_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
        let mut cfg = AlacConfig {
            frame_length: be32(0),
            compatible_version: c[4],
            bit_depth: c[5],
            pb: c[6],
            mb: c[7],
            kb: c[8],
            num_channels: c[9],
            max_run: u16::from_be_bytes([c[10], c[11]]),
            max_frame_bytes: be32(12),
            avg_bit_rate: be32(16),
            sample_rate: be32(20),
            channel_layout_tag: None,
        };
        let rest = &d[CONFIG_LEN..];
        if rest.len() >= 24 && &rest[4..8] == b"chan" {
            cfg.channel_layout_tag =
                Some(u32::from_be_bytes([rest[12], rest[13], rest[14], rest[15]]));
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Check the fields the decoder relies on.
    pub fn validate(&self) -> Result<()> {
        if self.compatible_version != 0 {
            return Err(Error::unsupported(format!(
                "ALAC compatible version {}",
                self.compatible_version
            )));
        }
        if !matches!(self.bit_depth, 16 | 20 | 24 | 32) {
            return Err(Error::unsupported(format!(
                "ALAC bit depth {}",
                self.bit_depth
            )));
        }
        if !(1..=8).contains(&self.num_channels) {
            return Err(Error::unsupported(format!(
                "ALAC with {} channels",
                self.num_channels
            )));
        }
        if self.frame_length == 0 || self.frame_length > MAX_FRAME_LENGTH {
            return Err(Error::invalid(format!(
                "ALAC frame length {}",
                self.frame_length
            )));
        }
        // kb = 0 occurs in streams of uncompressed frames only (ffmpeg's compression level 0)
        if self.kb > 31 {
            return Err(Error::invalid(format!("ALAC rice limit {}", self.kb)));
        }
        Ok(())
    }

    /// The 24-byte `ALACSpecificConfig` (big-endian).
    pub fn to_bytes(&self) -> [u8; CONFIG_LEN] {
        let mut b = [0u8; CONFIG_LEN];
        b[0..4].copy_from_slice(&self.frame_length.to_be_bytes());
        b[4] = self.compatible_version;
        b[5] = self.bit_depth;
        b[6] = self.pb;
        b[7] = self.mb;
        b[8] = self.kb;
        b[9] = self.num_channels;
        b[10..12].copy_from_slice(&self.max_run.to_be_bytes());
        b[12..16].copy_from_slice(&self.max_frame_bytes.to_be_bytes());
        b[16..20].copy_from_slice(&self.avg_bit_rate.to_be_bytes());
        b[20..24].copy_from_slice(&self.sample_rate.to_be_bytes());
        b
    }

    /// The config wrapped in an MP4 `alac` box (size, `alac`, version/flags 0, config): the
    /// child box of an MP4 `alac` sample entry.
    pub fn to_mp4_box(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(36);
        v.extend_from_slice(&36u32.to_be_bytes());
        v.extend_from_slice(b"alac");
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(&self.to_bytes());
        v
    }
}

/// Where each ALAC channel (in bitstream order) goes in WAV / SMPTE order, per channel count.
///
/// ALAC's default layouts code C first, then front pairs, surround pairs, a back centre, and LFE
/// last: mono (C); stereo (L R); 3.0 (C L R); 4.0 (C L R Cs); 5.0 (C L R Ls Rs); 5.1 (C L R Ls Rs
/// LFE); 6.1 (C L R Ls Rs Cs LFE); 7.1 (C Lc Rc L R Ls Rs LFE). WAV order follows the channel
/// mask bits: L R C LFE Ls Rs Lc Rc Cs.
pub(crate) const CHANNEL_MAP: [&[usize]; 8] = [
    &[0],
    &[0, 1],
    &[2, 0, 1],
    &[2, 0, 1, 3],
    &[2, 0, 1, 3, 4],
    &[2, 0, 1, 4, 5, 3],
    &[2, 0, 1, 4, 5, 6, 3],
    &[2, 6, 7, 0, 1, 4, 5, 3],
];

/// Element types (3-bit tags) in ALAC channel-element order for `channels` channels:
/// 0 = SCE, 1 = CPE, 3 = LFE.
pub(crate) fn element_layout(channels: usize) -> &'static [u8] {
    match channels {
        1 => &[0],
        2 => &[1],
        3 => &[0, 1],
        4 => &[0, 1, 0],
        5 => &[0, 1, 1],
        6 => &[0, 1, 1, 3],
        7 => &[0, 1, 1, 0, 3],
        _ => &[0, 1, 1, 1, 3],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_forms() {
        let cfg = AlacConfig {
            bit_depth: 24,
            num_channels: 6,
            sample_rate: 96000,
            ..Default::default()
        };
        let bare = cfg.to_bytes();
        assert_eq!(AlacConfig::parse(&bare).unwrap(), cfg);
        let mp4 = cfg.to_mp4_box();
        assert_eq!(AlacConfig::parse(&mp4).unwrap(), cfg);
        let mut wave = vec![0, 0, 0, 12];
        wave.extend_from_slice(b"frmaalac");
        wave.extend_from_slice(&mp4);
        // chan atom: layout tag kAudioChannelLayoutTag_MPEG_5_1_D = (121 << 16) | 6
        wave.extend_from_slice(&[0, 0, 0, 24]);
        wave.extend_from_slice(b"chan");
        wave.extend_from_slice(&[0; 4]);
        wave.extend_from_slice(&((121u32 << 16) | 6).to_be_bytes());
        wave.extend_from_slice(&[0; 8]);
        let parsed = AlacConfig::parse(&wave).unwrap();
        assert_eq!(parsed.channel_layout_tag, Some((121 << 16) | 6));
        assert_eq!(
            AlacConfig {
                channel_layout_tag: None,
                ..parsed
            },
            cfg
        );
        for len in 0..bare.len() {
            assert!(AlacConfig::parse(&bare[..len]).is_err());
        }
        let mut bad = bare;
        bad[5] = 8;
        assert!(AlacConfig::parse(&bad).is_err());
        bad[5] = 16;
        bad[9] = 9;
        assert!(AlacConfig::parse(&bad).is_err());
    }

    #[test]
    fn layouts_cover_all_channels() {
        for ch in 1..=8 {
            let els = element_layout(ch);
            let n: usize = els.iter().map(|&e| if e == 1 { 2 } else { 1 }).sum();
            assert_eq!(n, ch);
            let mut seen = CHANNEL_MAP[ch - 1].to_vec();
            seen.sort_unstable();
            assert_eq!(seen, (0..ch).collect::<Vec<_>>());
        }
    }
}
