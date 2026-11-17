//! Raw MPEG-1/2/2.5 audio (layers I, II, III): frame headers, Xing/Info/LAME and VBRI tags,
//! demuxing and writing.

use std::io::{Seek, SeekFrom, Write};

use mkff_audio_core::{CodecId, Error, Result};

use crate::io::{be16, be32, patch, Source};
use crate::{id3, DemuxPacket, Demuxer, Track};

const BITRATES: [[[u16; 15]; 3]; 2] = [
    // MPEG-1: layer I, II, III
    [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ],
    // MPEG-2 / 2.5: layer I, II & III
    [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];

const RATES: [u32; 3] = [44100, 48000, 32000];

/// A parsed MPEG audio frame header (ISO/IEC 11172-3 / 13818-3, plus the MPEG-2.5 extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpegAudioHeader {
    /// The raw 32-bit header.
    pub raw: u32,
    /// 10 for MPEG-1, 20 for MPEG-2, 25 for MPEG-2.5.
    pub version: u8,
    /// 1, 2 or 3.
    pub layer: u8,
    /// A 16-bit CRC follows the header.
    pub crc: bool,
    /// Bit rate in bits per second (free-format streams are not supported).
    pub bitrate: u32,
    /// Bit rate index (1..=14).
    pub bitrate_index: u8,
    /// Sampling rate in Hz.
    pub sample_rate: u32,
    /// The frame has one padding slot.
    pub padding: bool,
    /// 0 stereo, 1 joint stereo, 2 dual channel, 3 mono.
    pub mode: u8,
    /// 1 (mono) or 2.
    pub channels: u32,
    /// Frame length in bytes including the header.
    pub frame_len: usize,
    /// Samples per channel decoded from one frame.
    pub samples: u32,
}

impl MpegAudioHeader {
    /// Parse the header at the start of `b` (`None` if it is not a valid, supported header).
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 4 {
            return None;
        }
        Self::from_u32(be32(b, 0))
    }

    /// Parse a header word.
    pub fn from_u32(h: u32) -> Option<Self> {
        if h >> 21 != 0x7FF {
            return None;
        }
        let version = match (h >> 19) & 3 {
            0 => 25,
            2 => 20,
            3 => 10,
            _ => return None,
        };
        let layer = match (h >> 17) & 3 {
            1 => 3,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let bri = ((h >> 12) & 0xF) as usize;
        let sri = ((h >> 10) & 3) as usize;
        if bri == 0 || bri == 15 || sri == 3 {
            return None;
        }
        let lsf = version != 10;
        let kbps = BITRATES[usize::from(lsf)][usize::from(layer - 1)][bri];
        let bitrate = u32::from(kbps) * 1000;
        let sample_rate = RATES[sri]
            >> match version {
                10 => 0,
                20 => 1,
                _ => 2,
            };
        let padding = (h >> 9) & 1 == 1;
        let mode = ((h >> 6) & 3) as u8;
        let (samples, frame_len) = match layer {
            1 => (384, ((12 * bitrate / sample_rate) + u32::from(padding)) * 4),
            2 => (1152, 144 * bitrate / sample_rate + u32::from(padding)),
            _ if lsf => (576, 72 * bitrate / sample_rate + u32::from(padding)),
            _ => (1152, 144 * bitrate / sample_rate + u32::from(padding)),
        };
        Some(Self {
            raw: h,
            version,
            layer,
            crc: (h >> 16) & 1 == 0,
            bitrate,
            bitrate_index: bri as u8,
            sample_rate,
            padding,
            mode,
            channels: if mode == 3 { 1 } else { 2 },
            frame_len: frame_len as usize,
            samples,
        })
    }

    /// Whether `other` belongs to the same stream (version, layer, rate and channel count).
    pub fn same_stream(&self, other: &Self) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate == other.sample_rate
            && self.channels == other.channels
    }

    /// Bytes of layer III side information after the header (and CRC).
    pub fn side_info_len(&self) -> usize {
        match (self.version == 10, self.channels) {
            (true, 1) => 17,
            (true, _) => 32,
            (false, 1) => 9,
            (false, _) => 17,
        }
    }

    /// `"MP1"`, `"MP2"` or `"MP3"`.
    pub fn codec_name(&self) -> &'static str {
        match self.layer {
            1 => "MP1",
            2 => "MP2",
            _ => "MP3",
        }
    }
}

/// `Some(true)` when `d` starts with a frame followed by a matching frame, `Some(false)` when it
/// starts with a frame but the data ends before the next one, `None` otherwise.
pub(crate) fn sniff(d: &[u8]) -> Option<bool> {
    let h = MpegAudioHeader::parse(d)?;
    let mut at = h.frame_len;
    for i in 0..2 {
        if at + 4 > d.len() {
            return if i == 0 { Some(false) } else { Some(true) };
        }
        let n = MpegAudioHeader::parse(&d[at..])?;
        if !n.same_stream(&h) {
            return None;
        }
        at += n.frame_len;
    }
    Some(true)
}

/// The LAME / Xing tag variant of CRC-16 (CRC-16/ARC: reflected 0x8005, init 0).
fn crc16_arc(mut crc: u16, data: &[u8]) -> u16 {
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

#[derive(Clone, Debug)]
enum SeekTable {
    None,
    /// Xing TOC: 100 entries of byte position / bytes * 256, from the tag frame.
    Xing {
        start: u64,
        bytes: u64,
        toc: [u8; 100],
    },
    /// VBRI: cumulative byte offsets (from the tag frame) every `frames_per_entry` frames.
    Vbri {
        start: u64,
        offsets: Vec<u64>,
        frames_per_entry: u64,
    },
}

/// Information from a Xing/Info or VBRI tag frame.
#[derive(Clone, Debug)]
struct TagInfo {
    frames: Option<u64>,
    delay: Option<(u32, u32)>,
    table: SeekTable,
}

fn parse_tag_frame(frame: &[u8], h: &MpegAudioHeader, start: u64) -> Option<TagInfo> {
    if h.layer == 3 {
        let base = 4 + h.side_info_len();
        let cands: &[usize] = if h.crc { &[base + 2, base] } else { &[base] };
        for &at in cands {
            let id = frame.get(at..at + 4)?;
            if id != b"Xing" && id != b"Info" {
                continue;
            }
            let flags = be32(frame, at + 4);
            let mut p = at + 8;
            let mut frames = None;
            let mut bytes = None;
            let mut toc = None;
            if flags & 1 != 0 {
                frames = Some(u64::from(be32(frame, p)));
                p += 4;
            }
            if flags & 2 != 0 {
                bytes = Some(u64::from(be32(frame, p)));
                p += 4;
            }
            if flags & 4 != 0 {
                if let Some(t) = frame.get(p..p + 100) {
                    let mut a = [0u8; 100];
                    a.copy_from_slice(t);
                    toc = Some(a);
                }
                p += 100;
            }
            if flags & 8 != 0 {
                p += 4;
            }
            // LAME extension: 9-byte encoder string, then delay / padding at +21
            let mut delay = None;
            if let Some(lame) = frame.get(p..p + 36) {
                let tagged = lame[..4].iter().all(|c| c.is_ascii_alphanumeric());
                if tagged {
                    let v =
                        u32::from(lame[21]) << 16 | u32::from(lame[22]) << 8 | u32::from(lame[23]);
                    delay = Some((v >> 12, v & 0xFFF));
                }
            }
            let table = match (toc, bytes) {
                (Some(toc), Some(bytes)) if bytes > 0 => SeekTable::Xing { start, bytes, toc },
                _ => SeekTable::None,
            };
            return Some(TagInfo {
                frames,
                delay,
                table,
            });
        }
        None
    } else {
        None
    }
    .or_else(|| {
        // VBRI sits 32 bytes after the header in any layer III frame
        let at = 36;
        if frame.get(at..at + 4)? != b"VBRI" {
            return None;
        }
        let frames = u64::from(be32(frame, at + 14));
        let entries = usize::from(be16(frame, at + 18));
        let scale = u64::from(be16(frame, at + 20));
        let esize = usize::from(be16(frame, at + 22));
        let fpe = u64::from(be16(frame, at + 24));
        let mut offsets = vec![0u64];
        let mut p = at + 26;
        if (1..=4).contains(&esize) {
            for _ in 0..entries {
                let Some(e) = frame.get(p..p + esize) else {
                    break;
                };
                let v = e.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b));
                let last = *offsets.last().unwrap_or(&0);
                offsets.push(last + v * scale.max(1));
                p += esize;
            }
        }
        let table = if offsets.len() > 1 && fpe > 0 {
            SeekTable::Vbri {
                start,
                offsets,
                frames_per_entry: fpe,
            }
        } else {
            SeekTable::None
        };
        Some(TagInfo {
            frames: Some(frames),
            delay: None,
            table,
        })
    })
}

/// Decoder delay of the standard layer III decoder (synthesis filterbank), added to the LAME
/// encoder delay to get the samples to drop.
pub(crate) const MP3_DECODER_DELAY: u64 = 529;

/// Files up to this size are scanned completely at open (exact duration and index).
const FULL_SCAN_LIMIT: u64 = 8 << 20;
/// A seek scans headers forward this far before falling back to the table / CBR estimate.
const SCAN_SEEK_LIMIT: u64 = 16 << 20;
/// How far to look for the next sync after damage.
const RESYNC_LIMIT: u64 = 1 << 20;

pub(crate) struct MpaDemuxer {
    src: Source,
    tracks: Vec<Track>,
    hdr: MpegAudioHeader,
    first: u64,
    end: u64,
    spf: u64,
    /// File offsets of frames 0, 1, … (exact, contiguous from the first audio frame).
    index: Vec<u64>,
    /// Offset just after the last indexed frame.
    index_end: u64,
    index_complete: bool,
    table: SeekTable,
    total_frames: Option<u64>,
    pos: u64,
    /// Index of the next frame when known exactly.
    frame_no: Option<u64>,
    next_pts: i64,
}

impl MpaDemuxer {
    pub fn open(mut src: Source, start: u64) -> Result<Self> {
        let end = id3::audio_end(&mut src, start)?;
        // find the first frame that is followed by a matching frame
        let mut scan = start;
        let (first, hdr) = loop {
            if scan >= end || scan > start + (1 << 20) {
                return Err(Error::invalid("no MPEG audio frames found"));
            }
            src.seek(scan);
            let win = src.read_upto(64 * 1024)?;
            let lim = win.len().min((end - scan) as usize);
            let mut found = None;
            for i in 0..lim.saturating_sub(3) {
                if win[i] != 0xFF || win[i + 1] & 0xE0 != 0xE0 {
                    continue;
                }
                let Some(h) = MpegAudioHeader::parse(&win[i..]) else {
                    continue;
                };
                let next = scan + i as u64 + h.frame_len as u64;
                if next + 4 > end {
                    if next <= end {
                        found = Some((scan + i as u64, h));
                        break;
                    }
                    continue;
                }
                let mut nb = [0u8; 4];
                src.read_at(next, &mut nb)?;
                if MpegAudioHeader::parse(&nb).is_some_and(|n| n.same_stream(&h)) {
                    found = Some((scan + i as u64, h));
                    break;
                }
            }
            if let Some(f) = found {
                break f;
            }
            if win.len() < 4 {
                return Err(Error::invalid("no MPEG audio frames found"));
            }
            scan += win.len() as u64 - 3;
        };
        let spf = u64::from(hdr.samples);
        src.seek(first);
        let frame = src.read_upto(hdr.frame_len)?;
        let tag = parse_tag_frame(&frame, &hdr, first);
        let audio_first = if tag.is_some() {
            first + hdr.frame_len as u64
        } else {
            first
        };
        let mut d = Self {
            src,
            tracks: Vec::new(),
            hdr,
            first: audio_first,
            end,
            spf,
            index: Vec::new(),
            index_end: audio_first,
            index_complete: false,
            table: SeekTable::None,
            total_frames: None,
            pos: audio_first,
            frame_no: Some(0),
            next_pts: 0,
        };
        let mut priming = 0u64;
        let mut padding = 0u64;
        let mut trim_total = 0u64;
        if let Some(t) = &tag {
            d.total_frames = t.frames.filter(|&f| f > 0);
            d.table = t.table.clone();
            if let Some((delay, pad)) = t.delay {
                if hdr.layer == 3 && (delay > 0 || pad > 0) {
                    priming = u64::from(delay) + MP3_DECODER_DELAY;
                    padding = u64::from(pad).saturating_sub(MP3_DECODER_DELAY);
                    trim_total = u64::from(delay) + u64::from(pad);
                }
            }
        }
        if d.total_frames.is_none() && end - audio_first <= FULL_SCAN_LIMIT {
            d.scan_index(u64::MAX)?;
            d.total_frames = Some(d.index.len() as u64);
        }
        let duration = match d.total_frames {
            Some(f) => {
                let total = f * spf;
                Some(
                    total
                        .saturating_sub(trim_total)
                        .min(total.saturating_sub(priming)),
                )
            }
            None => {
                // estimate from the average size of the first frames
                d.scan_index(63)?;
                let frames = ((end - audio_first) as f64 / d.avg_frame_len()).round() as u64;
                Some(frames * spf)
            }
        };
        let mut t = Track::new(
            CodecId::MpegAudio,
            hdr.sample_rate,
            hdr.channels,
            hdr.codec_name(),
        );
        t.duration = duration;
        t.priming = priming;
        t.padding = padding;
        d.tracks.push(t);
        Ok(d)
    }

    fn read_header(&mut self, at: u64) -> Result<Option<MpegAudioHeader>> {
        if at + 4 > self.end {
            return Ok(None);
        }
        let mut b = [0u8; 4];
        self.src.read_at(at, &mut b)?;
        Ok(MpegAudioHeader::parse(&b).filter(|h| h.same_stream(&self.hdr)))
    }

    /// Next frame start at or after `from` whose header is followed by another matching header
    /// (or the end of the data).
    fn resync(&mut self, from: u64) -> Result<Option<u64>> {
        let mut at = from;
        let stop = self.end.min(from.saturating_add(RESYNC_LIMIT));
        while at < stop {
            self.src.seek(at);
            let win = self.src.read_upto(16 * 1024)?;
            if win.len() < 4 {
                return Ok(None);
            }
            for i in 0..win.len() - 3 {
                if win[i] != 0xFF || win[i + 1] & 0xE0 != 0xE0 {
                    continue;
                }
                let p = at + i as u64;
                if let Some(h) = self.read_header(p)? {
                    let next = p + h.frame_len as u64;
                    if next + 4 > self.end {
                        if next <= self.end {
                            return Ok(Some(p));
                        }
                    } else if self.read_header(next)?.is_some() {
                        return Ok(Some(p));
                    }
                }
            }
            at += win.len() as u64 - 3;
        }
        Ok(None)
    }

    /// Extend the exact frame index until it holds `upto + 1` frames or the data ends.
    fn scan_index(&mut self, upto: u64) -> Result<()> {
        while !self.index_complete && (self.index.len() as u64) <= upto {
            let at = self.index_end;
            match self.read_header(at)? {
                Some(h) if at + h.frame_len as u64 <= self.end => {
                    self.index.push(at);
                    self.index_end = at + h.frame_len as u64;
                }
                Some(_) => self.index_complete = true,
                None => match self.resync(at + 1)? {
                    Some(p) => self.index_end = p,
                    None => self.index_complete = true,
                },
            }
        }
        Ok(())
    }

    /// Average frame size over the indexed frames (the first header's size before any).
    fn avg_frame_len(&self) -> f64 {
        if self.index.is_empty() {
            self.hdr.frame_len.max(1) as f64
        } else {
            ((self.index_end - self.first) as f64 / self.index.len() as f64).max(1.0)
        }
    }

    fn estimate_pos(&self, frame: u64) -> u64 {
        let total = self.total_frames.unwrap_or(0);
        match &self.table {
            SeekTable::Xing { start, bytes, toc } if total > 0 => {
                let pct = (frame as f64 / total as f64 * 100.0).clamp(0.0, 99.999);
                let i = pct as usize;
                let a = f64::from(toc[i]);
                let b = if i < 99 { f64::from(toc[i + 1]) } else { 256.0 };
                let f = a + (b - a) * (pct - i as f64);
                start + (f / 256.0 * *bytes as f64) as u64
            }
            SeekTable::Vbri {
                start,
                offsets,
                frames_per_entry,
            } => {
                let i = ((frame / frames_per_entry) as usize).min(offsets.len() - 1);
                start + offsets[i]
            }
            _ => {
                let bytes = self.end - self.first;
                let frames = if total > 0 {
                    total
                } else {
                    ((bytes as f64 / self.avg_frame_len()) as u64).max(1)
                };
                self.first + (bytes as f64 * (frame as f64 / frames as f64)) as u64
            }
        }
    }
}

impl Demuxer for MpaDemuxer {
    fn format_name(&self) -> &str {
        "MP3"
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<DemuxPacket>> {
        loop {
            let at = self.pos;
            let Some(h) = self.read_header(at)? else {
                match self.resync(at + 1)? {
                    Some(p) if at < self.end => {
                        self.pos = p;
                        self.frame_no = None;
                        continue;
                    }
                    _ => return Ok(None),
                }
            };
            if at + h.frame_len as u64 > self.end {
                return Ok(None);
            }
            self.src.seek(at);
            let data = self.src.read_vec(h.frame_len as u64)?;
            if let Some(n) = self.frame_no {
                if n == self.index.len() as u64 && at == self.index_end && !self.index_complete {
                    self.index.push(at);
                    self.index_end = at + h.frame_len as u64;
                }
                self.frame_no = Some(n + 1);
            }
            self.pos = at + h.frame_len as u64;
            let pts = self.next_pts;
            self.next_pts += self.spf as i64;
            return Ok(Some(DemuxPacket {
                track: 0,
                data,
                pts,
                duration: self.spf as u32,
                keyframe: true,
            }));
        }
    }

    fn seek(&mut self, track: usize, pts: i64) -> Result<i64> {
        if track != 0 {
            return Err(Error::InvalidArgument(format!("no track {track}")));
        }
        let f = (pts.max(0) as u64) / self.spf;
        if f >= self.index.len() as u64 && !self.index_complete {
            let est = self.estimate_pos(f);
            // scan headers when the target is near the indexed region, else jump to the estimate
            if est.saturating_sub(self.index_end) <= SCAN_SEEK_LIMIT {
                self.scan_index(f)?;
            } else {
                let p = self.resync(est.max(self.first))?.unwrap_or(self.end);
                self.pos = p;
                self.frame_no = None;
                self.next_pts = (f * self.spf) as i64;
                return Ok(self.next_pts);
            }
        }
        let f = f.min(self.index.len() as u64);
        self.pos = if (f as usize) < self.index.len() {
            self.index[f as usize]
        } else {
            self.index_end
        };
        self.frame_no = Some(f);
        self.next_pts = (f * self.spf) as i64;
        Ok(self.next_pts)
    }
}

/// Gapless information for a Xing/Info + LAME tag, in samples, LAME convention: the encoder
/// delay at the start and the padding at the end of the encoded stream, *not* including the
/// 529-sample decoder delay (readers add it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MpaGapless {
    /// Samples of encoder delay at the start (LAME: usually 576).
    pub encoder_delay: u32,
    /// Samples of padding added at the end.
    pub padding: u32,
}

/// What [`MpaWriter::finish`] writes into the frame reserved at the start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MpaInfoTag {
    /// Leave the reserved frame as is (a silent frame of zeros after its header).
    None,
    /// Build an `Info` (CBR) or `Xing` (VBR) tag with frame count, byte count and TOC, plus a
    /// LAME extension with the given gapless information.
    Auto(Option<MpaGapless>),
    /// Write this frame (exactly [`MpaWriter::reserved_len`] bytes).
    Frame(Vec<u8>),
}

/// A raw MPEG audio writer: frames are written back to back. With `reserve_info_frame`, a layer
/// III stream gets a placeholder frame (same version / rate / mode as the first frame) that
/// [`finish`](MpaWriter::finish) fills with a Xing/Info tag, so readers know the exact length,
/// can seek through the TOC, and (with gapless information) trim the encoder delay and padding.
/// The LAME extension's encoder string is `"LAMEmkff"` (readers such as FFmpeg only honour the
/// delay fields after a `LAME` prefix).
pub struct MpaWriter<W: Write + Seek> {
    w: W,
    reserve: bool,
    info: Option<(u64, MpegAudioHeader)>,
    first: Option<MpegAudioHeader>,
    sizes: Vec<u16>,
    bytes: u64,
    vbr: bool,
    music_crc: u16,
}

impl<W: Write + Seek> MpaWriter<W> {
    /// Start a stream at the writer's position.
    pub fn new(w: W, reserve_info_frame: bool) -> Result<Self> {
        Ok(Self {
            w,
            reserve: reserve_info_frame,
            info: None,
            first: None,
            sizes: Vec::new(),
            bytes: 0,
            vbr: false,
            music_crc: 0,
        })
    }

    /// Length of the reserved tag frame (0 if none was reserved).
    pub fn reserved_len(&self) -> usize {
        self.info.map_or(0, |(_, h)| h.frame_len)
    }

    /// Frames written so far (not counting the tag frame).
    pub fn frames(&self) -> u64 {
        self.sizes.len() as u64
    }

    /// Append one complete frame (header included; its length must match the header).
    pub fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        let h = MpegAudioHeader::parse(frame)
            .ok_or_else(|| Error::InvalidArgument("not an MPEG audio frame".into()))?;
        if frame.len() != h.frame_len {
            return Err(Error::InvalidArgument(format!(
                "frame is {} bytes, header says {}",
                frame.len(),
                h.frame_len
            )));
        }
        match self.first {
            None => {
                self.first = Some(h);
                if self.reserve && h.layer == 3 {
                    let ih = info_header(&h);
                    let pos = self.w.stream_position()?;
                    let mut f = vec![0u8; ih.frame_len];
                    f[..4].copy_from_slice(&ih.raw.to_be_bytes());
                    self.w.write_all(&f)?;
                    self.info = Some((pos, ih));
                }
            }
            Some(f) => {
                if !f.same_stream(&h) {
                    return Err(Error::InvalidArgument(
                        "frame parameters changed mid-stream".into(),
                    ));
                }
                if f.bitrate != h.bitrate {
                    self.vbr = true;
                }
            }
        }
        self.w.write_all(frame)?;
        self.music_crc = crc16_arc(self.music_crc, frame);
        self.sizes.push(frame.len() as u16);
        self.bytes += frame.len() as u64;
        Ok(())
    }

    /// Fill the reserved frame (if any) and return the writer, positioned at the end.
    pub fn finish(mut self, tag: MpaInfoTag) -> Result<W> {
        let end = self.w.stream_position()?;
        if let Some((pos, ih)) = self.info {
            let frame = match tag {
                MpaInfoTag::None => None,
                MpaInfoTag::Frame(f) => {
                    if f.len() != ih.frame_len {
                        return Err(Error::InvalidArgument(format!(
                            "tag frame must be {} bytes",
                            ih.frame_len
                        )));
                    }
                    Some(f)
                }
                MpaInfoTag::Auto(g) => Some(self.build_info(&ih, g)),
            };
            if let Some(f) = frame {
                patch(&mut self.w, pos, &f)?;
            }
        }
        self.w.seek(SeekFrom::Start(end))?;
        self.w.flush()?;
        Ok(self.w)
    }

    fn build_info(&self, ih: &MpegAudioHeader, g: Option<MpaGapless>) -> Vec<u8> {
        let mut f = vec![0u8; ih.frame_len];
        f[..4].copy_from_slice(&ih.raw.to_be_bytes());
        let mut p = 4 + ih.side_info_len();
        f[p..p + 4].copy_from_slice(if self.vbr { b"Xing" } else { b"Info" });
        f[p + 4..p + 8].copy_from_slice(&0x0Fu32.to_be_bytes());
        let frames = self.sizes.len() as u64;
        let total_bytes = self.bytes + ih.frame_len as u64;
        f[p + 8..p + 12].copy_from_slice(&(frames.min(u64::from(u32::MAX)) as u32).to_be_bytes());
        f[p + 12..p + 16]
            .copy_from_slice(&(total_bytes.min(u64::from(u32::MAX)) as u32).to_be_bytes());
        // TOC: byte position (from the tag frame) of the frame at each percent of the stream
        let mut toc = [0u8; 100];
        if frames > 0 {
            let mut off = ih.frame_len as u64;
            let mut k = 0u64;
            for (i, t) in toc.iter_mut().enumerate() {
                let target = i as u64 * frames / 100;
                while k < target {
                    off += u64::from(self.sizes[k as usize]);
                    k += 1;
                }
                *t = (off * 256 / total_bytes).min(255) as u8;
            }
        }
        f[p + 16..p + 116].copy_from_slice(&toc);
        f[p + 116..p + 120].copy_from_slice(&0u32.to_be_bytes());
        p += 120;
        if let Some(g) = g {
            let l = &mut f[p..p + 36];
            l[..9].copy_from_slice(b"LAMEmkff\0");
            l[9] = if self.vbr { 0x00 } else { 0x01 };
            l[20] = (self.first.map_or(0, |h| h.bitrate / 1000).min(255)) as u8;
            let d = g.encoder_delay.min(4095);
            let pd = g.padding.min(4095);
            l[21] = (d >> 4) as u8;
            l[22] = ((d & 0xF) << 4 | pd >> 8) as u8;
            l[23] = pd as u8;
            l[28..32].copy_from_slice(&(total_bytes.min(u64::from(u32::MAX)) as u32).to_be_bytes());
            l[32..34].copy_from_slice(&self.music_crc.to_be_bytes());
            let crc = crc16_arc(0, &f[..p + 34]);
            f[p + 34..p + 36].copy_from_slice(&crc.to_be_bytes());
        }
        f
    }
}

/// The header of an Info/Xing frame for a stream whose first frame is `h`: no CRC, no padding,
/// the same bit rate when the frame is large enough for the tag (so CBR streams stay CBR),
/// otherwise the smallest bit rate that fits.
fn info_header(h: &MpegAudioHeader) -> MpegAudioHeader {
    let need = 4 + h.side_info_len() + 120 + 36;
    let base = (h.raw | 0x0001_0000) & !0x0000_0200;
    let with = |bri: u32| MpegAudioHeader::from_u32((base & !0xF000) | (bri << 12));
    if let Some(x) = with(u32::from(h.bitrate_index)).filter(|x| x.frame_len >= need) {
        return x;
    }
    for bri in 1..15 {
        if let Some(x) = with(bri).filter(|x| x.frame_len >= need) {
            return x;
        }
    }
    with(14).unwrap_or(*h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_sizes() {
        // MPEG-1 layer III 128 kb/s 44.1 kHz stereo, no padding
        let h = MpegAudioHeader::from_u32(0xFFFB_9064).unwrap();
        assert_eq!(
            (
                h.version,
                h.layer,
                h.bitrate,
                h.sample_rate,
                h.frame_len,
                h.samples
            ),
            (10, 3, 128_000, 44100, 417, 1152)
        );
        // MPEG-2 layer III 32 kb/s 22.05 kHz mono
        let h = MpegAudioHeader::from_u32(0xFFF3_40C4).unwrap();
        assert_eq!(
            (h.version, h.sample_rate, h.samples, h.channels),
            (20, 22050, 576, 1)
        );
        assert_eq!(h.frame_len, 72 * 32_000 / 22050);
        assert!(MpegAudioHeader::from_u32(0xFFFF_FFFF).is_none());
        assert!(MpegAudioHeader::from_u32(0xFFE0_0000).is_none());
    }

    #[test]
    fn crc_arc() {
        assert_eq!(crc16_arc(0, b"123456789"), 0xBB3D);
    }
}
