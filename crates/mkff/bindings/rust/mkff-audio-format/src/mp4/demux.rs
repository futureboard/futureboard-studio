//! MP4 / MOV audio track reading.

use mkff_audio_core::{CodecId, Error, Result};

use super::{child, parse_asc, parse_dac3, parse_dec3, parse_esds, path, Boxes};
use crate::adts::AAC_RATES;
use crate::flac::StreamInfo;
use crate::io::{be16, be32, Bytes, Source};
use crate::pcm::PcmFormat;
use crate::wav::PCM_PACKET_FRAMES;
use crate::xiph::{opus_head_from_dops, OpusHead};
use crate::{DemuxPacket, Demuxer, Track};

const MAX_MOOV: u64 = 256 << 20;
const MAX_MOOF: u64 = 64 << 20;
const MAX_SAMPLE: u32 = 64 << 20;
const MAX_VAR_SAMPLES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug)]
struct Chunk {
    offset: u64,
    first: u64,
    count: u64,
}

/// `count * delta` as a saturating time span.
fn span(count: u64, delta: u32) -> i64 {
    i64::try_from(count)
        .unwrap_or(i64::MAX)
        .saturating_mul(i64::from(delta))
}

#[derive(Clone, Copy, Debug)]
struct TimeRun {
    first: u64,
    count: u64,
    delta: u32,
    dts: i64,
}

/// A track's sample table: chunks (contiguous sample groups), sizes and decode times.
#[derive(Default, Debug)]
struct Tables {
    chunks: Vec<Chunk>,
    chunk_samples: u64,
    const_size: Option<u32>,
    var: bool,
    sizes: Vec<u32>,
    size_count: u64,
    times: Vec<TimeRun>,
    time_count: u64,
    next_dts: i64,
    sync: Option<Vec<u64>>,
    /// Most samples with individually stored sizes (bounded by the file size).
    var_limit: u64,
}

impl Tables {
    fn push_chunk(&mut self, offset: u64, count: u64) {
        if count == 0 {
            return;
        }
        self.chunks.push(Chunk {
            offset,
            first: self.chunk_samples,
            count,
        });
        self.chunk_samples += count;
    }

    fn push_sizes(&mut self, size: u32, n: u64) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        if !self.var {
            match self.const_size {
                None => self.const_size = Some(size),
                Some(c) if c == size => {}
                Some(c) => {
                    if self.size_count > self.var_limit {
                        return Err(Error::unsupported("too many samples with varying sizes"));
                    }
                    self.sizes = vec![c; self.size_count as usize];
                    self.var = true;
                }
            }
        }
        if self.var {
            if self.size_count + n > self.var_limit {
                return Err(Error::unsupported("too many samples with varying sizes"));
            }
            self.sizes.extend(std::iter::repeat_n(size, n as usize));
        }
        self.size_count += n;
        Ok(())
    }

    fn push_times(&mut self, delta: u32, n: u64) {
        if n == 0 {
            return;
        }
        if let Some(last) = self.times.last_mut() {
            let end = last.dts.saturating_add(span(last.count, last.delta));
            if last.delta == delta && end == self.next_dts {
                last.count += n;
                self.time_count += n;
                self.next_dts = self.next_dts.saturating_add(span(n, delta));
                return;
            }
        }
        self.times.push(TimeRun {
            first: self.time_count,
            count: n,
            delta,
            dts: self.next_dts,
        });
        self.time_count += n;
        self.next_dts = self.next_dts.saturating_add(span(n, delta));
    }

    /// Samples present in all three tables.
    fn count(&self) -> u64 {
        self.chunk_samples.min(self.size_count).min(self.time_count)
    }

    fn size(&self, i: u64) -> u32 {
        if self.var {
            self.sizes.get(i as usize).copied().unwrap_or(0)
        } else {
            self.const_size.unwrap_or(0)
        }
    }

    fn run(&self, i: u64) -> Option<&TimeRun> {
        let k = self.times.partition_point(|r| r.first <= i);
        self.times.get(k.checked_sub(1)?)
    }

    fn dts(&self, i: u64) -> i64 {
        match self.run(i) {
            Some(r) => r.dts.saturating_add(span(i - r.first, r.delta)),
            None => self.next_dts,
        }
    }

    fn end_dts(&self) -> i64 {
        let n = self.count();
        if n == 0 {
            return 0;
        }
        self.dts(n - 1)
            .saturating_add(i64::from(self.run(n - 1).map_or(0, |r| r.delta)))
    }

    /// The last sample whose decode time is at or before `t` (0 if `t` precedes everything,
    /// `count()` if it is past the end).
    fn sample_at(&self, t: i64) -> u64 {
        let n = self.count();
        if n == 0 {
            return 0;
        }
        if t >= self.end_dts() {
            return n;
        }
        let k = self.times.partition_point(|r| r.dts <= t);
        let Some(r) = k.checked_sub(1).and_then(|k| self.times.get(k)) else {
            return 0;
        };
        let within = if r.delta == 0 {
            0
        } else {
            (t.saturating_sub(r.dts) / i64::from(r.delta)) as u64
        };
        (r.first + within.min(r.count - 1)).min(n - 1)
    }

    fn chunk_of(&self, i: u64) -> Option<usize> {
        let k = self.chunks.partition_point(|c| c.first <= i);
        let k = k.checked_sub(1)?;
        let c = &self.chunks[k];
        (i < c.first + c.count).then_some(k)
    }

    /// Byte offset of sample `i` inside its chunk.
    fn offset_in_chunk(&self, chunk: usize, i: u64) -> u64 {
        let c = &self.chunks[chunk];
        if self.var {
            let a = (c.first as usize).min(self.sizes.len());
            let b = (i as usize).min(self.sizes.len());
            self.sizes[a..b].iter().map(|&s| u64::from(s)).sum()
        } else {
            (i - c.first) * u64::from(self.const_size.unwrap_or(0))
        }
    }

    fn is_sync(&self, i: u64) -> bool {
        match &self.sync {
            None => true,
            Some(s) => s.binary_search(&i).is_ok(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Trex {
    duration: u32,
    size: u32,
}

struct Mp4Track {
    id: u32,
    timescale: u32,
    tables: Tables,
    trex: Trex,
    /// Bytes per PCM frame (0 for compressed codecs).
    pcm_bpf: u64,
    /// First non-empty edit: (media_time, segment_duration in the movie timescale).
    edit: Option<(u64, u64)>,
    /// Opus pre-skip.
    pre_skip: u64,
    next: u64,
    chunk: usize,
    chunk_off: u64,
}

impl Mp4Track {
    /// Point the cursor at sample `i`.
    fn set_next(&mut self, i: u64) {
        self.next = i;
        match self.tables.chunk_of(i) {
            Some(c) => {
                self.chunk = c;
                self.chunk_off = self.tables.offset_in_chunk(c, i);
            }
            None => {
                self.next = self.tables.count();
                self.chunk = self.tables.chunks.len();
                self.chunk_off = 0;
            }
        }
    }

    fn file_pos(&self) -> Option<u64> {
        if self.next >= self.tables.count() {
            return None;
        }
        Some(
            self.tables
                .chunks
                .get(self.chunk)?
                .offset
                .saturating_add(self.chunk_off),
        )
    }

    /// Samples per PCM packet from `next`, staying inside the chunk.
    fn pcm_group(&self) -> u64 {
        let c = &self.tables.chunks[self.chunk];
        let left = (c.first + c.count - self.next).min(self.tables.count() - self.next);
        let size = u64::from(self.tables.size(self.next));
        let frames_per_sample = if size >= self.pcm_bpf && self.pcm_bpf > 0 {
            size / self.pcm_bpf
        } else {
            1
        };
        (PCM_PACKET_FRAMES / frames_per_sample.max(1)).clamp(1, left.max(1))
    }

    fn sample_bytes(&self, i: u64) -> u64 {
        let s = u64::from(self.tables.size(i));
        if self.pcm_bpf > 0 && s < self.pcm_bpf {
            // old QuickTime PCM: a "sample" of size 1 is one frame
            self.pcm_bpf
        } else {
            s
        }
    }

    fn advance(&mut self, n: u64, bytes: u64) {
        self.next += n;
        self.chunk_off += bytes;
        while let Some(c) = self.tables.chunks.get(self.chunk) {
            if self.next < c.first + c.count {
                break;
            }
            self.chunk += 1;
            self.chunk_off = 0;
        }
    }
}

/// What a sample entry describes.
struct Entry {
    track: Track,
    pcm_bpf: u64,
    /// Opus pre-skip (priming when there is no edit list).
    pre_skip: u64,
}

fn looks_like_box(d: &[u8], at: usize) -> bool {
    let Some(h) = d.get(at..at + 8) else {
        return false;
    };
    let size = be32(h, 0) as usize;
    size >= 8
        && at + size <= d.len()
        && h[4..8]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b' ')
}

/// Find a child box of a sample entry, also looking inside a QuickTime `wave` atom.
fn entry_child<'a>(children: &'a [u8], t: &[u8; 4]) -> Option<&'a [u8]> {
    child(children, t).or_else(|| child(child(children, b"wave")?, t))
}

fn parse_entry(typ: &[u8; 4], body: &[u8], qt: bool, mdhd_rate: u32) -> Result<Option<Entry>> {
    if body.len() < 28 {
        return Err(Error::invalid("short audio sample entry"));
    }
    let version = be16(body, 8);
    let mut channels = u32::from(be16(body, 16));
    let sample_size = u32::from(be16(body, 18));
    let mut rate = be32(body, 24) >> 16;
    let mut v1_bytes_per_sample = 0;
    let mut lpcm: Option<(u32, u32)> = None; // bits, flags
    let children_at = match version {
        1 if !qt && looks_like_box(body, 28) => 28,
        1 => {
            v1_bytes_per_sample = be32(body, 40);
            44
        }
        2 => {
            if body.len() < 64 {
                return Err(Error::invalid("short v2 sound description"));
            }
            let r = f64::from_bits(u64::from(be32(body, 32)) << 32 | u64::from(be32(body, 36)));
            if (1.0..1e7).contains(&r) {
                rate = r.round() as u32;
            }
            channels = be32(body, 40);
            lpcm = Some((be32(body, 48), be32(body, 52)));
            64
        }
        _ => 28,
    };
    let children = body.get(children_at..).unwrap_or(&[]);
    if rate == 0 {
        rate = mdhd_rate;
    }
    if let Some(srat) = child(children, b"srat") {
        if srat.len() >= 8 {
            rate = be32(srat, 4);
        }
    }
    let little = entry_child(children, b"enda").is_some_and(|e| be16(e, 0) == 1);
    let pcm_bits = if v1_bytes_per_sample > 0 {
        v1_bytes_per_sample.saturating_mul(8)
    } else {
        sample_size
    };
    let mut pre_skip = 0;
    let pcm: Option<PcmFormat> = match typ {
        b"sowt" => Some(PcmFormat::int(pcm_bits.max(8), false)),
        b"twos" | b"NONE" => Some(PcmFormat::int(pcm_bits.max(8), pcm_bits > 8)),
        b"raw " => Some(PcmFormat::u8()),
        b"in24" => Some(PcmFormat::int(24, !little)),
        b"in32" => Some(PcmFormat::int(32, !little)),
        b"fl32" => Some(PcmFormat::float(32, !little)),
        b"fl64" => Some(PcmFormat::float(64, !little)),
        b"ulaw" => Some(PcmFormat::mulaw()),
        b"alaw" => Some(PcmFormat::alaw()),
        b"lpcm" => {
            let (bits, flags) = lpcm.unwrap_or((sample_size, 0x0C));
            let float = flags & 1 != 0;
            Some(PcmFormat {
                bits: bits.min(64).div_ceil(8) * 8,
                float,
                big_endian: flags & 2 != 0,
                signed: float || flags & 4 != 0 || bits > 8,
                coding: crate::PcmCoding::Linear,
            })
        }
        b"ipcm" | b"fpcm" => {
            let c = child(children, b"pcmC").ok_or_else(|| Error::invalid("ipcm without pcmC"))?;
            let flags = *c.get(4).unwrap_or(&0);
            let bits = u32::from(*c.get(5).unwrap_or(&16));
            let le = flags & 1 != 0;
            Some(if typ == b"fpcm" {
                PcmFormat::float(bits, !le)
            } else {
                PcmFormat::int(bits, !le)
            })
        }
        _ => None,
    };
    if let Some(fmt) = pcm {
        if !fmt.is_valid() || channels == 0 {
            return Err(Error::unsupported(format!("PCM {} bits", fmt.bits)));
        }
        let mut t = Track::new(CodecId::Pcm, rate, channels, format!("PCM {}", fmt.label()));
        t.pcm = Some(fmt);
        t.bits_per_sample = Some(fmt.bits);
        let bpf = fmt.bytes_per_sample() as u64 * u64::from(channels);
        return Ok(Some(Entry {
            track: t,
            pcm_bpf: bpf,
            pre_skip: 0,
        }));
    }
    let track = match typ {
        b"mp4a" => {
            let Some(esds) = entry_child(children, b"esds") else {
                return Ok(None);
            };
            let e = parse_esds(esds)?;
            match e.oti {
                0x40 | 0x66..=0x68 => {
                    let asc = if e.dsi.is_empty() {
                        let aot = if e.oti == 0x40 {
                            2
                        } else {
                            u16::from(e.oti - 0x66 + 1)
                        };
                        let sfi = AAC_RATES.iter().position(|&r| r == rate).unwrap_or(4) as u16;
                        ((aot << 11) | (sfi << 7) | ((channels.min(7) as u16) << 3))
                            .to_be_bytes()
                            .to_vec()
                    } else {
                        e.dsi.clone()
                    };
                    let info = parse_asc(&asc)?;
                    let ch = if info.channels > 0 {
                        info.channels
                    } else {
                        channels
                    };
                    let mut t = Track::new(CodecId::Aac, info.rate, ch, info.name());
                    t.config = asc;
                    t
                }
                0x69 | 0x6B => Track::new(CodecId::MpegAudio, rate, channels, "MP3"),
                0xA5 => Track::new(CodecId::Ac3, rate, channels, "AC-3"),
                0xA6 => Track::new(CodecId::Ac3, rate, channels, "E-AC-3"),
                _ => return Ok(None),
            }
        }
        b".mp3" | b"mp3 " | b"ms\0\x55" => Track::new(CodecId::MpegAudio, rate, channels, "MP3"),
        b"alac" => {
            let a = entry_child(children, b"alac")
                .ok_or_else(|| Error::invalid("ALAC without cookie"))?;
            let cfg = if a.len() >= 28 {
                &a[4..28]
            } else if a.len() == 24 {
                a
            } else {
                return Err(Error::invalid("short ALAC cookie"));
            };
            let mut t = Track::new(CodecId::Alac, be32(cfg, 20), u32::from(cfg[9]), "ALAC");
            if t.sample_rate == 0 {
                t.sample_rate = rate;
            }
            t.bits_per_sample = Some(u32::from(cfg[5]));
            t.config = cfg.to_vec();
            t
        }
        b"fLaC" => {
            let d = entry_child(children, b"dfLa")
                .ok_or_else(|| Error::invalid("fLaC without dfLa"))?;
            let blocks = d.get(4..).unwrap_or(&[]);
            if blocks.len() < 38 || blocks[0] & 0x7F != 0 {
                return Err(Error::invalid("dfLa without STREAMINFO"));
            }
            let si = StreamInfo::parse(&blocks[4..38])?;
            let mut t = si.track(&blocks[4..38]);
            t.duration = None;
            t
        }
        b"Opus" => {
            let d = entry_child(children, b"dOps")
                .ok_or_else(|| Error::invalid("Opus without dOps"))?;
            let head = opus_head_from_dops(d)?;
            let h = OpusHead::parse(&head)?;
            pre_skip = u64::from(h.pre_skip);
            let mut t = Track::new(CodecId::Opus, 48000, h.channels, "Opus");
            t.config = head;
            t
        }
        b"ac-3" => {
            let d = entry_child(children, b"dac3")
                .ok_or_else(|| Error::invalid("ac-3 without dac3"))?;
            let (r, ch) = parse_dac3(d)?;
            let mut t = Track::new(CodecId::Ac3, if r > 0 { r } else { rate }, ch, "AC-3");
            t.config = d.to_vec();
            t
        }
        b"ec-3" => {
            let d = entry_child(children, b"dec3")
                .ok_or_else(|| Error::invalid("ec-3 without dec3"))?;
            let (r, ch) = parse_dec3(d)?;
            let mut t = Track::new(CodecId::Ac3, if r > 0 { r } else { rate }, ch, "E-AC-3");
            t.config = d.to_vec();
            t
        }
        _ => return Ok(None),
    };
    Ok(Some(Entry {
        track,
        pcm_bpf: 0,
        pre_skip,
    }))
}

/// `(priming, padding, total)` from an iTunSMPB string.
fn parse_itunsmpb(s: &str) -> Option<(u64, u64, u64)> {
    let f: Vec<u64> = s
        .split_whitespace()
        .take(4)
        .map(|x| u64::from_str_radix(x, 16).ok())
        .collect::<Option<_>>()?;
    (f.len() == 4).then(|| (f[1], f[2], f[3]))
}

fn find_itunsmpb(moov: &[u8]) -> Option<(u64, u64, u64)> {
    let meta = path(moov, &[b"udta", b"meta"])?;
    // ISO `meta` is a full box; QuickTime-style `meta` has no version / flags
    let meta = if meta.get(4..8) == Some(b"hdlr") {
        meta
    } else {
        meta.get(4..)?
    };
    let ilst = child(meta, b"ilst")?;
    for (t, item) in Boxes::new(ilst) {
        if &t != b"----" {
            continue;
        }
        let name = child(item, b"name")?;
        if name.get(4..) != Some(b"iTunSMPB") {
            continue;
        }
        let data = child(item, b"data")?;
        return parse_itunsmpb(&String::from_utf8_lossy(data.get(8..)?));
    }
    None
}

/// MP4 demuxer: audio tracks (`soun` handler with a supported sample entry) in `moov` order.
/// Packets of all tracks come interleaved by file offset (the order a sequential read of the
/// file meets them). Fragmented files (`moof`) are indexed at open.
pub(crate) struct Mp4Demuxer {
    src: Source,
    tracks: Vec<Track>,
    state: Vec<Mp4Track>,
}

impl Mp4Demuxer {
    pub fn open(mut src: Source) -> Result<Self> {
        let len = src.len();
        let mut pos = 0u64;
        let mut moov: Option<Vec<u8>> = None;
        let mut moofs: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut qt = false;
        let mut first = true;
        while pos + 8 <= len {
            let mut h = [0u8; 16];
            let n = src.read_at(pos, &mut h)?;
            let size32 = be32(&h, 0);
            let typ = [h[4], h[5], h[6], h[7]];
            let (hlen, size) = match size32 {
                0 => (8u64, len - pos),
                1 if n >= 16 => (16, u64::from(be32(&h, 8)) << 32 | u64::from(be32(&h, 12))),
                1 => break,
                s => (8, u64::from(s)),
            };
            if size < hlen {
                if first {
                    return Err(Error::invalid("bad MP4 box"));
                }
                break;
            }
            first = false;
            let body_len = (size - hlen).min(len - pos - hlen.min(len - pos));
            match &typ {
                b"ftyp" => {
                    src.seek(pos + hlen);
                    let b = src.read_vec(body_len.min(4096))?;
                    qt = b.get(..4) == Some(b"qt  ");
                }
                b"moov" => {
                    if body_len > MAX_MOOV {
                        return Err(Error::unsupported("moov box too large"));
                    }
                    src.seek(pos + hlen);
                    moov = Some(src.read_vec(body_len)?);
                }
                b"moof" if body_len <= MAX_MOOF => {
                    src.seek(pos + hlen);
                    moofs.push((pos, src.read_vec(body_len)?));
                }
                _ => {}
            }
            pos = pos.saturating_add(size);
        }
        let moov = moov.ok_or_else(|| Error::invalid("MP4 without moov"))?;
        let mut d = Self {
            src,
            tracks: Vec::new(),
            state: Vec::new(),
        };
        d.parse_moov(&moov, qt)?;
        if d.state.is_empty() {
            return Err(Error::unsupported("no supported audio track"));
        }
        for (off, body) in &moofs {
            d.parse_moof(*off, body)?;
        }
        d.finish_tracks(&moov);
        for s in &mut d.state {
            s.set_next(0);
        }
        Ok(d)
    }

    fn parse_moov(&mut self, moov: &[u8], qt: bool) -> Result<()> {
        let mut trex: Vec<(u32, Trex)> = Vec::new();
        if let Some(mvex) = child(moov, b"mvex") {
            for (t, b) in Boxes::new(mvex) {
                if &t == b"trex" && b.len() >= 24 {
                    trex.push((
                        be32(b, 4),
                        Trex {
                            duration: be32(b, 12),
                            size: be32(b, 16),
                        },
                    ));
                }
            }
        }
        for (t, trak) in Boxes::new(moov) {
            if &t != b"trak" {
                continue;
            }
            let Some(mdia) = child(trak, b"mdia") else {
                continue;
            };
            let Some(hdlr) = child(mdia, b"hdlr") else {
                continue;
            };
            if hdlr.get(8..12) != Some(b"soun") {
                continue;
            }
            let id = child(trak, b"tkhd").map_or(0, |b| {
                if b.first() == Some(&1) {
                    be32(b, 20)
                } else {
                    be32(b, 12)
                }
            });
            let mdhd = child(mdia, b"mdhd").ok_or_else(|| Error::invalid("trak without mdhd"))?;
            let timescale = if mdhd.first() == Some(&1) {
                be32(mdhd, 20)
            } else {
                be32(mdhd, 12)
            };
            if timescale == 0 {
                continue;
            }
            let Some(stbl) = path(mdia, &[b"minf", b"stbl"]) else {
                continue;
            };
            let Some(stsd) = child(stbl, b"stsd") else {
                continue;
            };
            let Some((etype, ebody)) = Boxes::new(stsd.get(8..).unwrap_or(&[])).next() else {
                continue;
            };
            let entry = match parse_entry(&etype, ebody, qt, timescale) {
                Ok(Some(e)) => e,
                _ => continue,
            };
            let tables = match parse_stbl(stbl, self.src.len()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let mut track = entry.track;
            track.time_base = timescale;
            let mut tr = Mp4Track {
                id,
                timescale,
                tables,
                trex: trex
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map(|x| x.1)
                    .unwrap_or_default(),
                pcm_bpf: entry.pcm_bpf,
                edit: path(trak, &[b"edts", b"elst"]).and_then(parse_elst),
                pre_skip: entry.pre_skip,
                next: 0,
                chunk: 0,
                chunk_off: 0,
            };
            tr.tables.sync = tr.tables.sync.take().filter(|s| !s.is_empty());
            self.tracks.push(track);
            self.state.push(tr);
        }
        Ok(())
    }

    fn parse_moof(&mut self, moof_off: u64, body: &[u8]) -> Result<()> {
        let mut prev_end = moof_off;
        let mut first_traf = true;
        for (t, traf) in Boxes::new(body) {
            if &t != b"traf" {
                continue;
            }
            let Some(tfhd) = child(traf, b"tfhd") else {
                continue;
            };
            let mut b = Bytes::new(tfhd);
            let flags = b.u32()? & 0xFF_FFFF;
            let id = b.u32()?;
            let Some(ti) = self.state.iter().position(|s| s.id == id) else {
                first_traf = false;
                continue;
            };
            let mut base = if first_traf || flags & 0x2_0000 != 0 {
                moof_off
            } else {
                prev_end
            };
            first_traf = false;
            if flags & 1 != 0 {
                base = b.u64()?;
            }
            if flags & 2 != 0 {
                b.u32()?;
            }
            let tr = &mut self.state[ti];
            let mut def_dur = tr.trex.duration;
            let mut def_size = tr.trex.size;
            if flags & 8 != 0 {
                def_dur = b.u32()?;
            }
            if flags & 0x10 != 0 {
                def_size = b.u32()?;
            }
            if let Some(tfdt) = child(traf, b"tfdt") {
                let t = if tfdt.first() == Some(&1) {
                    u64::from(be32(tfdt, 4)) << 32 | u64::from(be32(tfdt, 8))
                } else {
                    u64::from(be32(tfdt, 4))
                };
                tr.tables.next_dts = t as i64;
            }
            let mut data_pos = base;
            for (tt, trun) in Boxes::new(traf) {
                if &tt != b"trun" {
                    continue;
                }
                let mut r = Bytes::new(trun);
                let tf = r.u32()? & 0xFF_FFFF;
                let count = u64::from(r.u32()?);
                if tf & 1 != 0 {
                    data_pos = base.wrapping_add_signed(i64::from(r.u32()? as i32));
                }
                if tf & 4 != 0 {
                    r.u32()?;
                }
                let per = [0x100, 0x200, 0x400, 0x800]
                    .iter()
                    .filter(|&&f| tf & f != 0)
                    .count() as u64
                    * 4;
                if per > 0 && count * per > r.remaining() as u64 {
                    return Err(Error::invalid("truncated trun"));
                }
                let mut bytes = 0u64;
                if per == 0 {
                    tr.tables.push_sizes(def_size, count)?;
                    tr.tables.push_times(def_dur, count);
                    bytes = count * u64::from(def_size);
                } else {
                    for _ in 0..count {
                        let dur = if tf & 0x100 != 0 { r.u32()? } else { def_dur };
                        let size = if tf & 0x200 != 0 { r.u32()? } else { def_size };
                        if tf & 0x400 != 0 {
                            r.u32()?;
                        }
                        if tf & 0x800 != 0 {
                            r.u32()?;
                        }
                        tr.tables.push_sizes(size, 1)?;
                        tr.tables.push_times(dur, 1);
                        bytes += u64::from(size);
                    }
                }
                tr.tables.push_chunk(data_pos, count);
                data_pos = data_pos.saturating_add(bytes);
            }
            prev_end = data_pos;
        }
        Ok(())
    }

    /// Resolve priming / duration / padding once all samples are known.
    fn finish_tracks(&mut self, moov: &[u8]) {
        let movie_ts = child(moov, b"mvhd").map_or(0, |b| {
            if b.first() == Some(&1) {
                be32(b, 20)
            } else {
                be32(b, 12)
            }
        });
        let smpb = find_itunsmpb(moov);
        for (t, s) in self.tracks.iter_mut().zip(&self.state) {
            let ts = u64::from(s.timescale);
            let total =
                (s.tables.end_dts() - s.tables.dts(0).min(s.tables.end_dts())).max(0) as u64;
            let (mut prim, mut dur) = (0u64, total);
            let mut from_edit = false;
            if let Some((mt, seg)) = s.edit {
                prim = mt.min(total);
                dur = if seg > 0 && movie_ts > 0 {
                    (u128::from(seg) * u128::from(ts) / u128::from(movie_ts)) as u64
                } else {
                    total - prim
                };
                dur = dur.min(total - prim);
                from_edit = prim > 0;
            }
            if s.pre_skip > prim {
                // Opus pre-skip that the edit list (if any) does not already cover
                let extra = s.pre_skip - prim;
                prim = s.pre_skip.min(total);
                dur = dur.saturating_sub(extra).min(total - prim);
                from_edit = true;
            }
            if !from_edit && t.codec == CodecId::Aac {
                if let Some((p, pad, n)) = smpb {
                    prim = p.min(total);
                    dur = if n > 0 {
                        n.min(total - prim)
                    } else {
                        total.saturating_sub(p + pad)
                    };
                }
            }
            let to_samples = |v: u64| {
                if ts == u64::from(t.sample_rate) || ts == 0 {
                    v
                } else {
                    (u128::from(v) * u128::from(t.sample_rate) / u128::from(ts)) as u64
                }
            };
            t.priming = to_samples(prim);
            t.padding = to_samples(total.saturating_sub(prim + dur));
            t.duration = Some(dur);
        }
    }
}

/// `(media_time, segment_duration)` of the first non-empty edit.
fn parse_elst(b: &[u8]) -> Option<(u64, u64)> {
    let mut r = Bytes::new(b);
    let v = r.u8().ok()?;
    r.skip(3).ok()?;
    let n = r.u32().ok()?;
    for _ in 0..n {
        let (seg, mt) = if v == 1 {
            (r.u64().ok()?, r.u64().ok()? as i64)
        } else {
            (u64::from(r.u32().ok()?), i64::from(r.u32().ok()? as i32))
        };
        r.skip(4).ok()?;
        if mt >= 0 {
            return Some((mt as u64, seg));
        }
    }
    None
}

fn parse_stbl(stbl: &[u8], file_len: u64) -> Result<Tables> {
    let mut t = Tables {
        var_limit: MAX_VAR_SAMPLES.min(file_len + 1024),
        ..Tables::default()
    };
    // sizes
    if let Some(stsz) = child(stbl, b"stsz") {
        let mut b = Bytes::new(stsz);
        b.u32()?;
        let size = b.u32()?;
        let count = u64::from(b.u32()?);
        if size != 0 {
            t.push_sizes(size, count)?;
        } else {
            if count * 4 > b.remaining() as u64 {
                return Err(Error::invalid("truncated stsz"));
            }
            for _ in 0..count {
                t.push_sizes(b.u32()?, 1)?;
            }
        }
    } else if let Some(stz2) = child(stbl, b"stz2") {
        let mut b = Bytes::new(stz2);
        b.u32()?;
        b.u24()?;
        let field = b.u8()?;
        let count = u64::from(b.u32()?);
        if !matches!(field, 4 | 8 | 16) || count * u64::from(field) > b.remaining() as u64 * 8 {
            return Err(Error::invalid("bad stz2"));
        }
        let data = b.rest();
        for i in 0..count as usize {
            let s = match field {
                4 => u32::from(if i % 2 == 0 {
                    data[i / 2] >> 4
                } else {
                    data[i / 2] & 0xF
                }),
                8 => u32::from(data[i]),
                _ => u32::from(be16(data, i * 2)),
            };
            t.push_sizes(s, 1)?;
        }
    }
    // times
    if let Some(stts) = child(stbl, b"stts") {
        let mut b = Bytes::new(stts);
        b.u32()?;
        let n = b.u32()?;
        for _ in 0..n {
            let count = u64::from(b.u32()?);
            let delta = b.u32()?;
            t.push_times(delta, count);
        }
    }
    // chunks
    let mut offsets: Vec<u64> = Vec::new();
    if let Some(stco) = child(stbl, b"stco") {
        let mut b = Bytes::new(stco);
        b.u32()?;
        let n = b.u32()? as usize;
        if n * 4 > b.remaining() {
            return Err(Error::invalid("truncated stco"));
        }
        for _ in 0..n {
            offsets.push(u64::from(b.u32()?));
        }
    } else if let Some(co64) = child(stbl, b"co64") {
        let mut b = Bytes::new(co64);
        b.u32()?;
        let n = b.u32()? as usize;
        if n * 8 > b.remaining() {
            return Err(Error::invalid("truncated co64"));
        }
        for _ in 0..n {
            offsets.push(b.u64()?);
        }
    }
    let mut stsc: Vec<(u64, u64)> = Vec::new();
    if let Some(s) = child(stbl, b"stsc") {
        let mut b = Bytes::new(s);
        b.u32()?;
        let n = b.u32()?;
        for _ in 0..n {
            let first = u64::from(b.u32()?);
            let spc = u64::from(b.u32()?);
            b.u32()?;
            if first == 0 || stsc.last().is_some_and(|l| l.0 >= first) {
                return Err(Error::invalid("bad stsc"));
            }
            stsc.push((first - 1, spc));
        }
    }
    let total = t.size_count.min(t.time_count);
    let mut k = 0;
    for (ci, &off) in offsets.iter().enumerate() {
        while k + 1 < stsc.len() && stsc[k + 1].0 <= ci as u64 {
            k += 1;
        }
        let spc = stsc.get(k).filter(|s| s.0 <= ci as u64).map_or(0, |s| s.1);
        if t.chunk_samples >= total {
            break;
        }
        t.push_chunk(off, spc.min(total - t.chunk_samples));
    }
    if let Some(stss) = child(stbl, b"stss") {
        let mut b = Bytes::new(stss);
        b.u32()?;
        let n = b.u32()? as usize;
        if n * 4 <= b.remaining() {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(u64::from(b.u32()?.saturating_sub(1)));
            }
            v.sort_unstable();
            t.sync = Some(v);
        }
    }
    Ok(t)
}

impl Demuxer for Mp4Demuxer {
    fn format_name(&self) -> &str {
        "MP4"
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<DemuxPacket>> {
        loop {
            let Some(ti) = self
                .state
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.file_pos().map(|p| (p, i)))
                .min()
                .map(|(_, i)| i)
            else {
                return Ok(None);
            };
            let len = self.src.len();
            let s = &mut self.state[ti];
            let pos = s.file_pos().unwrap_or(0);
            let i = s.next;
            let (n, bytes) = if s.pcm_bpf > 0 {
                let n = s.pcm_group();
                let bytes: u64 = (i..i + n).map(|k| s.sample_bytes(k)).sum();
                (n, bytes)
            } else {
                (1, u64::from(s.tables.size(i)))
            };
            if bytes > u64::from(MAX_SAMPLE) * n || pos.saturating_add(bytes) > len {
                // damaged table or truncated file: this track ends here
                s.next = s.tables.count();
                continue;
            }
            let dts = s.tables.dts(i);
            let duration = if i + n >= s.tables.count() {
                (i..i + n)
                    .map(|k| u64::from(s.tables.run(k).map_or(0, |r| r.delta)))
                    .sum::<u64>()
            } else {
                s.tables.dts(i + n).saturating_sub(dts).max(0) as u64
            };
            let duration = duration.min(u64::from(u32::MAX)) as u32;
            let keyframe = s.tables.is_sync(i);
            s.advance(n, bytes);
            self.src.seek(pos);
            let data = self.src.read_vec(bytes)?;
            return Ok(Some(DemuxPacket {
                track: ti,
                data,
                pts: dts,
                duration,
                keyframe,
            }));
        }
    }

    fn seek(&mut self, track: usize, pts: i64) -> Result<i64> {
        if track >= self.state.len() {
            return Err(Error::InvalidArgument(format!("no track {track}")));
        }
        let main_ts = self.state[track].timescale;
        let mut result = pts;
        for (ti, s) in self.state.iter_mut().enumerate() {
            let t = if ti == track {
                pts
            } else {
                (i128::from(pts) * i128::from(s.timescale) / i128::from(main_ts.max(1))) as i64
            };
            let mut i = s.tables.sample_at(t);
            if s.tables.sync.is_some() {
                while i > 0 && !s.tables.is_sync(i) {
                    i -= 1;
                }
            }
            s.set_next(i);
            if ti == track {
                result = if s.next >= s.tables.count() {
                    s.tables.end_dts()
                } else {
                    s.tables.dts(s.next)
                };
            }
        }
        Ok(result)
    }
}
