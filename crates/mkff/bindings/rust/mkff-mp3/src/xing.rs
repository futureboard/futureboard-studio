//! Xing / Info headers with the LAME extension, and the Fraunhofer VBRI header: the first frame
//! of most MP3 files, holding the frame count, the stream size, a seek table and the encoder
//! delay / padding needed for gapless playback.
//!
//! Gapless convention (LAME): the decoded stream (without the tag frame) starts with
//! `encoder_delay + 529` samples of delay, and `padding` counts the samples after the end of the
//! audio from the encoder's point of view, so a decoder drops `encoder_delay + 529` samples at
//! the start and `padding - 529` at the end (when `padding >= 529`).

use crate::header::{bitrate_table, frame_length, parse_header, ChannelMode, FrameHeader};

/// Decoder delay (filterbank) that the LAME tag's delay field does not include.
pub const DECODER_DELAY: u32 = 529;

/// Which header the frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XingKind {
    /// `Xing`: variable bit rate.
    Xing,
    /// `Info`: constant bit rate (written by LAME for CBR files).
    Info,
    /// Fraunhofer `VBRI`.
    Vbri,
}

/// Contents of a Xing / Info / VBRI header frame.
#[derive(Clone, Debug, PartialEq)]
pub struct XingInfo {
    pub kind: XingKind,
    /// Number of audio frames (the tag frame itself not included).
    pub frames: Option<u32>,
    /// Stream size in bytes (normally including the tag frame).
    pub bytes: Option<u32>,
    /// Xing seek table: entry `i` is the byte position (in 1/256 of `bytes`) of `i` percent of
    /// the duration.
    pub toc: Option<[u8; 100]>,
    /// Xing quality indicator (0 best .. 100 worst).
    pub quality: Option<u32>,
    /// Encoder version string of the LAME extension (`"LAME3.100"`, `"Lavc60.3"`, …).
    pub encoder: Option<String>,
    /// Samples the encoder added at the start (LAME tag / VBRI delay), excluding the 529-sample
    /// decoder delay.
    pub encoder_delay: Option<u32>,
    /// Samples the encoder added at the end (LAME tag).
    pub padding: Option<u32>,
    /// LAME tag low-pass filter frequency in Hz.
    pub lowpass_hz: Option<u32>,
    /// LAME tag: bytes from the tag frame to the end of the last audio frame.
    pub music_length: Option<u32>,
    /// LAME tag: CRC-16 of the audio frames.
    pub music_crc: Option<u16>,
    /// LAME tag: whether the tag's own CRC matched (`None` without a LAME extension).
    pub tag_crc_ok: Option<bool>,
}

impl XingInfo {
    /// Total decoded samples after gapless trimming, when the frame count and LAME delay /
    /// padding are known.
    pub fn gapless_samples(&self, samples_per_frame: u32) -> Option<u64> {
        let frames = u64::from(self.frames?);
        let total = frames * u64::from(samples_per_frame);
        let delay = u64::from(self.encoder_delay?) + u64::from(DECODER_DELAY);
        let pad = u64::from(self.padding?).saturating_sub(u64::from(DECODER_DELAY));
        Some(total.saturating_sub(delay + pad))
    }

    /// Byte offset (from the start of the tag frame) for a position in `0.0..=1.0` of the
    /// duration, interpolating the TOC; `None` without a TOC and byte count.
    pub fn seek_offset(&self, fraction: f64) -> Option<u64> {
        let toc = self.toc.as_ref()?;
        let bytes = f64::from(self.bytes?);
        let p = (fraction.clamp(0.0, 1.0) * 100.0).min(99.999);
        let i = p as usize;
        let a = f64::from(toc[i]);
        let b = if i < 99 { f64::from(toc[i + 1]) } else { 256.0 };
        Some(((a + (b - a) * (p - i as f64)) / 256.0 * bytes) as u64)
    }
}

/// Byte offset of the Xing / Info tag in a frame with this header.
fn xing_offset(h: &FrameHeader) -> usize {
    4 + if h.protection { 2 } else { 0 } + h.side_info_len()
}

/// Whether `frame` is a tag frame (Xing, Info or VBRI) rather than audio.
pub(crate) fn is_info_frame(frame: &[u8]) -> bool {
    let Some(h) = parse_header(frame) else {
        return false;
    };
    if h.layer != 3 {
        return false;
    }
    let o = xing_offset(&h);
    matches!(frame.get(o..o + 4), Some(b"Xing" | b"Info")) || frame.get(36..40) == Some(b"VBRI")
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

/// CRC-16/ARC (reflected polynomial 0x8005, initial value 0), used by the LAME tag.
pub(crate) fn crc16_lame(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Parse the Xing / Info (with the LAME extension when present) or VBRI header of `frame`, the
/// complete first frame of a stream. Returns `None` when the frame carries no such header.
pub fn parse_xing(frame: &[u8]) -> Option<XingInfo> {
    let h = parse_header(frame)?;
    if h.layer != 3 {
        return None;
    }
    let o = xing_offset(&h);
    let kind = match frame.get(o..o + 4) {
        Some(b"Xing") => XingKind::Xing,
        Some(b"Info") => XingKind::Info,
        _ => return parse_vbri(frame),
    };
    let mut info = XingInfo {
        kind,
        frames: None,
        bytes: None,
        toc: None,
        quality: None,
        encoder: None,
        encoder_delay: None,
        padding: None,
        lowpass_hz: None,
        music_length: None,
        music_crc: None,
        tag_crc_ok: None,
    };
    let flags = be32(frame, o + 4)?;
    let mut p = o + 8;
    if flags & 1 != 0 {
        info.frames = Some(be32(frame, p)?);
        p += 4;
    }
    if flags & 2 != 0 {
        info.bytes = Some(be32(frame, p)?);
        p += 4;
    }
    if flags & 4 != 0 {
        let t = frame.get(p..p + 100)?;
        let mut toc = [0u8; 100];
        toc.copy_from_slice(t);
        info.toc = Some(toc);
        p += 100;
    }
    if flags & 8 != 0 {
        info.quality = Some(be32(frame, p)?);
        p += 4;
    }
    // LAME extension: 36 bytes starting with a 9-byte encoder string
    if let Some(ext) = frame.get(p..p + 36) {
        let enc = &ext[..9];
        let printable = enc.iter().all(|&c| c == 0 || (0x20..0x7F).contains(&c));
        if printable && enc[0].is_ascii_alphabetic() {
            let s: String = enc
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as char)
                .collect();
            info.encoder = Some(s.trim_end().to_string());
            info.lowpass_hz = Some(u32::from(ext[10]) * 100);
            let dp = u32::from(ext[21]) << 16 | u32::from(ext[22]) << 8 | u32::from(ext[23]);
            info.encoder_delay = Some(dp >> 12);
            info.padding = Some(dp & 0xFFF);
            info.music_length = be32(ext, 28);
            info.music_crc = be16(ext, 32);
            let stored = be16(ext, 34)?;
            info.tag_crc_ok = Some(crc16_lame(0, &frame[..p + 34]) == stored);
        }
    }
    Some(info)
}

fn parse_vbri(frame: &[u8]) -> Option<XingInfo> {
    if frame.get(36..40)? != b"VBRI" {
        return None;
    }
    // version(2) delay(2) quality(2) bytes(4) frames(4) toc entries(2) scale(2) entry size(2)
    // frames per entry(2), then the table
    Some(XingInfo {
        kind: XingKind::Vbri,
        frames: be32(frame, 50),
        bytes: be32(frame, 46),
        toc: None,
        quality: be16(frame, 44).map(u32::from),
        encoder: None,
        encoder_delay: be16(frame, 42).map(u32::from),
        padding: None,
        lowpass_hz: None,
        music_length: None,
        music_crc: None,
        tag_crc_ok: None,
    })
}

/// What to write into an Info / Xing header frame (see [`xing_frame`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XingFrameInfo {
    /// Audio frames in the stream (the tag frame not included).
    pub frames: u32,
    /// Bytes of the audio frames (the tag frame not included).
    pub audio_bytes: u64,
    /// Samples of delay the encoder inserted, excluding the 529-sample decoder delay (the LAME
    /// tag convention; `Mp3Encoder::priming()` minus 529).
    pub encoder_delay: u32,
    /// Samples after the end of the input in the last frame plus the decoder delay
    /// (`frames * samples_per_frame - encoder_delay - input_samples`).
    pub padding: u32,
    /// `true` writes `Info` (constant bit rate), `false` writes `Xing`.
    pub cbr: bool,
    /// Average bit rate written into the LAME extension, kbit/s.
    pub bitrate_kbps: u32,
    /// Low-pass frequency in Hz (0 = unknown).
    pub lowpass_hz: u32,
    /// CRC-16/ARC of the audio frames (see [`crc16_audio`]).
    pub music_crc: u16,
    /// Seek table; `None` writes a linear one (exact for CBR).
    pub toc: Option<[u8; 100]>,
    /// The 9-byte encoder version string of the LAME extension. Players only read the delay /
    /// padding fields when it starts with `LAME` (most) or `Lavc` / `Lavf` (FFmpeg); the
    /// default, [`XingFrameInfo::LAME_COMPATIBLE`], is `"LAMEmkff "` (the `LAME` prefix, not a LAME
    /// version) so that the gapless fields
    /// are honoured. Use your own string if you do not need that.
    pub encoder: [u8; 9],
}

impl XingFrameInfo {
    /// Encoder string recognised by players for the LAME extension.
    pub const LAME_COMPATIBLE: [u8; 9] = *b"LAMEmkff ";
}

impl Default for XingFrameInfo {
    fn default() -> Self {
        Self {
            frames: 0,
            audio_bytes: 0,
            encoder_delay: 0,
            padding: 0,
            cbr: true,
            bitrate_kbps: 0,
            lowpass_hz: 0,
            music_crc: 0,
            toc: None,
            encoder: Self::LAME_COMPATIBLE,
        }
    }
}

/// CRC-16/ARC over audio frame bytes, as stored in the LAME tag's music CRC; continue it over
/// consecutive frames by passing the previous value (start with 0).
pub fn crc16_audio(crc: u16, data: &[u8]) -> u16 {
    crc16_lame(crc, data)
}

/// Build a Layer III Info / Xing header frame (with the LAME extension) for a stream whose
/// frames look like `stream` (version, sampling rate and channel mode are copied; the bit rate
/// is the stream's when the tag fits, the next larger one otherwise). The frame decodes to
/// silence in decoders that do not know the tag.
pub fn xing_frame(stream: &FrameHeader, info: &XingFrameInfo) -> Vec<u8> {
    let mut h = *stream;
    h.layer = 3;
    h.protection = false;
    h.padding = false;
    h.mode_extension = 0;
    if h.channel_mode == ChannelMode::DualChannel {
        h.channel_mode = ChannelMode::Stereo;
    }
    let need = xing_offset(&h) + 120 + 36;
    let rates = bitrate_table(h.version, 3);
    let mut len = 0;
    for &br in rates.iter().skip(1) {
        let br = u32::from(br);
        if br < stream.bitrate_kbps && stream.bitrate_kbps != 0 {
            continue;
        }
        let l = frame_length(h.version, 3, br, h.sample_rate, false);
        if l >= need {
            h.bitrate_kbps = br;
            len = l;
            break;
        }
    }
    if len == 0 {
        // even the largest rate is too small (cannot happen for valid rates); use the largest
        h.bitrate_kbps = u32::from(rates[14]);
        len = frame_length(h.version, 3, h.bitrate_kbps, h.sample_rate, false).max(need);
    }
    h.frame_len_bytes = len;
    let mut f = vec![0u8; len];
    f[..4].copy_from_slice(&h.to_bytes());
    let o = xing_offset(&h);
    f[o..o + 4].copy_from_slice(if info.cbr { b"Info" } else { b"Xing" });
    f[o + 4..o + 8].copy_from_slice(&0x0Fu32.to_be_bytes());
    f[o + 8..o + 12].copy_from_slice(&info.frames.to_be_bytes());
    let total = (info.audio_bytes + len as u64).min(u64::from(u32::MAX)) as u32;
    f[o + 12..o + 16].copy_from_slice(&total.to_be_bytes());
    let toc = info.toc.unwrap_or_else(|| {
        let mut t = [0u8; 100];
        for (i, v) in t.iter_mut().enumerate() {
            *v = (i * 256 / 100) as u8;
        }
        t
    });
    f[o + 16..o + 116].copy_from_slice(&toc);
    // quality indicator (LAME writes 100 - 10 * quality); 0 here
    let p = o + 120;
    let ext = &mut f[p..p + 36];
    ext[..9].copy_from_slice(&info.encoder);
    ext[9] = if info.cbr { 1 } else { 3 };
    ext[10] = (info.lowpass_hz / 100).min(255) as u8;
    ext[20] = info.bitrate_kbps.min(255) as u8;
    let d = info.encoder_delay.min(4095);
    let pd = info.padding.min(4095);
    ext[21] = (d >> 4) as u8;
    ext[22] = ((d & 15) << 4 | pd >> 8) as u8;
    ext[23] = (pd & 0xFF) as u8;
    ext[24] = match stream.channel_mode {
        ChannelMode::Mono => 0,
        ChannelMode::Stereo => 1 << 2,
        ChannelMode::DualChannel => 2 << 2,
        ChannelMode::JointStereo => 3 << 2,
    } | match stream.sample_rate {
        0..=32000 => 0,
        32001..=44100 => 1,
        _ => 2,
    } << 6;
    let music_len = total;
    ext[28..32].copy_from_slice(&music_len.to_be_bytes());
    ext[32..34].copy_from_slice(&info.music_crc.to_be_bytes());
    let crc = crc16_lame(0, &f[..p + 34]);
    f[p + 34..p + 36].copy_from_slice(&crc.to_be_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let stream = parse_header(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        let info = XingFrameInfo {
            frames: 1234,
            audio_bytes: 1234 * 417,
            encoder_delay: 576,
            padding: 1000,
            bitrate_kbps: 128,
            lowpass_hz: 17000,
            music_crc: 0xBEEF,
            ..Default::default()
        };
        let f = xing_frame(&stream, &info);
        assert_eq!(f.len(), 417);
        assert!(is_info_frame(&f));
        let x = parse_xing(&f).unwrap();
        assert_eq!(x.kind, XingKind::Info);
        assert_eq!(x.frames, Some(1234));
        assert_eq!(x.bytes, Some(1234 * 417 + 417));
        assert_eq!(x.encoder.as_deref().map(str::trim_end), Some("LAMEmkff"));
        assert_eq!(x.encoder_delay, Some(576));
        assert_eq!(x.padding, Some(1000));
        assert_eq!(x.lowpass_hz, Some(17000));
        assert_eq!(x.music_crc, Some(0xBEEF));
        assert_eq!(x.tag_crc_ok, Some(true));
        assert_eq!(
            x.gapless_samples(1152),
            Some(1234 * 1152 - 576 - 529 - (1000 - 529))
        );
        assert_eq!(x.seek_offset(0.0), Some(0));
        // small frames grow to the next bit rate that fits the tag
        let low = parse_header(&[0xFF, 0xFB, 0x10, 0xC4]).unwrap(); // 32 kbit/s mono
        let f = xing_frame(&low, &info);
        assert!(f.len() >= 4 + 17 + 156);
        assert!(parse_xing(&f).is_some());
    }

    #[test]
    fn rejects_audio_frames() {
        let mut f = vec![0xFF, 0xFB, 0x90, 0x64];
        f.resize(417, 0x55);
        assert!(parse_xing(&f).is_none());
        assert!(!is_info_frame(&f));
        assert!(parse_xing(&[0xFF, 0xFB]).is_none());
    }
}
