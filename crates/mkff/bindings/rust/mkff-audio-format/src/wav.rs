//! RIFF WAVE / RF64 / BW64 reading and writing.

use std::io::{Seek, SeekFrom, Write};

use mkff_audio_core::{AudioBuffer, CodecId, Error, Result};

use crate::io::{le16, le32, patch, Bytes, Source};
use crate::pcm::{push_sample, PcmCoding, PcmFormat};
use crate::{DemuxPacket, Demuxer, Track};

/// Frames per packet for PCM containers.
pub(crate) const PCM_PACKET_FRAMES: u64 = 4096;

/// A demuxer over one contiguous region of interleaved PCM (WAV and AIFF).
pub(crate) struct PcmRegion {
    pub src: Source,
    pub name: &'static str,
    pub tracks: Vec<Track>,
    pub data_start: u64,
    pub block_align: u64,
    pub frames: u64,
    pub next: u64,
}

impl PcmRegion {
    pub fn new(
        src: Source,
        name: &'static str,
        mut track: Track,
        data_start: u64,
        data_len: u64,
    ) -> Result<Self> {
        let fmt = track
            .pcm
            .ok_or_else(|| Error::invalid("PCM track without format"))?;
        let block_align = fmt.bytes_per_sample() as u64 * u64::from(track.channels);
        if block_align == 0 {
            return Err(Error::invalid("PCM with zero block size"));
        }
        let avail = src.len().saturating_sub(data_start);
        let frames = data_len.min(avail) / block_align;
        track.duration = Some(frames);
        Ok(Self {
            src,
            name,
            tracks: vec![track],
            data_start,
            block_align,
            frames,
            next: 0,
        })
    }
}

impl Demuxer for PcmRegion {
    fn format_name(&self) -> &str {
        self.name
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<DemuxPacket>> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let n = PCM_PACKET_FRAMES.min(self.frames - self.next);
        self.src
            .seek(self.data_start + self.next * self.block_align);
        let mut data = self.src.read_upto((n * self.block_align) as usize)?;
        let got = data.len() as u64 / self.block_align;
        if got == 0 {
            self.frames = self.next;
            return Ok(None);
        }
        data.truncate((got * self.block_align) as usize);
        let pts = self.next as i64;
        self.next += got;
        Ok(Some(DemuxPacket {
            track: 0,
            data,
            pts,
            duration: got as u32,
            keyframe: true,
        }))
    }

    fn seek(&mut self, track: usize, pts: i64) -> Result<i64> {
        if track != 0 {
            return Err(Error::InvalidArgument(format!("no track {track}")));
        }
        self.next = (pts.max(0) as u64).min(self.frames);
        Ok(self.next as i64)
    }
}

const KSDATAFORMAT_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Parse a `fmt ` chunk body into a PCM track.
fn parse_fmt(body: &[u8]) -> Result<Track> {
    let mut b = Bytes::new(body);
    let mut tag = b.le_u16()?;
    let channels = u32::from(b.le_u16()?);
    let rate = b.le_u32()?;
    let _byte_rate = b.le_u32()?;
    let block_align = u32::from(b.le_u16()?);
    let bits = u32::from(b.le_u16().unwrap_or(0));
    let mut valid_bits = bits;
    let mut mask = None;
    if tag == 0xFFFE {
        let cb = b.le_u16()?;
        if cb < 22 {
            return Err(Error::invalid("short WAVE_FORMAT_EXTENSIBLE"));
        }
        let v = b.le_u16()?;
        if v != 0 {
            valid_bits = u32::from(v);
        }
        let m = b.le_u32()?;
        if m != 0 {
            mask = Some(m);
        }
        let guid = b.bytes(16)?;
        tag = le16(guid, 0);
        if guid[2..] != KSDATAFORMAT_TAIL {
            return Err(Error::unsupported(
                "WAVE_FORMAT_EXTENSIBLE with a non-standard sub-format",
            ));
        }
    }
    if channels == 0 || rate == 0 {
        return Err(Error::invalid("WAV with 0 channels or 0 Hz"));
    }
    // container bits: from the block alignment when it is consistent
    let slot = if block_align > 0 && block_align % channels == 0 && (block_align / channels) <= 8 {
        block_align / channels * 8
    } else {
        bits.div_ceil(8) * 8
    };
    let slot = if slot < bits.div_ceil(8) * 8 {
        bits.div_ceil(8) * 8
    } else {
        slot
    };
    let fmt = match tag {
        1 => PcmFormat {
            bits: slot,
            float: false,
            big_endian: false,
            signed: slot > 8,
            coding: PcmCoding::Linear,
        },
        3 => PcmFormat::float(slot, false),
        6 => PcmFormat::alaw(),
        7 => PcmFormat::mulaw(),
        t => return Err(Error::unsupported(format!("WAV format tag 0x{t:04X}"))),
    };
    if !fmt.is_valid() {
        return Err(Error::unsupported(format!(
            "WAV sample format {} bits",
            slot
        )));
    }
    let mut t = Track::new(CodecId::Pcm, rate, channels, format!("PCM {}", fmt.label()));
    t.pcm = Some(fmt);
    t.bits_per_sample = Some(if fmt.coding == PcmCoding::Linear {
        valid_bits.min(slot).max(1)
    } else {
        8
    });
    t.channel_mask = mask;
    Ok(t)
}

/// WAV demuxer constructor.
pub(crate) struct WavDemuxer;

impl WavDemuxer {
    #[allow(clippy::new_ret_no_self)]
    pub fn open(mut src: Source) -> Result<PcmRegion> {
        let mut h = [0u8; 12];
        src.read_at(0, &mut h)?;
        let rf64 = &h[..4] == b"RF64" || &h[..4] == b"BW64";
        if !(rf64 || &h[..4] == b"RIFF") || &h[8..12] != b"WAVE" {
            return Err(Error::invalid("not a WAVE file"));
        }
        let len = src.len();
        let mut pos = 12u64;
        let mut ds64_data: Option<u64> = None;
        let mut track: Option<Track> = None;
        let mut data: Option<(u64, u64, bool)> = None; // start, length, length known
        while pos + 8 <= len {
            let mut ck = [0u8; 8];
            if src.read_at(pos, &mut ck)? < 8 {
                break;
            }
            let id = [ck[0], ck[1], ck[2], ck[3]];
            let size32 = le32(&ck, 4);
            let mut size = u64::from(size32);
            match &id {
                b"ds64" => {
                    let body = src.read_vec(size.min(4096).min(len - pos - 8))?;
                    if body.len() >= 24 {
                        ds64_data =
                            Some(u64::from(le32(&body, 8)) | u64::from(le32(&body, 12)) << 32);
                    }
                }
                b"fmt " => {
                    let body = src.read_vec(size.min(4096).min(len - pos - 8))?;
                    track = Some(parse_fmt(&body)?);
                }
                b"data" => {
                    let start = pos + 8;
                    let mut known = true;
                    if rf64 && size32 == 0xFFFF_FFFF {
                        match ds64_data {
                            Some(d) => size = d,
                            None => known = false,
                        }
                    } else if size32 == 0 || size32 == 0xFFFF_FFFF {
                        // a streamed file whose header was never patched
                        known = false;
                    }
                    if !known {
                        size = len - start;
                    }
                    data = Some((start, size, known));
                    if track.is_some() || !known || start + size >= len {
                        break;
                    }
                }
                _ => {}
            }
            let next = pos
                .saturating_add(8)
                .saturating_add(size)
                .saturating_add(size & 1);
            if next <= pos {
                break;
            }
            pos = next;
        }
        let track = track.ok_or_else(|| Error::invalid("WAV without a fmt chunk"))?;
        let (start, size, _) = data.ok_or_else(|| Error::invalid("WAV without a data chunk"))?;
        PcmRegion::new(src, "WAV", track, start, size)
    }
}

/// Sample formats [`WavWriter`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WavSampleFormat {
    /// Signed 16-bit integer.
    Pcm16,
    /// Signed 24-bit integer.
    Pcm24,
    /// Signed 32-bit integer.
    Pcm32,
    /// IEEE 32-bit float.
    Float32,
}

impl WavSampleFormat {
    /// The matching (little-endian) [`PcmFormat`].
    pub fn pcm_format(self) -> PcmFormat {
        match self {
            WavSampleFormat::Pcm16 => PcmFormat::int(16, false),
            WavSampleFormat::Pcm24 => PcmFormat::int(24, false),
            WavSampleFormat::Pcm32 => PcmFormat::int(32, false),
            WavSampleFormat::Float32 => PcmFormat::float(32, false),
        }
    }
}

/// The default WAV channel mask for `channels` channels in WAV order (mono: FC; stereo: FL FR;
/// 3: FL FR FC; 4: FL FR BL BR; 5: FL FR FC BL BR; 5.1: FL FR FC LFE BL BR; 6.1: FL FR FC LFE BC
/// SL SR; 7.1: FL FR FC LFE BL BR SL SR; otherwise the first `channels` speaker bits).
pub fn default_channel_mask(channels: u32) -> u32 {
    match channels {
        1 => 0x4,
        2 => 0x3,
        3 => 0x7,
        4 => 0x33,
        5 => 0x37,
        6 => 0x3F,
        7 => 0x70F,
        8 => 0x63F,
        n if n < 18 => (1u32 << n) - 1,
        _ => 0,
    }
}

const RIFF_LIMIT: u64 = 0xFFFF_FFFF;

/// A WAV writer over `Write + Seek`: the header is written first and patched by
/// [`finish`](WavWriter::finish). Files over 4 GiB become RF64 (EBU Tech 3306): the writer always
/// reserves a 28-byte `JUNK` chunk that turns into `ds64` when needed.
///
/// `WAVE_FORMAT_EXTENSIBLE` (with a channel mask) is used for more than 2 channels or more than
/// 16 bits per sample (float included), plain `WAVEFORMATEX` otherwise.
pub struct WavWriter<W: Write + Seek> {
    w: W,
    fmt: PcmFormat,
    channels: u32,
    start: u64,
    data_size_pos: u64,
    data_bytes: u64,
    scratch: Vec<u8>,
}

impl<W: Write + Seek> WavWriter<W> {
    /// Start a WAV file with the default channel mask.
    pub fn new(w: W, sample_rate: u32, channels: u32, format: WavSampleFormat) -> Result<Self> {
        Self::with_channel_mask(w, sample_rate, channels, format, None)
    }

    /// Start a WAV file with an explicit channel mask (used when the extensible header is).
    pub fn with_channel_mask(
        mut w: W,
        sample_rate: u32,
        channels: u32,
        format: WavSampleFormat,
        channel_mask: Option<u32>,
    ) -> Result<Self> {
        if channels == 0 || channels > 0xFFFF || sample_rate == 0 {
            return Err(Error::InvalidArgument(
                "WAV needs 1..65535 channels and a sample rate".into(),
            ));
        }
        let fmt = format.pcm_format();
        let bits = fmt.bits;
        let block = (bits / 8) * channels;
        if block > 0xFFFF {
            return Err(Error::InvalidArgument("too many channels for WAV".into()));
        }
        let extensible = channels > 2 || bits > 16;
        let tag: u16 = if fmt.float { 3 } else { 1 };
        let start = w.stream_position()?;
        let mut h = Vec::with_capacity(80);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        h.extend_from_slice(b"JUNK");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&[0; 28]);
        h.extend_from_slice(b"fmt ");
        let fmt_len: u32 = if extensible {
            40
        } else if fmt.float {
            18
        } else {
            16
        };
        h.extend_from_slice(&fmt_len.to_le_bytes());
        h.extend_from_slice(&(if extensible { 0xFFFEu16 } else { tag }).to_le_bytes());
        h.extend_from_slice(&(channels as u16).to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&(sample_rate.saturating_mul(block)).to_le_bytes());
        h.extend_from_slice(&(block as u16).to_le_bytes());
        h.extend_from_slice(&(bits as u16).to_le_bytes());
        if extensible {
            h.extend_from_slice(&22u16.to_le_bytes());
            h.extend_from_slice(&(bits as u16).to_le_bytes());
            let mask = channel_mask.unwrap_or_else(|| default_channel_mask(channels));
            h.extend_from_slice(&mask.to_le_bytes());
            h.extend_from_slice(&tag.to_le_bytes());
            h.extend_from_slice(&KSDATAFORMAT_TAIL);
        } else if fmt.float {
            h.extend_from_slice(&0u16.to_le_bytes());
        }
        h.extend_from_slice(b"data\0\0\0\0");
        w.write_all(&h)?;
        let data_size_pos = start + h.len() as u64 - 4;
        Ok(Self {
            w,
            fmt,
            channels,
            start,
            data_size_pos,
            data_bytes: 0,
            scratch: Vec::new(),
        })
    }

    /// Append planar samples (one channel per `buf.channels` entry; extra channels are ignored,
    /// missing ones are written as silence).
    pub fn write(&mut self, buf: &AudioBuffer) -> Result<()> {
        let mut s = std::mem::take(&mut self.scratch);
        s.clear();
        for i in 0..buf.frames() {
            for c in 0..self.channels as usize {
                let v = buf
                    .channels
                    .get(c)
                    .and_then(|ch| ch.get(i))
                    .copied()
                    .unwrap_or(0.0);
                push_sample(&mut s, v, &self.fmt);
            }
        }
        let r = self.write_bytes(&s);
        self.scratch = s;
        r
    }

    /// Append interleaved samples (the length must be a multiple of the channel count).
    pub fn write_interleaved(&mut self, samples: &[f32]) -> Result<()> {
        if !samples.len().is_multiple_of(self.channels as usize) {
            return Err(Error::InvalidArgument("partial frame".into()));
        }
        let mut s = std::mem::take(&mut self.scratch);
        s.clear();
        for &v in samples {
            push_sample(&mut s, v, &self.fmt);
        }
        let r = self.write_bytes(&s);
        self.scratch = s;
        r
    }

    /// Append already-encoded interleaved little-endian samples in the writer's format (whole
    /// frames).
    pub fn write_bytes(&mut self, data: &[u8]) -> Result<()> {
        let block = self.fmt.bytes_per_sample() * self.channels as usize;
        if !data.len().is_multiple_of(block) {
            return Err(Error::InvalidArgument("partial frame".into()));
        }
        self.w.write_all(data)?;
        self.data_bytes += data.len() as u64;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames_written(&self) -> u64 {
        self.data_bytes / (self.fmt.bytes_per_sample() as u64 * u64::from(self.channels))
    }

    /// Pad, patch the sizes (upgrading to RF64 when they exceed 32 bits) and return the writer
    /// positioned at the end of the file.
    pub fn finish(mut self) -> Result<W> {
        if self.data_bytes & 1 == 1 {
            self.w.write_all(&[0])?;
        }
        let end = self.w.stream_position()?;
        let riff = end - self.start - 8;
        if riff > RIFF_LIMIT || self.data_bytes > RIFF_LIMIT {
            patch(&mut self.w, self.start, b"RF64")?;
            patch(&mut self.w, self.start + 4, &u32::MAX.to_le_bytes())?;
            let mut ds = Vec::with_capacity(36);
            ds.extend_from_slice(b"ds64");
            ds.extend_from_slice(&28u32.to_le_bytes());
            ds.extend_from_slice(&riff.to_le_bytes());
            ds.extend_from_slice(&self.data_bytes.to_le_bytes());
            ds.extend_from_slice(&self.frames_written().to_le_bytes());
            ds.extend_from_slice(&0u32.to_le_bytes());
            patch(&mut self.w, self.start + 12, &ds)?;
            patch(&mut self.w, self.data_size_pos, &u32::MAX.to_le_bytes())?;
        } else {
            patch(&mut self.w, self.start + 4, &(riff as u32).to_le_bytes())?;
            patch(
                &mut self.w,
                self.data_size_pos,
                &(self.data_bytes as u32).to_le_bytes(),
            )?;
        }
        self.w.seek(SeekFrom::Start(end))?;
        self.w.flush()?;
        Ok(self.w)
    }
}
