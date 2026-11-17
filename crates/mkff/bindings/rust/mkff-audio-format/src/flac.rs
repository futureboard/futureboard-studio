//! Native FLAC (RFC 9639) reading and writing, plus the frame header / STREAMINFO parsing that
//! FLAC-in-Ogg and FLAC-in-MP4 share.

use std::io::{Seek, SeekFrom, Write};

use mkff_audio_core::crc::{crc16, crc8};
use mkff_audio_core::{CodecId, Error, Result};

use crate::io::{be16, patch, Source};
use crate::{id3, DemuxPacket, Demuxer, Track};

/// The fields of a 34-byte STREAMINFO block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamInfo {
    pub min_block: u32,
    pub max_block: u32,
    pub min_frame: u32,
    pub max_frame: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bps: u32,
    /// 0 when unknown.
    pub total_samples: u64,
}

impl StreamInfo {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < 34 {
            return Err(Error::invalid("short STREAMINFO"));
        }
        let u24 = |i: usize| u32::from(b[i]) << 16 | u32::from(b[i + 1]) << 8 | u32::from(b[i + 2]);
        let x = u64::from_be_bytes([b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
        let s = Self {
            min_block: u32::from(be16(b, 0)),
            max_block: u32::from(be16(b, 2)),
            min_frame: u24(4),
            max_frame: u24(7),
            sample_rate: (x >> 44) as u32,
            channels: ((x >> 41) & 7) as u32 + 1,
            bps: ((x >> 36) & 31) as u32 + 1,
            total_samples: x & 0xF_FFFF_FFFF,
        };
        if s.sample_rate == 0 || s.bps < 4 {
            return Err(Error::invalid("bad STREAMINFO"));
        }
        Ok(s)
    }

    /// A FLAC track for this STREAMINFO (`config` = the 34 bytes).
    pub fn track(&self, raw: &[u8]) -> Track {
        let mut t = Track::new(CodecId::Flac, self.sample_rate, self.channels, "FLAC");
        t.bits_per_sample = Some(self.bps);
        t.config = raw[..34].to_vec();
        t.duration = (self.total_samples > 0).then_some(self.total_samples);
        t
    }
}

/// A parsed FLAC frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    pub variable: bool,
    pub block_size: u32,
    /// `None`: as in STREAMINFO.
    pub sample_rate: Option<u32>,
    pub channels: u32,
    /// `None`: as in STREAMINFO.
    pub bps: Option<u32>,
    /// Frame number (fixed block size) or first sample number (variable).
    pub number: u64,
    /// Header length including the CRC-8.
    pub len: usize,
}

impl FrameHeader {
    /// Parse and CRC-8-check a frame header at the start of `b`.
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 6 || b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
            return None;
        }
        let variable = b[1] & 1 == 1;
        let bs_code = b[2] >> 4;
        let sr_code = b[2] & 0xF;
        let ch_code = b[3] >> 4;
        let ss_code = (b[3] >> 1) & 7;
        if bs_code == 0 || sr_code == 15 || ch_code > 10 || ss_code == 3 || b[3] & 1 != 0 {
            return None;
        }
        // UTF-8-style coded number
        let lead = b[4];
        let n = lead.leading_ones() as usize;
        let (mut number, extra) = match n {
            0 => (u64::from(lead), 0),
            2..=7 => (u64::from(lead) & ((1u64 << (7 - n)) - 1), n - 1),
            _ => return None,
        };
        let mut p = 5;
        for _ in 0..extra {
            let c = *b.get(p)?;
            if c & 0xC0 != 0x80 {
                return None;
            }
            number = (number << 6) | u64::from(c & 0x3F);
            p += 1;
        }
        if !variable && number >= 1 << 31 {
            return None;
        }
        let block_size = match bs_code {
            1 => 192,
            2..=5 => 576 << (bs_code - 2),
            6 => {
                p += 1;
                u32::from(*b.get(p - 1)?) + 1
            }
            7 => {
                p += 2;
                u32::from(be16(b.get(p - 2..p)?, 0)) + 1
            }
            _ => 256 << (bs_code - 8),
        };
        let sample_rate = match sr_code {
            0 => None,
            1 => Some(88200),
            2 => Some(176400),
            3 => Some(192000),
            4 => Some(8000),
            5 => Some(16000),
            6 => Some(22050),
            7 => Some(24000),
            8 => Some(32000),
            9 => Some(44100),
            10 => Some(48000),
            11 => Some(96000),
            12 => {
                p += 1;
                Some(u32::from(*b.get(p - 1)?) * 1000)
            }
            13 => {
                p += 2;
                Some(u32::from(be16(b.get(p - 2..p)?, 0)))
            }
            _ => {
                p += 2;
                Some(u32::from(be16(b.get(p - 2..p)?, 0)) * 10)
            }
        };
        let crc = *b.get(p)?;
        if crc8(&b[..p]) != crc {
            return None;
        }
        let bps = match ss_code {
            0 => None,
            1 => Some(8),
            2 => Some(12),
            4 => Some(16),
            5 => Some(20),
            6 => Some(24),
            _ => Some(32),
        };
        let channels = if ch_code < 8 {
            u32::from(ch_code) + 1
        } else {
            2
        };
        Some(Self {
            variable,
            block_size,
            sample_rate,
            channels,
            bps,
            number,
            len: p + 1,
        })
    }

    /// Whether the header agrees with the stream parameters.
    pub fn matches(&self, si: &StreamInfo) -> bool {
        self.channels == si.channels
            && self.bps.is_none_or(|b| b == si.bps)
            && self.sample_rate.is_none_or(|r| r == si.sample_rate)
            && (si.max_block < 16 || self.block_size <= si.max_block)
    }

    /// First sample of this frame (`fixed_block` = the stream's fixed block size).
    pub fn first_sample(&self, fixed_block: u32) -> u64 {
        if self.variable {
            self.number
        } else {
            self.number * u64::from(fixed_block)
        }
    }
}

/// The fixed block size of a stream for frame-number to sample conversion.
pub(crate) fn fixed_block_size(si: &StreamInfo, first: Option<&FrameHeader>) -> u32 {
    if si.min_block == si.max_block && si.max_block >= 16 {
        si.max_block
    } else {
        first.map_or(si.max_block.max(1), |h| h.block_size)
    }
}

const MAX_SEARCH: usize = 16 << 20;

struct Frame {
    data: Vec<u8>,
    hdr: FrameHeader,
    crc_ok: bool,
}

pub(crate) struct FlacDemuxer {
    src: Source,
    tracks: Vec<Track>,
    info: StreamInfo,
    first_frame: u64,
    end: u64,
    fixed_block: u32,
    variable: Option<bool>,
    seektable: Vec<(u64, u64)>,
    pos: u64,
}

impl FlacDemuxer {
    pub fn open(mut src: Source, start: u64) -> Result<Self> {
        let mut magic = [0u8; 4];
        src.read_at(start, &mut magic)?;
        if &magic != b"fLaC" {
            return Err(Error::invalid("not a FLAC stream"));
        }
        let mut pos = start + 4;
        let mut info_raw: Option<Vec<u8>> = None;
        let mut seektable = Vec::new();
        loop {
            let mut h = [0u8; 4];
            if src.read_at(pos, &mut h)? < 4 {
                return Err(Error::EndOfData);
            }
            let last = h[0] & 0x80 != 0;
            let typ = h[0] & 0x7F;
            let len = u64::from(h[1]) << 16 | u64::from(h[2]) << 8 | u64::from(h[3]);
            if typ == 127 {
                return Err(Error::invalid("invalid FLAC metadata block"));
            }
            match typ {
                0 => {
                    let b = src.read_vec(len)?;
                    if b.len() < 34 {
                        return Err(Error::invalid("short STREAMINFO"));
                    }
                    info_raw = Some(b[..34].to_vec());
                }
                3 => {
                    let b = src.read_vec(len)?;
                    for p in b.as_chunks::<18>().0 {
                        let sample =
                            u64::from_be_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]);
                        let off = u64::from_be_bytes([
                            p[8], p[9], p[10], p[11], p[12], p[13], p[14], p[15],
                        ]);
                        if sample != u64::MAX {
                            seektable.push((sample, off));
                        }
                    }
                }
                _ => {}
            }
            pos += 4 + len;
            if last {
                break;
            }
            if pos >= src.len() {
                return Err(Error::EndOfData);
            }
        }
        let info_raw = info_raw.ok_or_else(|| Error::invalid("FLAC without STREAMINFO"))?;
        let info = StreamInfo::parse(&info_raw)?;
        let end = id3::audio_end(&mut src, pos)?;
        seektable.sort_unstable();
        let mut d = Self {
            src,
            tracks: vec![info.track(&info_raw)],
            info,
            first_frame: pos,
            end,
            fixed_block: info.max_block.max(1),
            variable: None,
            seektable,
            pos,
        };
        // learn the blocking strategy from the first frame
        let mut hb = [0u8; 16];
        let n = d.src.read_at(pos, &mut hb)?;
        if let Some(h) = FrameHeader::parse(&hb[..n]) {
            d.variable = Some(h.variable);
            d.fixed_block = fixed_block_size(&info, Some(&h));
        }
        Ok(d)
    }

    fn header_ok(&self, h: &FrameHeader) -> bool {
        h.matches(&self.info) && self.variable.is_none_or(|v| v == h.variable)
    }

    /// Read the frame starting at `at`: its end is the next valid header whose frame CRC-16
    /// checks out (or, failing that, the first plausible header; or the end of the data).
    fn read_frame(&mut self, at: u64) -> Result<Option<Frame>> {
        if at >= self.end {
            return Ok(None);
        }
        let avail = (self.end - at) as usize;
        let mut want = (self.info.max_frame as usize + 32)
            .clamp(4096, 1 << 20)
            .min(avail);
        self.src.seek(at);
        let mut win = self.src.read_upto(want)?;
        let Some(hdr) = FrameHeader::parse(&win).filter(|h| self.header_ok(h)) else {
            return Ok(None);
        };
        let next_number = if hdr.variable {
            hdr.number + u64::from(hdr.block_size)
        } else {
            hdr.number + 1
        };
        let mut i = hdr.len + 1;
        let mut crc = crc16(0, &win[..i.min(win.len())]);
        let mut crc_pos = i.min(win.len());
        let mut fallback: Option<usize> = None;
        loop {
            let eof = win.len() >= avail;
            let scan_end = if eof {
                win.len()
            } else {
                win.len().saturating_sub(24)
            };
            while i + 1 < scan_end {
                if win[i] == 0xFF && win[i + 1] & 0xFE == 0xF8 {
                    if let Some(n) = FrameHeader::parse(&win[i..]).filter(|n| self.header_ok(n)) {
                        crc = crc16(crc as u16, &win[crc_pos..i - 2]) as u16 as u16;
                        crc_pos = i - 2;
                        let ok = crc == be16(&win, i - 2);
                        if ok && n.number == next_number {
                            win.truncate(i);
                            return Ok(Some(Frame {
                                data: win,
                                hdr,
                                crc_ok: true,
                            }));
                        }
                        if fallback.is_none() && n.number == next_number {
                            fallback = Some(i);
                        }
                    }
                }
                i += 1;
            }
            if eof {
                let ok =
                    win.len() >= 2 && crc16(0, &win[..win.len() - 2]) == be16(&win, win.len() - 2);
                if !ok {
                    if let Some(f) = fallback {
                        win.truncate(f);
                        return Ok(Some(Frame {
                            data: win,
                            hdr,
                            crc_ok: false,
                        }));
                    }
                }
                return Ok(Some(Frame {
                    data: win,
                    hdr,
                    crc_ok: ok,
                }));
            }
            if win.len() >= MAX_SEARCH {
                if let Some(f) = fallback {
                    win.truncate(f);
                    return Ok(Some(Frame {
                        data: win,
                        hdr,
                        crc_ok: false,
                    }));
                }
                return Err(Error::invalid("FLAC frame end not found"));
            }
            want = (win.len() * 2).min(avail).min(MAX_SEARCH);
            let more = self.src.read_upto(want - win.len())?;
            if more.is_empty() {
                return Ok(Some(Frame {
                    data: win,
                    hdr,
                    crc_ok: false,
                }));
            }
            win.extend_from_slice(&more);
        }
    }

    /// The next position at or after `from` holding a plausible frame header.
    fn find_header(&mut self, from: u64, limit: u64) -> Result<Option<(u64, FrameHeader)>> {
        let mut at = from;
        while at < limit.min(self.end) {
            self.src.seek(at);
            let win = self.src.read_upto(64 * 1024)?;
            if win.len() < 16 && at + (win.len() as u64) < self.end {
                return Ok(None);
            }
            let n = win.len();
            for i in 0..n.saturating_sub(1) {
                if win[i] == 0xFF && win[i + 1] & 0xFE == 0xF8 {
                    if let Some(h) = FrameHeader::parse(&win[i..]).filter(|h| self.header_ok(h)) {
                        return Ok(Some((at + i as u64, h)));
                    }
                }
            }
            if n < 16 {
                return Ok(None);
            }
            at += n as u64 - 15;
        }
        Ok(None)
    }

    /// A verified frame at or after `from` (its CRC-16 checked).
    fn find_frame(&mut self, from: u64, limit: u64) -> Result<Option<(u64, FrameHeader)>> {
        let mut at = from;
        while let Some((p, h)) = self.find_header(at, limit)? {
            if let Some(f) = self.read_frame(p)? {
                if f.crc_ok {
                    return Ok(Some((p, h)));
                }
            }
            at = p + 1;
        }
        Ok(None)
    }
}

impl Demuxer for FlacDemuxer {
    fn format_name(&self) -> &str {
        "FLAC"
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<DemuxPacket>> {
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            match self.read_frame(self.pos)? {
                Some(f) => {
                    self.pos += f.data.len() as u64;
                    if self.variable.is_none() {
                        self.variable = Some(f.hdr.variable);
                        self.fixed_block = fixed_block_size(&self.info, Some(&f.hdr));
                    }
                    let pts = f.hdr.first_sample(self.fixed_block) as i64;
                    return Ok(Some(DemuxPacket {
                        track: 0,
                        data: f.data,
                        pts,
                        duration: f.hdr.block_size,
                        keyframe: true,
                    }));
                }
                None => match self.find_frame(self.pos + 1, u64::MAX)? {
                    Some((p, _)) => self.pos = p,
                    None => {
                        self.pos = self.end;
                        return Ok(None);
                    }
                },
            }
        }
    }

    fn seek(&mut self, track: usize, pts: i64) -> Result<i64> {
        if track != 0 {
            return Err(Error::InvalidArgument(format!("no track {track}")));
        }
        let target = pts.max(0) as u64;
        // a lower bound from the seek table, refined by bisection on frame sample numbers
        let mut lo = (self.first_frame, 0u64);
        if let Some(&(s, off)) = self.seektable.iter().rev().find(|(s, _)| *s <= target) {
            let p = self.first_frame.saturating_add(off);
            if p < self.end {
                lo = (p, s);
            }
        }
        let mut hi = self.end;
        if let Some(&(_, off)) = self.seektable.iter().find(|(s, _)| *s > target) {
            hi = hi.min(self.first_frame.saturating_add(off));
        }
        while hi > lo.0 + (256 << 10) {
            let mid = lo.0 + (hi - lo.0) / 2;
            match self.find_frame(mid, hi)? {
                Some((p, h)) => {
                    let s = h.first_sample(self.fixed_block);
                    if s <= target {
                        lo = (p, s);
                    } else {
                        hi = mid;
                    }
                }
                None => hi = mid,
            }
        }
        // walk frames to the one containing the target
        let mut at = lo.0;
        let mut landed = (lo.0, lo.1);
        while let Some(f) = self.read_frame(at)? {
            let s = f.hdr.first_sample(self.fixed_block);
            if s > target {
                break;
            }
            landed = (at, s);
            if s + u64::from(f.hdr.block_size) > target {
                break;
            }
            at += f.data.len() as u64;
        }
        self.pos = landed.0;
        Ok(landed.1 as i64)
    }
}

/// Options for [`FlacWriter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacWriterOptions {
    /// Seek points to reserve (a SEEKTABLE block filled at `finish`; 0 for none).
    pub seek_points: u32,
    /// Bytes of PADDING block to leave for later tag edits (0 for none).
    pub padding: u32,
}

impl Default for FlacWriterOptions {
    fn default() -> Self {
        Self {
            seek_points: 100,
            padding: 0,
        }
    }
}

/// A native FLAC writer: `fLaC`, STREAMINFO (rewritten at [`finish`](FlacWriter::finish)), an
/// optional SEEKTABLE (filled at `finish` from the frames written) and PADDING, then the
/// encoded frames.
pub struct FlacWriter<W: Write + Seek> {
    w: W,
    info_pos: u64,
    info: [u8; 34],
    si: StreamInfo,
    seek_pos: Option<u64>,
    seek_points: u32,
    /// (first sample, offset from the first frame) per frame.
    frames: Vec<(u64, u64)>,
    fixed_block: Option<u32>,
    min_frame: u32,
    max_frame: u32,
    samples: u64,
    bytes: u64,
}

impl<W: Write + Seek> FlacWriter<W> {
    /// Write the stream header. `streaminfo` is the 34-byte STREAMINFO body from the encoder.
    pub fn new(mut w: W, streaminfo: &[u8], opts: FlacWriterOptions) -> Result<Self> {
        let si = StreamInfo::parse(streaminfo)?;
        let mut info = [0u8; 34];
        info.copy_from_slice(&streaminfo[..34]);
        let start = w.stream_position()?;
        let mut h = Vec::new();
        h.extend_from_slice(b"fLaC");
        let more = opts.seek_points > 0 || opts.padding > 0;
        h.push(if more { 0 } else { 0x80 });
        h.extend_from_slice(&[0, 0, 34]);
        h.extend_from_slice(&info);
        let mut seek_pos = None;
        if opts.seek_points > 0 {
            let len = opts.seek_points.min(0xFFFFFF / 18) * 18;
            h.push(if opts.padding > 0 { 3 } else { 0x83 });
            h.extend_from_slice(&len.to_be_bytes()[1..]);
            seek_pos = Some(start + h.len() as u64);
            for _ in 0..len / 18 {
                h.extend_from_slice(&u64::MAX.to_be_bytes());
                h.extend_from_slice(&[0; 10]);
            }
        }
        if opts.padding > 0 {
            let len = opts.padding.min(0xFFFFFF);
            h.push(0x81);
            h.extend_from_slice(&len.to_be_bytes()[1..]);
            h.resize(h.len() + len as usize, 0);
        }
        w.write_all(&h)?;
        Ok(Self {
            w,
            info_pos: start + 8,
            info,
            si,
            seek_pos,
            seek_points: opts.seek_points.min(0xFFFFFF / 18),
            frames: Vec::new(),
            fixed_block: None,
            min_frame: u32::MAX,
            max_frame: 0,
            samples: 0,
            bytes: 0,
        })
    }

    /// Append one encoded frame.
    pub fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        let h = FrameHeader::parse(frame)
            .ok_or_else(|| Error::InvalidArgument("not a FLAC frame".into()))?;
        let fb = *self
            .fixed_block
            .get_or_insert(fixed_block_size(&self.si, Some(&h)));
        let first = h.first_sample(fb);
        self.frames.push((first, self.bytes));
        self.w.write_all(frame)?;
        self.bytes += frame.len() as u64;
        self.samples = self.samples.max(first + u64::from(h.block_size));
        let n = frame.len().min(0xFFFFFF) as u32;
        self.min_frame = self.min_frame.min(n);
        self.max_frame = self.max_frame.max(n);
        Ok(())
    }

    /// Rewrite STREAMINFO (with `streaminfo` when given, else the original with the total
    /// sample count and frame sizes observed) and the seek table; return the writer at the end.
    pub fn finish(mut self, streaminfo: Option<&[u8]>) -> Result<W> {
        let end = self.w.stream_position()?;
        let info = match streaminfo {
            Some(s) => {
                StreamInfo::parse(s)?;
                s[..34].to_vec()
            }
            None => {
                let mut i = self.info.to_vec();
                if self.max_frame > 0 {
                    i[4..7].copy_from_slice(&self.min_frame.to_be_bytes()[1..]);
                    i[7..10].copy_from_slice(&self.max_frame.to_be_bytes()[1..]);
                }
                let total = self.samples.min(0xF_FFFF_FFFF);
                i[13] = (i[13] & 0xF0) | (total >> 32) as u8;
                i[14..18].copy_from_slice(&(total as u32).to_be_bytes());
                i
            }
        };
        patch(&mut self.w, self.info_pos, &info)?;
        if let Some(pos) = self.seek_pos {
            let n = u64::from(self.seek_points);
            let mut points: Vec<(u64, u64, u16)> = Vec::new();
            if !self.frames.is_empty() && self.samples > 0 {
                let mut fi = 0usize;
                for k in 0..n {
                    let target = k * self.samples / n;
                    while fi + 1 < self.frames.len() && self.frames[fi + 1].0 <= target {
                        fi += 1;
                    }
                    let (s, off) = self.frames[fi];
                    if points.last().is_none_or(|p| p.0 != s) {
                        let next = self.frames.get(fi + 1).map_or(self.samples, |f| f.0);
                        points.push((s, off, (next - s).min(0xFFFF) as u16));
                    }
                }
            }
            let mut table = Vec::with_capacity(n as usize * 18);
            for k in 0..n as usize {
                match points.get(k) {
                    Some(&(s, off, len)) => {
                        table.extend_from_slice(&s.to_be_bytes());
                        table.extend_from_slice(&off.to_be_bytes());
                        table.extend_from_slice(&len.to_be_bytes());
                    }
                    None => {
                        table.extend_from_slice(&u64::MAX.to_be_bytes());
                        table.extend_from_slice(&[0; 10]);
                    }
                }
            }
            patch(&mut self.w, pos, &table)?;
        }
        self.w.seek(SeekFrom::Start(end))?;
        self.w.flush()?;
        Ok(self.w)
    }
}
