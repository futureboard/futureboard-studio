//! Ogg (RFC 3533) reading and writing, with the Vorbis, Opus (RFC 7845) and FLAC mappings.

use std::collections::VecDeque;
use std::io::Write;

use mkff_audio_core::crc::crc32_ogg;
use mkff_audio_core::{CodecId, Error, Result};

use crate::flac::{fixed_block_size, FrameHeader, StreamInfo};
use crate::io::{le32, Source};
use crate::xiph::{opus_packet_duration, xiph_lace, OpusHead, VorbisInfo};
use crate::{DemuxPacket, Demuxer, Track};

const FLAG_CONTINUED: u8 = 1;
const FLAG_BOS: u8 = 2;
const FLAG_EOS: u8 = 4;

/// Largest packet the reader reassembles.
const MAX_PACKET: usize = 16 << 20;

#[derive(Clone, Debug)]
struct Page {
    offset: u64,
    flags: u8,
    granule: i64,
    serial: u32,
    lacing: Vec<u8>,
    data: Vec<u8>,
}

impl Page {
    fn len(&self) -> u64 {
        27 + self.lacing.len() as u64 + self.data.len() as u64
    }
}

/// Read and CRC-check the page at `at`.
fn read_page(src: &mut Source, at: u64) -> Result<Option<Page>> {
    let mut h = [0u8; 27];
    if src.read_at(at, &mut h)? < 27 || &h[..4] != b"OggS" || h[4] != 0 {
        return Ok(None);
    }
    let nseg = usize::from(h[26]);
    let mut raw = vec![0u8; 27 + nseg];
    raw[..27].copy_from_slice(&h);
    if src.read(&mut raw[27..])? < nseg {
        return Ok(None);
    }
    let len: usize = raw[27..].iter().map(|&b| usize::from(b)).sum();
    raw.resize(27 + nseg + len, 0);
    if src.read(&mut raw[27 + nseg..])? < len {
        return Ok(None);
    }
    let want = le32(&raw, 22);
    raw[22..26].fill(0);
    if crc32_ogg(&raw) != want {
        return Ok(None);
    }
    let data = raw.split_off(27 + nseg);
    let lacing = raw.split_off(27);
    Ok(Some(Page {
        offset: at,
        flags: h[5],
        granule: i64::from_le_bytes([h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13]]),
        serial: le32(&h, 14),
        lacing,
        data,
    }))
}

/// The first valid page starting at or after `from` and before `limit`.
fn find_page(src: &mut Source, from: u64, limit: u64) -> Result<Option<Page>> {
    let mut at = from;
    let limit = limit.min(src.len());
    while at < limit {
        src.seek(at);
        let win = src.read_upto(64 * 1024)?;
        if win.len() < 27 {
            return Ok(None);
        }
        let n = win.len();
        let mut i = 0;
        while i + 4 <= n && at + (i as u64) < limit {
            if &win[i..i + 4] == b"OggS" {
                if let Some(p) = read_page(src, at + i as u64)? {
                    return Ok(Some(p));
                }
            }
            i += 1;
        }
        at += n as u64 - 3;
    }
    Ok(None)
}

#[derive(Clone, Debug)]
enum Kind {
    Vorbis(VorbisInfo),
    Opus,
    Flac {
        info: StreamInfo,
        fixed_block: Option<u32>,
    },
}

struct Stream {
    serial: u32,
    track: usize,
    kind: Kind,
    /// Header packets still expected (`None`: until the first audio packet, FLAC only).
    headers_left: Option<usize>,
    headers: Vec<Vec<u8>>,
    flac_info_raw: Vec<u8>,
    partial: Vec<u8>,
    partial_valid: bool,
    last_bs: Option<u32>,
    next_pts: Option<i64>,
    /// Packets waiting for a granule position to anchor their timestamps.
    unanchored: Vec<(Vec<u8>, u32)>,
    pts_offset: i64,
    eos: bool,
}

impl Stream {
    fn reset(&mut self) {
        self.partial.clear();
        self.partial_valid = false;
        self.last_bs = None;
        self.next_pts = None;
        self.unanchored.clear();
        self.eos = false;
    }

    fn packet_duration(&mut self, p: &[u8]) -> u32 {
        match &mut self.kind {
            Kind::Opus => opus_packet_duration(p).unwrap_or(0),
            Kind::Vorbis(v) => match v.packet_blocksize(p) {
                Some(bs) => {
                    let d = self.last_bs.map_or(0, |prev| prev / 4 + bs / 4);
                    self.last_bs = Some(bs);
                    d
                }
                None => 0,
            },
            Kind::Flac { .. } => FrameHeader::parse(p).map_or(0, |h| h.block_size),
        }
    }

    fn flac_pts(&mut self, p: &[u8]) -> Option<i64> {
        if let Kind::Flac { info, fixed_block } = &mut self.kind {
            let h = FrameHeader::parse(p)?;
            let fb = *fixed_block.get_or_insert_with(|| fixed_block_size(info, Some(&h)));
            Some(h.first_sample(fb) as i64)
        } else {
            None
        }
    }
}

/// Ogg demuxer: Vorbis, Opus and FLAC logical streams become tracks (in BOS order); other
/// streams (Theora, Skeleton, …) are skipped. Packets come in file order. Chained files
/// (several links) are read up to the end of the first link's streams; later links with new
/// serial numbers are ignored.
pub(crate) struct OggDemuxer {
    src: Source,
    tracks: Vec<Track>,
    streams: Vec<Stream>,
    ignored: Vec<u32>,
    data_start: u64,
    pos: u64,
    queue: VecDeque<DemuxPacket>,
}

impl OggDemuxer {
    pub fn open(src: Source) -> Result<Self> {
        let mut d = Self {
            src,
            tracks: Vec::new(),
            streams: Vec::new(),
            ignored: Vec::new(),
            data_start: 0,
            pos: 0,
            queue: VecDeque::new(),
        };
        // beginning-of-stream pages
        loop {
            let Some(page) = read_page(&mut d.src, d.pos)? else {
                if d.pos == 0 {
                    return Err(Error::invalid("not an Ogg stream"));
                }
                break;
            };
            if page.flags & FLAG_BOS == 0 {
                break;
            }
            d.pos += page.len();
            let first = first_packet(&page);
            d.add_stream(page.serial, &first)?;
        }
        if d.streams.is_empty() {
            return Err(Error::unsupported(
                "Ogg file without Vorbis, Opus or FLAC streams",
            ));
        }
        // header packets
        let mut guard = 0;
        while d.streams.iter().any(|s| s.headers_left != Some(0)) {
            guard += 1;
            if guard > 10_000 || !d.pump()? {
                break;
            }
        }
        for s in &mut d.streams {
            if s.headers_left.is_some_and(|n| n > 0) {
                return Err(Error::invalid("Ogg stream headers incomplete"));
            }
        }
        d.data_start = d.pos;
        // finish the track descriptions
        for i in 0..d.streams.len() {
            let s = &d.streams[i];
            let t = &mut d.tracks[s.track];
            match &s.kind {
                Kind::Vorbis(_) => {
                    let h: Vec<&[u8]> = s.headers.iter().map(Vec::as_slice).collect();
                    t.config = xiph_lace(&h);
                }
                Kind::Opus => t.config = s.headers[0].clone(),
                Kind::Flac { .. } => t.config = s.flac_info_raw.clone(),
            }
        }
        // anchor the first timestamps (queues the first packets) and find the last granules
        let mut pages = 0;
        while d.streams.iter().any(|s| s.next_pts.is_none() && !s.eos) && pages < 2000 {
            if !d.pump()? {
                break;
            }
            pages += 1;
        }
        let first_pts: Vec<Option<i64>> = d
            .streams
            .iter()
            .map(|s| d.queue.iter().find(|p| p.track == s.track).map(|p| p.pts))
            .collect();
        // a Vorbis stream whose first page claims fewer samples than its packets hold starts
        // with samples to discard: shift its timeline so it starts at 0
        for (i, s) in d.streams.iter_mut().enumerate() {
            if let (Kind::Vorbis(_), Some(fp)) = (&s.kind, first_pts[i]) {
                if fp < 0 {
                    s.pts_offset = -fp;
                    d.tracks[s.track].priming = (-fp) as u64;
                    for p in d.queue.iter_mut().filter(|p| p.track == s.track) {
                        p.pts += -fp;
                    }
                    if let Some(n) = &mut s.next_pts {
                        *n += -fp;
                    }
                }
            }
        }
        let last = d.last_granules()?;
        for i in 0..d.streams.len() {
            let (Some(g), Some(fp)) = (last[i], first_pts[i]) else {
                continue;
            };
            let s = &d.streams[i];
            let track = s.track;
            let off = s.pts_offset;
            let first = fp + off;
            let priming = d.tracks[track].priming as i64;
            let dur = g
                .saturating_add(off)
                .saturating_sub(first)
                .saturating_sub(priming);
            d.tracks[track].duration = (dur >= 0).then_some(dur as u64);
            // padding: decoded samples past the last granule position
            let decoded_end = if s.eos {
                d.queue
                    .iter()
                    .filter(|p| p.track == track)
                    .map(|p| p.pts + i64::from(p.duration))
                    .max()
            } else {
                d.tail_end(i)?.map(|e| e + off)
            };
            if let Some(e) = decoded_end {
                d.tracks[track].padding =
                    e.saturating_sub(g.saturating_add(off)).clamp(0, 1 << 20) as u64;
            }
        }
        Ok(d)
    }

    /// Where decoding the last pages of stream `si` ends (granule units, without the stream's
    /// pts offset): the previous granule page's position plus the durations of the packets that
    /// complete after it. `None` when the last two granule pages are not near the end.
    fn tail_end(&mut self, si: usize) -> Result<Option<i64>> {
        let serial = self.streams[si].serial;
        let len = self.src.len();
        let start = len.saturating_sub(256 << 10).max(self.data_start);
        let mut pages = Vec::new();
        let mut at = start;
        while let Some(p) = find_page(&mut self.src, at, len)? {
            at = p.offset + p.len();
            if p.serial == serial {
                pages.push(p);
            }
        }
        let granule_pages: Vec<usize> = (0..pages.len())
            .filter(|&k| pages[k].granule != -1)
            .collect();
        let [.., prev, last] = granule_pages[..] else {
            return Ok(None);
        };
        let mut scratch = Stream {
            serial,
            track: 0,
            kind: self.streams[si].kind.clone(),
            headers_left: Some(0),
            headers: Vec::new(),
            flac_info_raw: Vec::new(),
            partial: Vec::new(),
            partial_valid: false,
            last_bs: None,
            next_pts: None,
            unanchored: Vec::new(),
            pts_offset: 0,
            eos: false,
        };
        let mut sum = 0i64;
        let mut cur: Vec<u8> = Vec::new();
        let mut valid = !pages[prev].flags & FLAG_CONTINUED != 0;
        for (k, p) in pages.iter().enumerate().take(last + 1).skip(prev) {
            let mut off = 0usize;
            for &l in &p.lacing {
                let l = usize::from(l);
                if valid {
                    cur.extend_from_slice(&p.data[off..off + l]);
                }
                off += l;
                if l < 255 {
                    if valid {
                        let d = scratch.packet_duration(&cur);
                        if k > prev {
                            sum += i64::from(d);
                        }
                    }
                    cur.clear();
                    valid = true;
                }
            }
        }
        Ok(Some(pages[prev].granule.saturating_add(sum)))
    }

    fn add_stream(&mut self, serial: u32, p: &[u8]) -> Result<()> {
        let track = self.tracks.len();
        let (kind, t, left, info_raw) = if p.len() >= 7 && p[0] == 1 && &p[1..7] == b"vorbis" {
            let (ch, rate, bs) = VorbisInfo::parse_id(p)?;
            let info = VorbisInfo {
                channels: ch,
                sample_rate: rate,
                blocksizes: bs,
                mode_blockflags: Vec::new(),
            };
            (
                Kind::Vorbis(info),
                Track::new(CodecId::Vorbis, rate, ch, "Vorbis"),
                Some(2),
                Vec::new(),
            )
        } else if p.starts_with(b"OpusHead") {
            let h = OpusHead::parse(p)?;
            let mut t = Track::new(CodecId::Opus, 48000, h.channels, "Opus");
            t.priming = u64::from(h.pre_skip);
            (Kind::Opus, t, Some(1), Vec::new())
        } else if p.len() >= 51 && p[0] == 0x7F && &p[1..5] == b"FLAC" && &p[9..13] == b"fLaC" {
            let n = usize::from(u16::from_be_bytes([p[7], p[8]]));
            let raw = p[17..51].to_vec();
            let info = StreamInfo::parse(&raw)?;
            let mut t = info.track(&raw);
            t.duration = None;
            (
                Kind::Flac {
                    info,
                    fixed_block: None,
                },
                t,
                if n == 0 { None } else { Some(n) },
                raw,
            )
        } else {
            self.ignored.push(serial);
            return Ok(());
        };
        self.tracks.push(t);
        self.streams.push(Stream {
            serial,
            track,
            kind,
            headers_left: left,
            headers: vec![p.to_vec()],
            flac_info_raw: info_raw,
            partial: Vec::new(),
            partial_valid: true,
            last_bs: None,
            next_pts: None,
            unanchored: Vec::new(),
            pts_offset: 0,
            eos: false,
        });
        Ok(())
    }

    /// Read the page at `pos` (resynchronising after damage) and queue its packets. Returns
    /// false at the end of the data.
    fn pump(&mut self) -> Result<bool> {
        let page = match read_page(&mut self.src, self.pos)? {
            Some(p) => p,
            None => match find_page(&mut self.src, self.pos + 1, u64::MAX)? {
                Some(p) => {
                    for s in &mut self.streams {
                        s.partial.clear();
                        s.partial_valid = false;
                    }
                    p
                }
                None => {
                    self.pos = self.src.len();
                    return Ok(false);
                }
            },
        };
        self.pos = page.offset + page.len();
        let Some(si) = self.streams.iter().position(|s| s.serial == page.serial) else {
            return Ok(true);
        };
        // split into packets
        let mut done: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<u8>;
        let mut valid;
        {
            let s = &mut self.streams[si];
            if page.flags & FLAG_CONTINUED != 0 {
                cur = std::mem::take(&mut s.partial);
                valid = s.partial_valid && !cur.is_empty();
            } else {
                s.partial.clear();
                cur = Vec::new();
                valid = true;
            }
        }
        let mut off = 0usize;
        for &l in &page.lacing {
            let l = usize::from(l);
            if valid {
                if cur.len() + l > MAX_PACKET {
                    valid = false;
                    cur.clear();
                } else {
                    cur.extend_from_slice(&page.data[off..off + l]);
                }
            }
            off += l;
            if l < 255 {
                if valid {
                    done.push(std::mem::take(&mut cur));
                }
                cur.clear();
                valid = true;
            }
        }
        {
            let s = &mut self.streams[si];
            s.partial = cur;
            s.partial_valid = valid;
            if page.flags & FLAG_EOS != 0 {
                s.eos = true;
            }
        }
        let mut audio: Vec<Vec<u8>> = Vec::new();
        for p in done {
            let s = &mut self.streams[si];
            match s.headers_left {
                Some(0) => audio.push(p),
                Some(n) => {
                    s.headers.push(p);
                    s.headers_left = Some(n - 1);
                    if n == 1 {
                        if let Kind::Vorbis(v) = &mut s.kind {
                            let setup = s.headers.get(2).cloned().unwrap_or_default();
                            v.mode_blockflags = VorbisInfo::parse_setup_modes(&setup)?;
                        }
                    }
                }
                None => {
                    if p.first() == Some(&0xFF) {
                        s.headers_left = Some(0);
                        audio.push(p);
                    } else {
                        s.headers.push(p);
                    }
                }
            }
        }
        if audio.is_empty() {
            return Ok(true);
        }
        let s = &mut self.streams[si];
        let track = s.track;
        if let Kind::Flac { .. } = s.kind {
            for p in audio {
                let d = s.packet_duration(&p);
                let pts = s.flac_pts(&p).unwrap_or_else(|| s.next_pts.unwrap_or(0));
                s.next_pts = Some(pts.saturating_add(i64::from(d)));
                self.queue.push_back(DemuxPacket {
                    track,
                    data: p,
                    pts: pts.saturating_add(s.pts_offset),
                    duration: d,
                    keyframe: true,
                });
            }
            return Ok(true);
        }
        let durs: Vec<u32> = audio.iter().map(|p| s.packet_duration(p)).collect();
        match s.next_pts {
            Some(mut n) => {
                for (p, d) in audio.into_iter().zip(durs) {
                    self.queue.push_back(DemuxPacket {
                        track,
                        data: p,
                        pts: n,
                        duration: d,
                        keyframe: true,
                    });
                    n = n.saturating_add(i64::from(d));
                }
                s.next_pts = Some(n);
            }
            None => {
                s.unanchored.extend(audio.into_iter().zip(durs));
                if page.granule != -1 {
                    let pend = std::mem::take(&mut s.unanchored);
                    let total: i64 = pend.iter().map(|(_, d)| i64::from(*d)).sum();
                    let mut n = page
                        .granule
                        .saturating_add(s.pts_offset)
                        .saturating_sub(total);
                    if page.flags & FLAG_EOS != 0 && n < s.pts_offset {
                        // a single-page stream: its granule trims the end, the start is 0
                        n = s.pts_offset;
                    }
                    for (p, d) in pend {
                        self.queue.push_back(DemuxPacket {
                            track,
                            data: p,
                            pts: n,
                            duration: d,
                            keyframe: true,
                        });
                        n = n.saturating_add(i64::from(d));
                    }
                    s.next_pts = Some(n);
                }
            }
        }
        Ok(true)
    }

    /// The last granule position of each stream (scanning back from the end).
    fn last_granules(&mut self) -> Result<Vec<Option<i64>>> {
        let n = self.streams.len();
        let mut out = vec![None; n];
        let mut end = self.src.len();
        let mut chunk = 64 * 1024u64;
        loop {
            let start = end.saturating_sub(chunk).max(self.data_start);
            let mut at = start;
            let mut found = vec![None; n];
            while let Some(p) = find_page(&mut self.src, at, end)? {
                if let Some(i) = self.streams.iter().position(|s| s.serial == p.serial) {
                    if p.granule != -1 {
                        found[i] = Some(p.granule);
                    }
                }
                at = p.offset + p.len();
            }
            for (o, f) in out.iter_mut().zip(found) {
                if o.is_none() {
                    *o = f;
                }
            }
            if out.iter().all(Option::is_some) || start <= self.data_start {
                return Ok(out);
            }
            // overlap by one maximal page so a page straddling `start` is seen next time
            end = (start + 65_307).min(end);
            chunk = (chunk * 2).min(16 << 20);
        }
    }

    fn reset_streams(&mut self) {
        for s in &mut self.streams {
            s.reset();
        }
        self.queue.clear();
    }

    /// Fill the queue until it holds a packet of `track`; return its pts.
    fn first_pts_of(&mut self, track: usize) -> Result<Option<i64>> {
        let mut pages = 0;
        loop {
            if let Some(p) = self.queue.iter().find(|p| p.track == track) {
                return Ok(Some(p.pts));
            }
            if pages > 100_000 || !self.pump()? {
                return Ok(None);
            }
            pages += 1;
        }
    }
}

fn first_packet(page: &Page) -> Vec<u8> {
    let mut n = 0usize;
    for &l in &page.lacing {
        n += usize::from(l);
        if l < 255 {
            break;
        }
    }
    page.data[..n.min(page.data.len())].to_vec()
}

impl Demuxer for OggDemuxer {
    fn format_name(&self) -> &str {
        "Ogg"
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<DemuxPacket>> {
        loop {
            if let Some(p) = self.queue.pop_front() {
                return Ok(Some(p));
            }
            if !self.pump()? {
                return Ok(None);
            }
        }
    }

    fn seek(&mut self, track: usize, pts: i64) -> Result<i64> {
        let si = self
            .streams
            .iter()
            .position(|s| s.track == track)
            .ok_or_else(|| Error::InvalidArgument(format!("no track {track}")))?;
        let serial = self.streams[si].serial;
        let g = pts.saturating_sub(self.streams[si].pts_offset);
        let len = self.src.len();
        let (mut lo, mut hi) = (self.data_start, len);
        let mut best: Option<u64> = None;
        while hi > lo + 64 * 1024 {
            let mid = lo + (hi - lo) / 2;
            let mut at = mid;
            let mut hit = None;
            while let Some(p) = find_page(&mut self.src, at, hi)? {
                if p.serial == serial && p.granule != -1 {
                    hit = Some(p);
                    break;
                }
                at = p.offset + p.len();
            }
            match hit {
                Some(p) if p.granule <= g => {
                    best = Some(p.offset);
                    lo = p.offset + p.len();
                }
                _ => hi = mid,
            }
        }
        // start a little before the bisection result so the page preceding it is seen too
        let mut at = best.map_or(self.data_start, |b| {
            b.saturating_sub(65_536).max(self.data_start)
        });
        let mut cand = best;
        let mut prev = None;
        // the last pages of the stream seen, to recover the packet before the landing point
        let mut recent: VecDeque<Page> = VecDeque::new();
        // the page right after the candidate, when it ends past the target
        let mut after: Option<Page> = None;
        while let Some(p) = find_page(&mut self.src, at, len)? {
            at = p.offset + p.len();
            if p.serial != serial {
                continue;
            }
            if p.granule != -1 {
                if p.granule > g {
                    if recent.back().is_some_and(|r| Some(r.offset) == cand) {
                        after = Some(p);
                    }
                    break;
                }
                // the end-of-stream page's granule trims its packets: anchor on the one before
                if p.flags & FLAG_EOS != 0 && prev.is_some() {
                    cand = prev;
                    break;
                }
                prev = Some(p.offset);
                cand = Some(p.offset);
            }
            if recent.len() == 4 {
                recent.pop_front();
            }
            recent.push_back(p);
        }
        // A following page that starts with a fresh packet begins exactly at the candidate's
        // granule: land there. Otherwise land on the candidate (its complete packets end at or
        // before the target).
        let (landing, seed_page) = match (&after, cand) {
            (Some(q), Some(c)) if q.flags & (FLAG_CONTINUED | FLAG_EOS) == 0 => {
                (Some(q.offset), Some(c))
            }
            (_, Some(c)) => {
                let k = recent.iter().position(|p| p.offset == c);
                (
                    Some(c),
                    k.and_then(|k| k.checked_sub(1))
                        .and_then(|k| recent.get(k))
                        .map(|p| p.offset),
                )
            }
            (_, None) => (None, None),
        };
        // Vorbis: the block size of the packet preceding the landing page's first complete packet
        let seed_bs = seed_page.and_then(|off| {
            let page = recent.iter().find(|p| p.offset == off)?;
            let Kind::Vorbis(v) = &self.streams[si].kind else {
                return None;
            };
            v.packet_blocksize(&[*last_packet_start(page)?])
        });
        self.reset_streams();
        match landing {
            Some(off) => {
                self.pos = off;
                for s in &mut self.streams {
                    // the packets completed on this page are anchored by its granule
                    s.partial_valid = false;
                }
                self.streams[si].last_bs = seed_bs;
            }
            None => {
                self.pos = self.data_start;
                for s in &mut self.streams {
                    s.partial_valid = true;
                }
            }
        }
        Ok(self.first_pts_of(track)?.unwrap_or(pts))
    }
}

/// The first byte of the last packet that starts on `page` (`None` if none starts there).
fn last_packet_start(page: &Page) -> Option<&u8> {
    let mut start = if page.flags & FLAG_CONTINUED != 0 {
        None
    } else {
        Some(0usize)
    };
    let mut off = 0usize;
    let n = page.lacing.len();
    for (i, &l) in page.lacing.iter().enumerate() {
        off += usize::from(l);
        // a terminator followed by more lacing values starts a packet
        if l < 255 && i + 1 < n {
            start = Some(off);
        }
    }
    page.data.get(start?)
}

/// A generic Ogg page writer for one logical stream.
///
/// Packets are buffered into pages of about 4 KiB (at most 255 lacing values); a packet that
/// does not fit is continued on the next page. The page holding the last packet stays buffered
/// until [`finish`](OggWriter::finish), which writes it with the end-of-stream flag. The first
/// page gets the beginning-of-stream flag; codec mappings want the first header packet alone on
/// it, which [`write_packet`](OggWriter::write_packet) with `flush = true` achieves.
pub struct OggWriter<W: Write> {
    w: W,
    serial: u32,
    seq: u32,
    lacing: Vec<u8>,
    data: Vec<u8>,
    granule: i64,
    continued: bool,
    first: bool,
    page_bytes: usize,
    last_granule: i64,
    /// Where the last packet starts on the current page (lacing index, data offset) and the
    /// granule before it, when it starts on this page after another packet.
    last_start: Option<(usize, usize, i64)>,
}

impl<W: Write> OggWriter<W> {
    /// A writer for the logical stream `serial`.
    pub fn new(w: W, serial: u32) -> Self {
        Self {
            w,
            serial,
            seq: 0,
            lacing: Vec::new(),
            data: Vec::new(),
            granule: -1,
            continued: false,
            first: true,
            page_bytes: 4096,
            last_granule: 0,
            last_start: None,
        }
    }

    /// Target page payload size (default 4096 bytes; pages close when they reach it).
    pub fn set_page_size(&mut self, bytes: usize) {
        self.page_bytes = bytes.clamp(256, 65_025);
    }

    fn emit(&mut self, eos: bool) -> Result<()> {
        let mut flags = 0;
        if self.continued {
            flags |= FLAG_CONTINUED;
        }
        if self.first {
            flags |= FLAG_BOS;
        }
        if eos {
            flags |= FLAG_EOS;
        }
        let mut page = Vec::with_capacity(27 + self.lacing.len() + self.data.len());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&self.granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.seq.to_le_bytes());
        page.extend_from_slice(&[0; 4]);
        page.push(self.lacing.len() as u8);
        page.extend_from_slice(&self.lacing);
        page.extend_from_slice(&self.data);
        let crc = crc32_ogg(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        self.w.write_all(&page)?;
        self.seq = self.seq.wrapping_add(1);
        self.first = false;
        // a page ending in a 255 lacing value leaves its packet unfinished
        self.continued = self.lacing.last() == Some(&255);
        self.lacing.clear();
        self.data.clear();
        self.granule = -1;
        Ok(())
    }

    /// Append a packet whose last sample has granule position `granule`. With `flush`, the page
    /// is closed after it (header packets that must end a page).
    pub fn write_packet(&mut self, packet: &[u8], granule: i64, flush: bool) -> Result<()> {
        if !self.lacing.is_empty() && self.data.len() >= self.page_bytes {
            self.emit(false)?;
        }
        let mut rest = packet;
        self.last_start =
            (!self.lacing.is_empty()).then_some((self.lacing.len(), self.data.len(), self.granule));
        loop {
            if self.lacing.len() == 255 {
                self.emit(false)?;
                self.last_start = None;
            }
            let n = rest.len().min(255);
            self.lacing.push(n as u8);
            self.data.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if n < 255 {
                break;
            }
        }
        self.granule = granule;
        self.last_granule = granule;
        if flush {
            self.emit(false)?;
        }
        Ok(())
    }

    /// Close the current page (if it holds anything).
    pub fn flush(&mut self) -> Result<()> {
        if !self.lacing.is_empty() {
            self.emit(false)?;
        }
        Ok(())
    }

    /// Write the last page with the end-of-stream flag. `final_granule` replaces the granule of
    /// the last page (Opus / Vorbis end trimming: the true end position); `None` keeps it.
    pub fn finish(mut self, final_granule: Option<i64>) -> Result<W> {
        if self.lacing.is_empty() {
            // an empty EOS page carries the end position
            self.granule = final_granule.unwrap_or(self.last_granule);
        } else if let Some(g) = final_granule {
            if let (true, Some((li, di, before))) = (g < self.granule, self.last_start) {
                // End trimming: give the last packet a page of its own, so the trimmed position
                // is never on the first audio page (where readers would take it as start
                // trimming).
                let lacing = self.lacing.split_off(li);
                let data = self.data.split_off(di);
                self.granule = before;
                self.emit(false)?;
                self.lacing = lacing;
                self.data = data;
            }
            self.granule = g;
        }
        self.emit(true)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Ogg Opus (RFC 7845) writer: `OpusHead` and `OpusTags` pages, then audio packets with granule
/// positions counted from their TOC durations (pre-skip included).
pub struct OggOpusWriter<W: Write> {
    ogg: OggWriter<W>,
    pre_skip: u64,
    granule: u64,
}

impl<W: Write> OggOpusWriter<W> {
    /// Start a stream from an `OpusHead` packet (as produced by the Opus encoder).
    pub fn new(w: W, serial: u32, opus_head: &[u8], vendor: &str) -> Result<Self> {
        let h = OpusHead::parse(opus_head)?;
        let mut ogg = OggWriter::new(w, serial);
        ogg.write_packet(opus_head, 0, true)?;
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        ogg.write_packet(&tags, 0, true)?;
        Ok(Self {
            ogg,
            pre_skip: u64::from(h.pre_skip),
            granule: 0,
        })
    }

    /// A minimal `OpusHead` (mapping family 0 for 1–2 channels, 1 for more is the caller's job).
    pub fn opus_head(channels: u8, pre_skip: u16, input_rate: u32) -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.push(1);
        h.push(channels);
        h.extend_from_slice(&pre_skip.to_le_bytes());
        h.extend_from_slice(&input_rate.to_le_bytes());
        h.extend_from_slice(&0i16.to_le_bytes());
        h.push(0);
        h
    }

    /// Append one Opus packet.
    pub fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        let d = opus_packet_duration(packet)
            .ok_or_else(|| Error::InvalidArgument("bad Opus packet".into()))?;
        self.granule += u64::from(d);
        self.ogg.write_packet(packet, self.granule as i64, false)
    }

    /// End the stream. With `total_samples` (the real length at 48 kHz, without pre-skip), the
    /// last granule position is `pre_skip + total_samples`, so readers trim the padding.
    pub fn finish(self, total_samples: Option<u64>) -> Result<W> {
        let g = total_samples.map(|t| (self.pre_skip + t).min(self.granule) as i64);
        self.ogg.finish(g)
    }
}

/// The two header packets of FLAC in Ogg: the mapping packet (with STREAMINFO) and a
/// VORBIS_COMMENT block with `vendor`.
pub fn ogg_flac_headers(streaminfo: &[u8], vendor: &str) -> Result<[Vec<u8>; 2]> {
    StreamInfo::parse(streaminfo)?;
    let mut a = vec![0x7F];
    a.extend_from_slice(b"FLAC");
    a.extend_from_slice(&[1, 0, 0, 1]);
    a.extend_from_slice(b"fLaC");
    a.extend_from_slice(&[0, 0, 0, 34]);
    a.extend_from_slice(&streaminfo[..34]);
    let mut body = (vendor.len() as u32).to_le_bytes().to_vec();
    body.extend_from_slice(vendor.as_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let mut b = vec![0x84];
    b.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    b.extend_from_slice(&body);
    Ok([a, b])
}
