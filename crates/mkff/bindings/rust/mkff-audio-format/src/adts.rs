//! ADTS (ISO/IEC 13818-7 / 14496-3 audio data transport stream) reading and writing.

use std::io::Write;

use mkff_audio_core::{bits::BitReader, CodecId, Error, Result};

use crate::io::Source;
use crate::{id3, DemuxPacket, Demuxer, Track};

/// The sampling frequencies of `sampling_frequency_index` 0..=12.
pub(crate) const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// A parsed ADTS header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdtsHeader {
    /// `profile_ObjectType` (audio object type - 1).
    pub profile: u8,
    pub sf_index: u8,
    pub channel_config: u8,
    pub protection: bool,
    pub frame_len: usize,
    /// `number_of_raw_data_blocks_in_frame + 1`.
    pub blocks: usize,
}

impl AdtsHeader {
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
            return None;
        }
        let protection = b[1] & 1 == 0;
        let profile = b[2] >> 6;
        let sf_index = (b[2] >> 2) & 0xF;
        let channel_config = ((b[2] & 1) << 2) | (b[3] >> 6);
        let frame_len =
            (usize::from(b[3] & 3) << 11) | (usize::from(b[4]) << 3) | usize::from(b[5] >> 5);
        let blocks = usize::from(b[6] & 3) + 1;
        let hlen = if protection { 9 } else { 7 };
        if sf_index > 12 || frame_len <= hlen {
            return None;
        }
        Some(Self {
            profile,
            sf_index,
            channel_config,
            protection,
            frame_len,
            blocks,
        })
    }

    pub fn header_len(&self) -> usize {
        if self.protection {
            7 + 2 * self.blocks
        } else {
            7
        }
    }

    fn same_stream(&self, o: &Self) -> bool {
        self.profile == o.profile
            && self.sf_index == o.sf_index
            && self.channel_config == o.channel_config
    }

    /// A 2-byte `AudioSpecificConfig` for this stream.
    pub fn asc(&self) -> Vec<u8> {
        let aot = u16::from(self.profile) + 1;
        let v =
            (aot << 11) | (u16::from(self.sf_index) << 7) | (u16::from(self.channel_config) << 3);
        v.to_be_bytes().to_vec()
    }
}

/// The 7-byte ADTS header (no CRC) for a raw AAC access unit of `payload_len` bytes.
/// `profile` is the audio object type minus one (1 = AAC-LC).
pub fn adts_header(
    profile: u8,
    sf_index: u8,
    channel_config: u8,
    payload_len: usize,
) -> Result<[u8; 7]> {
    let len = payload_len + 7;
    if len > 0x1FFF || profile > 3 || sf_index > 12 || channel_config > 7 {
        return Err(Error::InvalidArgument(
            "ADTS cannot carry this frame".into(),
        ));
    }
    Ok([
        0xFF,
        0xF1,
        (profile << 6) | (sf_index << 2) | (channel_config >> 2),
        ((channel_config & 3) << 6) | (len >> 11) as u8,
        (len >> 3) as u8,
        (((len & 7) as u8) << 5) | 0x1F,
        0xFC,
    ])
}

pub(crate) fn sniff(d: &[u8]) -> Option<bool> {
    let h = AdtsHeader::parse(d)?;
    let mut at = h.frame_len;
    for i in 0..2 {
        if at + 7 > d.len() {
            return if i == 0 { Some(false) } else { Some(true) };
        }
        let n = AdtsHeader::parse(&d[at..])?;
        if !n.same_stream(&h) {
            return None;
        }
        at += n.frame_len;
    }
    Some(true)
}

fn profile_name(profile: u8) -> &'static str {
    match profile {
        0 => "AAC Main",
        1 => "AAC-LC",
        2 => "AAC SSR",
        _ => "AAC LTP",
    }
}

const RESYNC_LIMIT: u64 = 1 << 20;
const FULL_SCAN_LIMIT: u64 = 8 << 20;

/// ADTS demuxer. Packets are raw AAC access units (one per raw data block; frames with several
/// blocks are split using the CRC-protected block positions, and rejected when unprotected,
/// since the block boundaries are then only found by decoding).
pub(crate) struct AdtsDemuxer {
    src: Source,
    tracks: Vec<Track>,
    hdr: AdtsHeader,
    end: u64,
    /// Offsets of frames 0, 1, … (contiguous, exact).
    index: Vec<u64>,
    index_end: u64,
    index_complete: bool,
    pos: u64,
    frame_no: u64,
    pending: Vec<Vec<u8>>,
    next_pts: i64,
}

impl AdtsDemuxer {
    pub fn open(mut src: Source, start: u64) -> Result<Self> {
        let end = id3::audio_end(&mut src, start)?;
        src.seek(start);
        let win = src.read_upto(64 * 1024)?;
        let lim = win.len().min(end.saturating_sub(start) as usize);
        let mut first = None;
        for i in 0..lim.saturating_sub(6) {
            if win[i] != 0xFF || sniff(&win[i..lim]).is_none() {
                continue;
            }
            if let Some(h) = AdtsHeader::parse(&win[i..]) {
                first = Some((start + i as u64, h));
                break;
            }
        }
        let (first, hdr) = first.ok_or_else(|| Error::invalid("no ADTS frames found"))?;
        if hdr.blocks > 1 && !hdr.protection {
            return Err(Error::unsupported(
                "ADTS frames with several raw data blocks and no CRC",
            ));
        }
        let rate = AAC_RATES[usize::from(hdr.sf_index)];
        let mut t = Track::new(
            CodecId::Aac,
            rate,
            u32::from(hdr.channel_config),
            profile_name(hdr.profile),
        );
        if hdr.channel_config == 7 {
            t.channels = 8;
        }
        t.config = hdr.asc();
        let mut d = Self {
            src,
            tracks: Vec::new(),
            hdr,
            end,
            index: Vec::new(),
            index_end: first,
            index_complete: false,
            pos: first,
            frame_no: 0,
            pending: Vec::new(),
            next_pts: 0,
        };
        if end - first <= FULL_SCAN_LIMIT {
            d.scan_index(u64::MAX)?;
            t.duration = Some(d.index.len() as u64 * 1024 * hdr.blocks as u64);
        } else {
            // estimate from the average size of the first frames
            d.scan_index(63)?;
            let n = d.index.len() as u64;
            if n > 0 {
                let avg = (d.index_end - first) as f64 / n as f64;
                t.duration = Some(((end - first) as f64 / avg) as u64 * 1024);
            }
        }
        d.tracks.push(t);
        Ok(d)
    }

    fn header_at(&mut self, at: u64) -> Result<Option<AdtsHeader>> {
        if at + 7 > self.end {
            return Ok(None);
        }
        let mut b = [0u8; 7];
        self.src.read_at(at, &mut b)?;
        Ok(AdtsHeader::parse(&b).filter(|h| h.same_stream(&self.hdr)))
    }

    fn resync(&mut self, from: u64) -> Result<Option<u64>> {
        let mut at = from;
        let stop = self.end.min(from.saturating_add(RESYNC_LIMIT));
        while at < stop {
            self.src.seek(at);
            let win = self.src.read_upto(16 * 1024)?;
            if win.len() < 7 {
                return Ok(None);
            }
            for (i, _) in win[..win.len() - 6]
                .iter()
                .enumerate()
                .filter(|(_, &b)| b == 0xFF)
            {
                let p = at + i as u64;
                if let Some(h) = self.header_at(p)? {
                    let next = p + h.frame_len as u64;
                    if next + 7 > self.end || self.header_at(next)?.is_some() {
                        return Ok(Some(p));
                    }
                }
            }
            at += win.len() as u64 - 6;
        }
        Ok(None)
    }

    fn scan_index(&mut self, upto: u64) -> Result<()> {
        while !self.index_complete && self.index.len() as u64 <= upto {
            let at = self.index_end;
            match self.header_at(at)? {
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

    /// Split one ADTS frame into raw data blocks.
    fn split(h: &AdtsHeader, frame: &[u8]) -> Result<Vec<Vec<u8>>> {
        if h.blocks == 1 {
            return Ok(vec![frame[h.header_len().min(frame.len())..].to_vec()]);
        }
        if !h.protection {
            return Err(Error::unsupported(
                "ADTS frame with several raw data blocks and no CRC",
            ));
        }
        if frame.len() < h.header_len() {
            return Err(Error::invalid("short ADTS frame"));
        }
        // raw_data_block_position[i]: offset of block i from the first block (the payload start)
        let mut br = BitReader::new(&frame[7..]);
        let mut pos = vec![0usize];
        for _ in 1..h.blocks {
            pos.push(br.read(16)? as usize);
        }
        let payload = &frame[h.header_len()..];
        let mut out = Vec::new();
        for i in 0..h.blocks {
            let s = pos[i];
            let e = if i + 1 < h.blocks {
                pos[i + 1]
            } else {
                payload.len()
            };
            // every block is followed by a 16-bit CRC
            if s + 2 > e || e > payload.len() {
                return Err(Error::invalid("bad ADTS raw data block positions"));
            }
            out.push(payload[s..e - 2].to_vec());
        }
        Ok(out)
    }
}

impl Demuxer for AdtsDemuxer {
    fn format_name(&self) -> &str {
        "ADTS"
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<DemuxPacket>> {
        loop {
            if !self.pending.is_empty() {
                let data = self.pending.remove(0);
                let pts = self.next_pts;
                self.next_pts += 1024;
                return Ok(Some(DemuxPacket {
                    track: 0,
                    data,
                    pts,
                    duration: 1024,
                    keyframe: true,
                }));
            }
            let at = self.pos;
            let Some(h) = self.header_at(at)? else {
                if at >= self.end {
                    return Ok(None);
                }
                match self.resync(at + 1)? {
                    Some(p) => {
                        self.pos = p;
                        continue;
                    }
                    None => return Ok(None),
                }
            };
            if at + h.frame_len as u64 > self.end {
                return Ok(None);
            }
            self.src.seek(at);
            let frame = self.src.read_vec(h.frame_len as u64)?;
            if at == self.index_end
                && self.frame_no == self.index.len() as u64
                && !self.index_complete
            {
                self.index.push(at);
                self.index_end = at + h.frame_len as u64;
            }
            self.frame_no += 1;
            self.pos = at + h.frame_len as u64;
            // a damaged frame is skipped rather than ending the stream
            self.pending = Self::split(&h, &frame).unwrap_or_default();
            if self.pending.is_empty() {
                self.next_pts += 1024 * h.blocks as i64;
            }
        }
    }

    fn seek(&mut self, track: usize, pts: i64) -> Result<i64> {
        if track != 0 {
            return Err(Error::InvalidArgument(format!("no track {track}")));
        }
        let per = 1024 * self.hdr.blocks as u64;
        let f = pts.max(0) as u64 / per;
        self.scan_index(f)?;
        let f = f.min(self.index.len() as u64);
        self.pos = self
            .index
            .get(f as usize)
            .copied()
            .unwrap_or(self.index_end);
        self.frame_no = f;
        self.pending.clear();
        self.next_pts = (f * per) as i64;
        Ok(self.next_pts)
    }
}

/// Wraps raw AAC access units in ADTS headers (no CRC).
pub struct AdtsWriter<W: Write> {
    w: W,
    profile: u8,
    sf_index: u8,
    channel_config: u8,
}

impl<W: Write> AdtsWriter<W> {
    /// Create a writer from an `AudioSpecificConfig` (audio object types 1..=4 only; HE-AAC
    /// streams are written with their AAC-LC core configuration, which is how ADTS carries them).
    pub fn new(w: W, asc: &[u8]) -> Result<Self> {
        let mut br = BitReader::new(asc);
        let mut aot = br.read(5)?;
        if aot == 31 {
            aot = 32 + br.read(6)?;
        }
        let mut sfi = br.read(4)?;
        if sfi == 15 {
            let rate = br.read(24)?;
            sfi = AAC_RATES
                .iter()
                .position(|&r| r == rate)
                .ok_or_else(|| Error::unsupported("ADTS sample rate"))? as u32;
        }
        let ch = br.read(4)?;
        if aot == 5 || aot == 29 {
            // SBR / PS: the core follows the extension sampling frequency
            let ext = br.read(4)?;
            if ext == 15 {
                br.read(24)?;
            }
            aot = br.read(5)?;
        }
        if !(1..=4).contains(&aot) || sfi > 12 || ch > 7 {
            return Err(Error::unsupported(format!(
                "ADTS cannot carry audio object type {aot}"
            )));
        }
        Ok(Self {
            w,
            profile: (aot - 1) as u8,
            sf_index: sfi as u8,
            channel_config: ch as u8,
        })
    }

    /// Write one raw access unit.
    pub fn write_packet(&mut self, au: &[u8]) -> Result<()> {
        let h = adts_header(self.profile, self.sf_index, self.channel_config, au.len())?;
        self.w.write_all(&h)?;
        self.w.write_all(au)?;
        Ok(())
    }

    /// Flush and return the writer.
    pub fn finish(mut self) -> Result<W> {
        self.w.flush()?;
        Ok(self.w)
    }
}
