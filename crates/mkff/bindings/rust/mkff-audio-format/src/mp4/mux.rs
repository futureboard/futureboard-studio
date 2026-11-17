//! Audio-only MP4 / M4A writing.

use std::io::{Read, Seek, SeekFrom, Write};

use mkff_audio_core::{CodecId, Error, Result};

use super::parse_asc;
use crate::adts::AAC_RATES;
use crate::flac::StreamInfo;
use crate::io::patch;
use crate::pcm::{PcmCoding, PcmFormat};
use crate::xiph::dops_from_opus_head;
use crate::Track;

/// Description of one track for [`M4aWriter::add_track`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M4aTrack {
    /// [`CodecId::Aac`], [`CodecId::Alac`], [`CodecId::Flac`], [`CodecId::Opus`],
    /// [`CodecId::MpegAudio`] or [`CodecId::Pcm`].
    pub codec: CodecId,
    /// Output sample rate (Opus: 48000).
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u32,
    /// The PCM layout (PCM tracks only; any linear integer or float format).
    pub pcm: Option<PcmFormat>,
    /// AAC: `AudioSpecificConfig`; ALAC: the 24-byte `ALACSpecificConfig`; FLAC: the 34-byte
    /// STREAMINFO; Opus: an `OpusHead` packet; MP3 and PCM: empty.
    pub config: Vec<u8>,
    /// Samples of encoder delay to hide (an edit list; for AAC also `iTunSMPB`). Opus: the
    /// pre-skip.
    pub priming: u64,
    /// The real length in samples (after priming), when known: the edit list then also hides
    /// the padding at the end. `None`: everything after the priming plays.
    pub duration: Option<u64>,
    /// Media timescale (default: the sample rate).
    pub timescale: Option<u32>,
}

impl M4aTrack {
    /// A track description with no priming.
    pub fn new(codec: CodecId, sample_rate: u32, channels: u32, config: Vec<u8>) -> Self {
        Self {
            codec,
            sample_rate,
            channels,
            pcm: None,
            config,
            priming: 0,
            duration: None,
            timescale: None,
        }
    }

    /// The description of a demuxed track (for remuxing).
    pub fn from_track(t: &Track) -> Self {
        Self {
            codec: t.codec,
            sample_rate: t.sample_rate,
            channels: t.channels,
            pcm: t.pcm,
            config: t.config.clone(),
            priming: t.priming,
            duration: t.duration,
            timescale: Some(t.time_base),
        }
    }
}

struct TrackState {
    desc: M4aTrack,
    timescale: u32,
    /// Sample sizes (compressed codecs) — PCM tracks use `pcm_bpf` for every frame.
    sizes: Vec<u32>,
    pcm_bpf: u32,
    samples: u64,
    /// (count, delta) runs.
    stts: Vec<(u32, u32)>,
    /// (offset, samples) per chunk.
    chunks: Vec<(u64, u32)>,
    chunk_bytes: u64,
    total_dur: u64,
    bytes: u64,
    max_size: u32,
    window_start: u64,
    window_bytes: u64,
    max_window_bytes: u64,
}

/// An audio-only MP4 (M4A) writer over `Write + Seek`.
///
/// Layout: `ftyp`, then one `mdat` (64-bit size, patched at the end) holding the packets in the
/// order they are written, then `moov` ([`finish`](M4aWriter::finish)); or `moov` before `mdat`
/// with [`finish_faststart`](M4aWriter::finish_faststart) (needs `Read` to move the data).
/// Consecutive packets of a track form chunks of up to 1 MiB. Sample entries: AAC → `mp4a` +
/// `esds` (object type 0x40, buffer size and bit rates computed), MP3 → `mp4a` + `esds` (0x6B, or
/// 0x69 for MPEG-2 rates), ALAC → `alac`, FLAC → `fLaC` + `dfLa`, Opus → `Opus` + `dOps`, PCM →
/// `ipcm` / `fpcm` + `pcmC` (ISO/IEC 23003-5) for little-endian integers and floats, `lpcm`
/// (QuickTime v2 description) otherwise. Priming becomes an edit list (`media_time`), plus an
/// `iTunSMPB` tag for AAC, as Apple tools expect.
pub struct M4aWriter<W: Write + Seek> {
    w: W,
    tracks: Vec<TrackState>,
    mdat_pos: u64,
    started: bool,
    last_track: Option<usize>,
}

const CHUNK_LIMIT: u64 = 1 << 20;

impl<W: Write + Seek> M4aWriter<W> {
    /// Start a file at the writer's position (`ftyp` is chosen from the first track added, so it
    /// is written with the first packet).
    pub fn new(w: W) -> Result<Self> {
        Ok(Self {
            w,
            tracks: Vec::new(),
            mdat_pos: 0,
            started: false,
            last_track: None,
        })
    }

    /// Add a track (before the first packet). Returns its index.
    pub fn add_track(&mut self, desc: M4aTrack) -> Result<usize> {
        if self.started {
            return Err(Error::InvalidArgument(
                "tracks must be added before the first packet".into(),
            ));
        }
        if desc.sample_rate == 0 || desc.channels == 0 {
            return Err(Error::InvalidArgument(
                "track without rate or channels".into(),
            ));
        }
        let mut pcm_bpf = 0;
        match desc.codec {
            CodecId::Aac => {
                parse_asc(&desc.config)?;
            }
            CodecId::Flac => {
                StreamInfo::parse(&desc.config)?;
            }
            CodecId::Opus => {
                dops_from_opus_head(&desc.config)?;
            }
            CodecId::Alac => {
                if desc.config.len() < 24 {
                    return Err(Error::InvalidArgument(
                        "ALAC needs the 24-byte ALACSpecificConfig".into(),
                    ));
                }
            }
            CodecId::MpegAudio => {}
            CodecId::Pcm => {
                let f = desc
                    .pcm
                    .ok_or_else(|| Error::InvalidArgument("PCM track without format".into()))?;
                if !f.is_valid() || f.coding != PcmCoding::Linear {
                    return Err(Error::InvalidArgument(format!(
                        "cannot store PCM {} in MP4",
                        f.label()
                    )));
                }
                pcm_bpf = (f.bytes_per_sample() as u32) * desc.channels;
            }
            c => return Err(Error::unsupported(format!("{} in M4A", c.name()))),
        }
        let timescale = desc.timescale.unwrap_or(if desc.codec == CodecId::Opus {
            48000
        } else {
            desc.sample_rate
        });
        self.tracks.push(TrackState {
            desc,
            timescale,
            sizes: Vec::new(),
            pcm_bpf,
            samples: 0,
            stts: Vec::new(),
            chunks: Vec::new(),
            chunk_bytes: 0,
            total_dur: 0,
            bytes: 0,
            max_size: 0,
            window_start: 0,
            window_bytes: 0,
            max_window_bytes: 0,
        });
        Ok(self.tracks.len() - 1)
    }

    fn start(&mut self) -> Result<()> {
        if self.tracks.is_empty() {
            return Err(Error::InvalidArgument("no tracks".into()));
        }
        let m4a = self.tracks.iter().all(|t| {
            matches!(
                t.desc.codec,
                CodecId::Aac | CodecId::Alac | CodecId::MpegAudio
            )
        });
        let mut f = Vec::new();
        f.extend_from_slice(&28u32.to_be_bytes());
        f.extend_from_slice(b"ftyp");
        f.extend_from_slice(if m4a { b"M4A " } else { b"isom" });
        f.extend_from_slice(&0x200u32.to_be_bytes());
        f.extend_from_slice(if m4a { b"M4A " } else { b"isom" });
        f.extend_from_slice(b"iso2mp42");
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(b"mdat");
        f.extend_from_slice(&0u64.to_be_bytes());
        self.mdat_pos = self.w.stream_position()? + 28;
        self.w.write_all(&f)?;
        self.started = true;
        Ok(())
    }

    /// Set the real length of `track` in samples after the priming (e.g. once an encoder has been
    /// flushed and the input length is known), so the edit list also hides the end padding.
    /// Overrides [`M4aTrack::duration`]; call before `finish`.
    pub fn set_duration(&mut self, track: usize, samples: Option<u64>) -> Result<()> {
        let t = self.tracks.get_mut(track).ok_or_else(|| Error::InvalidArgument(format!("no track {track}")))?;
        t.desc.duration = samples;
        Ok(())
    }

    /// Append one packet of `track`: a compressed access unit with its duration in the track's
    /// timescale, or for PCM interleaved frames (`duration` = frame count, checked against the
    /// data length).
    pub fn write_packet(&mut self, track: usize, data: &[u8], duration: u32) -> Result<()> {
        if !self.started {
            self.start()?;
        }
        let pos = self.w.stream_position()?;
        let continues = self.last_track == Some(track);
        let t = self
            .tracks
            .get_mut(track)
            .ok_or_else(|| Error::InvalidArgument(format!("no track {track}")))?;
        let (n, dur_each) = if t.pcm_bpf > 0 {
            if !(data.len() as u64).is_multiple_of(u64::from(t.pcm_bpf)) {
                return Err(Error::InvalidArgument("partial PCM frame".into()));
            }
            ((data.len() / t.pcm_bpf as usize) as u32, 1)
        } else {
            if data.len() > u32::MAX as usize {
                return Err(Error::InvalidArgument("packet too large".into()));
            }
            t.sizes.push(data.len() as u32);
            t.max_size = t.max_size.max(data.len() as u32);
            (1, duration)
        };
        if n == 0 {
            return Ok(());
        }
        match t.stts.last_mut() {
            Some((c, d)) if *d == dur_each && *c <= u32::MAX - n => *c += n,
            _ => t.stts.push((n, dur_each)),
        }
        let new_chunk =
            !continues || t.chunk_bytes + data.len() as u64 > CHUNK_LIMIT || t.chunks.is_empty();
        if new_chunk {
            t.chunks.push((pos, 0));
            t.chunk_bytes = 0;
        }
        if let Some(c) = t.chunks.last_mut() {
            c.1 += n;
        }
        t.chunk_bytes += data.len() as u64;
        t.samples += u64::from(n);
        let dur = u64::from(n) * u64::from(dur_each);
        // peak bit rate over one-second windows
        if t.total_dur >= t.window_start + u64::from(t.timescale) {
            t.window_start = t.total_dur;
            t.window_bytes = 0;
        }
        t.window_bytes += data.len() as u64;
        t.max_window_bytes = t.max_window_bytes.max(t.window_bytes);
        t.total_dur += dur;
        t.bytes += data.len() as u64;
        self.w.write_all(data)?;
        self.last_track = Some(track);
        Ok(())
    }

    fn build_moov(&self, shift: u64) -> Result<Vec<u8>> {
        let movie_ts = self.tracks.first().map_or(1000, |t| t.timescale);
        let mut traks = Vec::new();
        let mut movie_dur = 0u64;
        for (i, t) in self.tracks.iter().enumerate() {
            let (trak, d) = self.build_trak(t, i as u32 + 1, movie_ts, shift)?;
            movie_dur = movie_dur.max(d);
            traks.extend_from_slice(&trak);
        }
        let mut mvhd = Vec::new();
        let v1 = movie_dur > u64::from(u32::MAX);
        if v1 {
            mvhd.extend_from_slice(&0x0100_0000u32.to_be_bytes());
            mvhd.extend_from_slice(&[0; 16]);
            mvhd.extend_from_slice(&movie_ts.to_be_bytes());
            mvhd.extend_from_slice(&movie_dur.to_be_bytes());
        } else {
            mvhd.extend_from_slice(&[0; 12]);
            mvhd.extend_from_slice(&movie_ts.to_be_bytes());
            mvhd.extend_from_slice(&(movie_dur as u32).to_be_bytes());
        }
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
        mvhd.extend_from_slice(&[0; 10]);
        mvhd.extend_from_slice(&MATRIX);
        mvhd.extend_from_slice(&[0; 24]);
        mvhd.extend_from_slice(&(self.tracks.len() as u32 + 1).to_be_bytes());
        let mut moov = boxed(b"mvhd", &mvhd);
        moov.extend_from_slice(&traks);
        if let Some(t) = self
            .tracks
            .iter()
            .find(|t| t.desc.codec == CodecId::Aac && t.desc.priming > 0)
        {
            moov.extend_from_slice(&itunsmpb_udta(t));
        }
        Ok(boxed(b"moov", &moov))
    }

    fn build_trak(
        &self,
        t: &TrackState,
        id: u32,
        movie_ts: u32,
        shift: u64,
    ) -> Result<(Vec<u8>, u64)> {
        let ts = u64::from(t.timescale);
        let rate = u64::from(t.desc.sample_rate);
        let to_media = |samples: u64| {
            if ts == rate {
                samples
            } else {
                samples * ts / rate.max(1)
            }
        };
        let priming = to_media(t.desc.priming).min(t.total_dur);
        let play = match t.desc.duration {
            Some(d) => to_media(d).min(t.total_dur - priming),
            None => t.total_dur - priming,
        };
        let to_movie =
            |v: u64| (u128::from(v) * u128::from(movie_ts) / u128::from(ts.max(1))) as u64;
        let movie_dur = to_movie(play);
        let mut trak = Vec::new();
        // tkhd
        let mut tkhd = Vec::new();
        let v1 = movie_dur > u64::from(u32::MAX);
        if v1 {
            tkhd.extend_from_slice(&0x0100_0003u32.to_be_bytes());
            tkhd.extend_from_slice(&[0; 16]);
            tkhd.extend_from_slice(&id.to_be_bytes());
            tkhd.extend_from_slice(&[0; 4]);
            tkhd.extend_from_slice(&movie_dur.to_be_bytes());
        } else {
            tkhd.extend_from_slice(&3u32.to_be_bytes());
            tkhd.extend_from_slice(&[0; 8]);
            tkhd.extend_from_slice(&id.to_be_bytes());
            tkhd.extend_from_slice(&[0; 4]);
            tkhd.extend_from_slice(&(movie_dur as u32).to_be_bytes());
        }
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&[0, 0, 0, 1]); // layer 0, alternate group 1
        tkhd.extend_from_slice(&[0x01, 0x00, 0, 0]); // volume 1.0
        tkhd.extend_from_slice(&MATRIX);
        tkhd.extend_from_slice(&[0; 8]);
        trak.extend_from_slice(&boxed(b"tkhd", &tkhd));
        // edit list
        if priming > 0 || play < t.total_dur {
            let mut elst = Vec::new();
            let big = movie_dur > u64::from(u32::MAX) || priming > i32::MAX as u64;
            elst.extend_from_slice(&(if big { 0x0100_0000u32 } else { 0 }).to_be_bytes());
            elst.extend_from_slice(&1u32.to_be_bytes());
            if big {
                elst.extend_from_slice(&movie_dur.to_be_bytes());
                elst.extend_from_slice(&priming.to_be_bytes());
            } else {
                elst.extend_from_slice(&(movie_dur as u32).to_be_bytes());
                elst.extend_from_slice(&(priming as u32).to_be_bytes());
            }
            elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            trak.extend_from_slice(&boxed(b"edts", &boxed(b"elst", &elst)));
        }
        // mdia
        let mut mdhd = Vec::new();
        if t.total_dur > u64::from(u32::MAX) {
            mdhd.extend_from_slice(&0x0100_0000u32.to_be_bytes());
            mdhd.extend_from_slice(&[0; 16]);
            mdhd.extend_from_slice(&t.timescale.to_be_bytes());
            mdhd.extend_from_slice(&t.total_dur.to_be_bytes());
        } else {
            mdhd.extend_from_slice(&[0; 12]);
            mdhd.extend_from_slice(&t.timescale.to_be_bytes());
            mdhd.extend_from_slice(&(t.total_dur as u32).to_be_bytes());
        }
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // "und"
        mdhd.extend_from_slice(&[0; 2]);
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"soun");
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.extend_from_slice(b"SoundHandler\0");
        let smhd = boxed(b"smhd", &[0; 8]);
        let dref = {
            let mut d = vec![0, 0, 0, 0, 0, 0, 0, 1];
            d.extend_from_slice(&boxed(b"url ", &[0, 0, 0, 1]));
            boxed(b"dinf", &boxed(b"dref", &d))
        };
        let stbl = self.build_stbl(t, shift)?;
        let mut minf = smhd;
        minf.extend_from_slice(&dref);
        minf.extend_from_slice(&stbl);
        let mut mdia = boxed(b"mdhd", &mdhd);
        mdia.extend_from_slice(&boxed(b"hdlr", &hdlr));
        mdia.extend_from_slice(&boxed(b"minf", &minf));
        trak.extend_from_slice(&boxed(b"mdia", &mdia));
        Ok((boxed(b"trak", &trak), movie_dur))
    }

    fn build_stbl(&self, t: &TrackState, shift: u64) -> Result<Vec<u8>> {
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&sample_entry(t)?);
        let mut stbl = boxed(b"stsd", &stsd);
        let mut stts = vec![0; 4];
        stts.extend_from_slice(&(t.stts.len() as u32).to_be_bytes());
        for (c, d) in &t.stts {
            stts.extend_from_slice(&c.to_be_bytes());
            stts.extend_from_slice(&d.to_be_bytes());
        }
        stbl.extend_from_slice(&boxed(b"stts", &stts));
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (i, (_, n)) in t.chunks.iter().enumerate() {
            if runs.last().is_none_or(|r| r.1 != *n) {
                runs.push((i as u32 + 1, *n));
            }
        }
        let mut stsc = vec![0; 4];
        stsc.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (f, n) in runs {
            stsc.extend_from_slice(&f.to_be_bytes());
            stsc.extend_from_slice(&n.to_be_bytes());
            stsc.extend_from_slice(&1u32.to_be_bytes());
        }
        stbl.extend_from_slice(&boxed(b"stsc", &stsc));
        let mut stsz = vec![0; 4];
        if t.pcm_bpf > 0 {
            stsz.extend_from_slice(&t.pcm_bpf.to_be_bytes());
            stsz.extend_from_slice(&(t.samples.min(u64::from(u32::MAX)) as u32).to_be_bytes());
        } else if !t.sizes.is_empty() && t.sizes.iter().all(|&s| s == t.sizes[0]) {
            stsz.extend_from_slice(&t.sizes[0].to_be_bytes());
            stsz.extend_from_slice(&(t.sizes.len() as u32).to_be_bytes());
        } else {
            stsz.extend_from_slice(&0u32.to_be_bytes());
            stsz.extend_from_slice(&(t.sizes.len() as u32).to_be_bytes());
            for s in &t.sizes {
                stsz.extend_from_slice(&s.to_be_bytes());
            }
        }
        stbl.extend_from_slice(&boxed(b"stsz", &stsz));
        let large = t
            .chunks
            .last()
            .is_some_and(|c| c.0 + shift > u64::from(u32::MAX));
        let mut co = vec![0; 4];
        co.extend_from_slice(&(t.chunks.len() as u32).to_be_bytes());
        for (off, _) in &t.chunks {
            if large {
                co.extend_from_slice(&(off + shift).to_be_bytes());
            } else {
                co.extend_from_slice(&((off + shift) as u32).to_be_bytes());
            }
        }
        stbl.extend_from_slice(&boxed(if large { b"co64" } else { b"stco" }, &co));
        Ok(boxed(b"stbl", &stbl))
    }

    fn patch_mdat(&mut self, end: u64) -> Result<()> {
        let size = end - (self.mdat_pos);
        patch(&mut self.w, self.mdat_pos + 8, &size.to_be_bytes())
    }

    /// Write `moov` after the media data and return the writer positioned at the end.
    pub fn finish(mut self) -> Result<W> {
        if !self.started {
            self.start()?;
        }
        let end = self.w.stream_position()?;
        self.patch_mdat(end)?;
        let moov = self.build_moov(0)?;
        self.w.seek(SeekFrom::Start(end))?;
        self.w.write_all(&moov)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

impl<W: Read + Write + Seek> M4aWriter<W> {
    /// Like [`finish`](M4aWriter::finish), but put `moov` before `mdat` (progressive download),
    /// moving the media data forward by the size of `moov`.
    pub fn finish_faststart(mut self) -> Result<W> {
        if !self.started {
            self.start()?;
        }
        let end = self.w.stream_position()?;
        self.patch_mdat(end)?;
        let mut moov_len = self.build_moov(0)?.len() as u64;
        let moov = loop {
            let m = self.build_moov(moov_len)?;
            if m.len() as u64 == moov_len {
                break m;
            }
            moov_len = m.len() as u64;
        };
        // move [mdat_pos, end) forward by moov_len, last block first
        let start = self.mdat_pos;
        let mut buf = vec![0u8; 1 << 20];
        let mut at = end;
        while at > start {
            let n = (at - start).min(buf.len() as u64) as usize;
            at -= n as u64;
            self.w.seek(SeekFrom::Start(at))?;
            self.w.read_exact(&mut buf[..n])?;
            self.w.seek(SeekFrom::Start(at + moov_len))?;
            self.w.write_all(&buf[..n])?;
        }
        self.w.seek(SeekFrom::Start(start))?;
        self.w.write_all(&moov)?;
        self.w.seek(SeekFrom::Start(end + moov_len))?;
        self.w.flush()?;
        Ok(self.w)
    }
}

const MATRIX: [u8; 36] = [
    0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0x40, 0, 0, 0,
];

fn boxed(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 16);
    let size = body.len() as u64 + 8;
    if size > u64::from(u32::MAX) {
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(t);
        b.extend_from_slice(&(size + 8).to_be_bytes());
    } else {
        b.extend_from_slice(&(size as u32).to_be_bytes());
        b.extend_from_slice(t);
    }
    b.extend_from_slice(body);
    b
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut d = vec![tag];
    let n = body.len();
    if n < 0x80 {
        d.push(n as u8);
    } else {
        d.extend_from_slice(&[
            0x80 | (n >> 21) as u8 & 0x7F,
            0x80 | (n >> 14) as u8 & 0x7F,
            0x80 | (n >> 7) as u8 & 0x7F,
            n as u8 & 0x7F,
        ]);
    }
    d.extend_from_slice(body);
    d
}

fn esds(t: &TrackState, oti: u8, dsi: &[u8]) -> Vec<u8> {
    let secs = t.total_dur as f64 / f64::from(t.timescale.max(1));
    let avg = if secs > 0.0 {
        (t.bytes as f64 * 8.0 / secs) as u32
    } else {
        0
    };
    let max = ((t.max_window_bytes * 8).min(u64::from(u32::MAX)) as u32).max(avg);
    let mut dc = vec![oti, 0x15];
    dc.extend_from_slice(&t.max_size.min(0xFF_FFFF).to_be_bytes()[1..]);
    dc.extend_from_slice(&max.to_be_bytes());
    dc.extend_from_slice(&avg.to_be_bytes());
    if !dsi.is_empty() {
        dc.extend_from_slice(&descriptor(0x05, dsi));
    }
    let mut es = vec![0, 0, 0];
    es.extend_from_slice(&descriptor(0x04, &dc));
    es.extend_from_slice(&descriptor(0x06, &[0x02]));
    let mut b = vec![0; 4];
    b.extend_from_slice(&descriptor(0x03, &es));
    boxed(b"esds", &b)
}

/// The common part of an audio sample entry (version 0).
fn audio_entry_head(channels: u32, bits: u32, rate: u32) -> Vec<u8> {
    let mut e = vec![0, 0, 0, 0, 0, 0, 0, 1];
    e.extend_from_slice(&[0; 8]);
    e.extend_from_slice(&(channels.min(0xFFFF) as u16).to_be_bytes());
    e.extend_from_slice(&(bits as u16).to_be_bytes());
    e.extend_from_slice(&[0; 4]);
    let r = if rate > 0xFFFF { 0 } else { rate << 16 };
    e.extend_from_slice(&r.to_be_bytes());
    e
}

fn sample_entry(t: &TrackState) -> Result<Vec<u8>> {
    let d = &t.desc;
    Ok(match d.codec {
        CodecId::Aac => {
            let info = parse_asc(&d.config)?;
            let rate = if AAC_RATES.contains(&d.sample_rate) {
                d.sample_rate
            } else {
                info.rate
            };
            let mut e = audio_entry_head(d.channels, 16, rate);
            e.extend_from_slice(&esds(t, 0x40, &d.config));
            boxed(b"mp4a", &e)
        }
        CodecId::MpegAudio => {
            let oti = if d.sample_rate >= 32000 { 0x6B } else { 0x69 };
            let mut e = audio_entry_head(d.channels, 16, d.sample_rate);
            e.extend_from_slice(&esds(t, oti, &[]));
            boxed(b"mp4a", &e)
        }
        CodecId::Alac => {
            let bits = u32::from(d.config[5]);
            let mut e = audio_entry_head(d.channels, bits, d.sample_rate);
            let mut a = vec![0; 4];
            a.extend_from_slice(&d.config[..24]);
            e.extend_from_slice(&boxed(b"alac", &a));
            boxed(b"alac", &e)
        }
        CodecId::Flac => {
            let si = StreamInfo::parse(&d.config)?;
            let mut e = audio_entry_head(d.channels, si.bps, d.sample_rate);
            let mut b = vec![0, 0, 0, 0, 0x80, 0, 0, 34];
            b.extend_from_slice(&d.config[..34]);
            e.extend_from_slice(&boxed(b"dfLa", &b));
            boxed(b"fLaC", &e)
        }
        CodecId::Opus => {
            let mut e = audio_entry_head(d.channels, 16, 48000);
            e.extend_from_slice(&boxed(b"dOps", &dops_from_opus_head(&d.config)?));
            boxed(b"Opus", &e)
        }
        CodecId::Pcm => {
            let f = d
                .pcm
                .ok_or_else(|| Error::InvalidArgument("PCM without format".into()))?;
            if !f.big_endian || f.bits == 8 {
                let mut e = audio_entry_head(d.channels, f.bits, d.sample_rate);
                let flags = if f.big_endian && f.bits > 8 { 0 } else { 1 };
                e.extend_from_slice(&boxed(b"pcmC", &[0, 0, 0, 0, flags, f.bits as u8]));
                boxed(if f.float { b"fpcm" } else { b"ipcm" }, &e)
            } else {
                // QuickTime v2 sound description
                let mut e = vec![0, 0, 0, 0, 0, 0, 0, 1];
                e.extend_from_slice(&2u16.to_be_bytes());
                e.extend_from_slice(&[0; 6]);
                e.extend_from_slice(&3u16.to_be_bytes());
                e.extend_from_slice(&16u16.to_be_bytes());
                e.extend_from_slice(&0xFFFEu16.to_be_bytes());
                e.extend_from_slice(&0u16.to_be_bytes());
                e.extend_from_slice(&0x0001_0000u32.to_be_bytes());
                e.extend_from_slice(&72u32.to_be_bytes());
                e.extend_from_slice(&f64::from(d.sample_rate).to_bits().to_be_bytes());
                e.extend_from_slice(&d.channels.to_be_bytes());
                e.extend_from_slice(&0x7F00_0000u32.to_be_bytes());
                e.extend_from_slice(&f.bits.to_be_bytes());
                let flags = if f.float { 1 } else { 4 } | 2 | 8;
                e.extend_from_slice(&(flags as u32).to_be_bytes());
                e.extend_from_slice(&t.pcm_bpf.to_be_bytes());
                e.extend_from_slice(&1u32.to_be_bytes());
                boxed(b"lpcm", &e)
            }
        }
        c => return Err(Error::unsupported(format!("{} in M4A", c.name()))),
    })
}

fn itunsmpb_udta(t: &TrackState) -> Vec<u8> {
    let total = t.total_dur;
    let ts = u64::from(t.timescale);
    let rate = u64::from(t.desc.sample_rate);
    let samples = if ts == rate {
        total
    } else {
        total * rate / ts.max(1)
    };
    let priming = t.desc.priming.min(samples);
    let len = t
        .desc
        .duration
        .unwrap_or(samples - priming)
        .min(samples - priming);
    let padding = samples - priming - len;
    let mut s = format!(" 00000000 {priming:08X} {padding:08X} {len:016X}");
    for _ in 0..8 {
        s.push_str(" 00000000");
    }
    let mut mean = vec![0; 4];
    mean.extend_from_slice(b"com.apple.iTunes");
    let mut name = vec![0; 4];
    name.extend_from_slice(b"iTunSMPB");
    let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0];
    data.extend_from_slice(s.as_bytes());
    let mut item = boxed(b"mean", &mean);
    item.extend_from_slice(&boxed(b"name", &name));
    item.extend_from_slice(&boxed(b"data", &data));
    let ilst = boxed(b"ilst", &boxed(b"----", &item));
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdirappl");
    hdlr.extend_from_slice(&[0; 9]);
    let mut meta = vec![0; 4];
    meta.extend_from_slice(&boxed(b"hdlr", &hdlr));
    meta.extend_from_slice(&ilst);
    boxed(b"udta", &boxed(b"meta", &meta))
}
