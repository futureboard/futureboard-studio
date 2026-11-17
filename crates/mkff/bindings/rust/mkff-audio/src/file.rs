//! [`AudioFile`]: one audio track of a file, decoded, trimmed and seekable.

use std::path::Path;

use mkff_audio_core::resample::Resampler;
use mkff_audio_core::{AudioBuffer, CodecId, Decoder, Error, Result};
use mkff_audio_format::{Demuxer, ReadSeek, Track};

use crate::codecs;

/// Decoded samples kept behind the read position for overlapping / slightly backwards reads.
const KEEP_BEHIND: usize = 1 << 16;
/// Edge margin (input samples) when resampling a random-access range.
const RESAMPLE_MARGIN: i64 = 512;

/// Samples to decode and discard before a seek target so the decoder's state (overlap, bit
/// reservoir, predictor) has settled.
fn preroll(codec: CodecId, rate: u32) -> i64 {
    match codec {
        // three frames: the reservoir reaches back up to 511 bytes, plus the MDCT overlap
        CodecId::MpegAudio => 3 * 1152,
        CodecId::Aac => 2 * 1024,
        CodecId::Ac3 => 1536,
        // RFC 7845 §4.6: at least 80 ms
        CodecId::Opus => i64::from(rate.max(8000)) * 80 / 1000,
        // the decoder already withholds the first packet after a reset
        CodecId::Vorbis => 0,
        CodecId::Pcm | CodecId::Flac | CodecId::Alac => 0,
    }
}

/// One audio track of a file (or any [`ReadSeek`]), decoded with MKFF's own codecs.
///
/// Sample positions are *presentation* samples at [`AudioFile::sample_rate`]: encoder priming is
/// already removed and end padding is cut, so position 0 is the first real sample and the file is
/// [`AudioFile::duration`] samples long — gapless.
///
/// ```no_run
/// let mut f = mkff_audio::AudioFile::open("song.mp3")?;
/// println!("{} Hz × {}, {:?} samples", f.sample_rate(), f.channels(), f.duration());
/// while let Some(chunk) = f.next_chunk()? {
///     // chunk.channels: planar f32
/// }
/// f.seek(44_100 * 30)?; // 0:30, sample-exact
/// let window = f.read(0, 1024, 48_000)?; // random access, resampled
/// # Ok::<(), mkff_audio::Error>(())
/// ```
pub struct AudioFile {
    demux: Box<dyn Demuxer>,
    track_index: usize,
    track: Track,
    decoder: Box<dyn Decoder>,
    rate: u32,
    channels: usize,
    /// Priming in output samples.
    priming: i64,
    /// End of the real audio (presentation samples), when known.
    end: Option<i64>,
    /// Decoded presentation samples `[cache_start, cache_start + cache[0].len())`.
    cache: Vec<Vec<f32>>,
    cache_start: i64,
    /// Next position `next_chunk` returns from.
    cursor: i64,
    eof: bool,
    /// Skip decode errors until the first good packet (right after a seek).
    tolerant: bool,
}

impl AudioFile {
    /// Open the first audio track of a file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_track(path, 0)
    }

    /// Open audio track `index` (an index into the container's audio tracks).
    pub fn open_track(path: impl AsRef<Path>, index: usize) -> Result<Self> {
        let demux = mkff_audio_format::open_path(path)?;
        Self::from_demuxer(demux, index)
    }

    /// Open from any reader; `name_hint` (a file name) breaks ties between formats.
    pub fn from_reader(reader: Box<dyn ReadSeek>, name_hint: &str, index: usize) -> Result<Self> {
        let demux = mkff_audio_format::open(reader, name_hint)?;
        Self::from_demuxer(demux, index)
    }

    /// Open from bytes in memory.
    pub fn from_bytes(bytes: Vec<u8>, name_hint: &str) -> Result<Self> {
        Self::from_reader(Box::new(std::io::Cursor::new(bytes)), name_hint, 0)
    }

    /// Decode track `index` of an opened demuxer.
    pub fn from_demuxer(demux: Box<dyn Demuxer>, index: usize) -> Result<Self> {
        let track = demux
            .tracks()
            .get(index)
            .cloned()
            .ok_or_else(|| Error::InvalidArgument(format!("no audio track {index}")))?;
        let decoder = codecs::decoder_for(&track)?;
        // the decoder knows its output; fall back on what the container says until it does
        let rate = match decoder.sample_rate() {
            0 => track.sample_rate,
            r => r,
        };
        let channels = match decoder.channels() {
            0 => track.channels as usize,
            c => c,
        };
        let mut f = AudioFile {
            demux,
            track_index: index,
            track,
            decoder,
            rate,
            channels,
            priming: 0,
            end: None,
            cache: Vec::new(),
            cache_start: 0,
            cursor: 0,
            eof: false,
            tolerant: false,
        };
        f.update_timing();
        // learn the real output layout from the first decoded audio, then start over
        if f.decoder.sample_rate() == 0
            || f.decoder.channels() == 0
            || f.rate == 0
            || f.channels == 0
        {
            f.fill_to(1)?;
            f.update_timing();
        }
        f.cache.resize(f.channels, Vec::new());
        Ok(f)
    }

    fn update_timing(&mut self) {
        let t = &self.track;
        // priming / padding are counted at the container's sample rate; the decoder may run at
        // another (HE-AAC core decoders upsample, ADTS reports the core rate)
        self.priming = if t.sample_rate == 0 || t.sample_rate == self.rate {
            t.priming as i64
        } else {
            (u128::from(t.priming) * u128::from(self.rate) / u128::from(t.sample_rate)) as i64
        };
        self.end = t.duration.map(|d| {
            let tb = u128::from(t.time_base.max(1));
            (u128::from(d) * u128::from(self.rate.max(1)) / tb) as i64
        });
    }

    /// The container's description of the track.
    pub fn track(&self) -> &Track {
        &self.track
    }

    /// Container format name ("MP3", "MP4", "Ogg", …).
    pub fn format_name(&self) -> &str {
        self.demux.format_name()
    }

    pub fn codec(&self) -> CodecId {
        self.track.codec
    }

    /// Output sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Length in presentation samples, when the container knows it.
    pub fn duration(&self) -> Option<u64> {
        self.end.map(|e| e.max(0) as u64)
    }

    /// Length in seconds, when known.
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration()
            .map(|d| d as f64 / f64::from(self.rate.max(1)))
    }

    /// Presentation position the next [`Self::next_chunk`] starts at.
    pub fn position(&self) -> u64 {
        self.cursor.max(0) as u64
    }

    fn samples_to_pts(&self, samples: i64) -> i64 {
        let tb = i64::from(self.track.time_base.max(1));
        let r = i64::from(self.rate.max(1));
        if tb == r {
            samples
        } else {
            (i128::from(samples) * i128::from(tb) / i128::from(r)) as i64
        }
    }

    fn pts_to_samples(&self, pts: i64) -> i64 {
        let tb = i64::from(self.track.time_base.max(1));
        let r = i64::from(self.rate.max(1));
        if tb == r {
            pts
        } else {
            (i128::from(pts) * i128::from(r) / i128::from(tb)) as i64
        }
    }

    fn cache_end(&self) -> i64 {
        self.cache_start + self.cache.first().map_or(0, Vec::len) as i64
    }

    /// Decode packets until the cache reaches `target` (presentation samples) or the stream ends.
    fn fill_to(&mut self, target: i64) -> Result<()> {
        while !self.eof && self.cache_end() < target {
            let Some(pkt) = self.demux.next_packet()? else {
                self.eof = true;
                break;
            };
            if pkt.track != self.track_index {
                continue;
            }
            let buf = match self.decoder.decode(&pkt.data) {
                Ok(b) => b,
                Err(e) if self.tolerant => {
                    log::debug!(
                        "{}: skipping undecodable packet after seek: {e}",
                        self.track.name
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };
            if buf.is_empty() {
                continue;
            }
            self.tolerant = false;
            // the decoder is the authority on its output (ADTS cannot signal HE-AAC, a WAV
            // header can lie); adopt it while nothing has been decoded yet
            let fresh = self.cache.first().is_none_or(Vec::is_empty) && self.cache_start == 0;
            if buf.sample_rate != 0 && buf.sample_rate != self.rate && (fresh || self.rate == 0) {
                self.rate = buf.sample_rate;
                self.update_timing();
            }
            if buf.channel_count() != 0
                && buf.channel_count() != self.channels
                && (fresh || self.channels == 0)
            {
                self.channels = buf.channel_count();
                self.cache = vec![Vec::new(); self.channels];
            }
            let start = self.pts_to_samples(pkt.pts) - self.priming;
            self.append(start, buf);
        }
        Ok(())
    }

    /// Put decoded samples starting at presentation sample `start` into the cache.
    fn append(&mut self, start: i64, mut buf: AudioBuffer) {
        let ch = self.channels.max(1);
        if self.cache.len() != ch {
            self.cache = vec![Vec::new(); ch];
        }
        // channel count changes mid-stream: keep the layout we opened with
        buf.channels.resize(ch, vec![0.0; buf.frames()]);
        let mut start = start;
        let mut n = buf.frames() as i64;
        // trim priming (negative positions) and end padding
        if start < 0 {
            let cut = (-start).min(n);
            buf.skip_front(cut as usize);
            start += cut;
            n -= cut;
        }
        if let Some(end) = self.end {
            if start + n > end {
                n = (end - start).max(0);
                buf.truncate(n as usize);
            }
        }
        if n <= 0 {
            return;
        }
        let cache_end = self.cache_end();
        if self.cache[0].is_empty() {
            self.cache_start = start;
        } else if start > cache_end {
            // a gap (damaged packets): fill with silence
            let gap = (start - cache_end) as usize;
            for c in &mut self.cache {
                c.extend(std::iter::repeat_n(0.0, gap));
            }
        } else if start < cache_end {
            // overlap: keep what we have
            let skip = ((cache_end - start) as usize).min(n as usize);
            buf.skip_front(skip);
        }
        for (c, src) in self.cache.iter_mut().zip(&buf.channels) {
            c.extend_from_slice(src);
        }
    }

    /// Drop cached samples well behind `pos`.
    fn trim_cache(&mut self, pos: i64) {
        let drop = (pos - self.cache_start - KEEP_BEHIND as i64).max(0) as usize;
        let drop = drop.min(self.cache.first().map_or(0, Vec::len));
        if drop > 0 {
            for c in &mut self.cache {
                c.drain(..drop);
            }
            self.cache_start += drop as i64;
        }
    }

    /// The next decoded samples from [`Self::position`] on (`None` at the end).
    pub fn next_chunk(&mut self) -> Result<Option<AudioBuffer>> {
        if self.cursor >= self.cache_end() {
            self.fill_to(self.cursor + 1)?;
        }
        let end = self.cache_end();
        if self.cursor >= end {
            return Ok(None);
        }
        let from = (self.cursor - self.cache_start).max(0) as usize;
        let buf = AudioBuffer {
            sample_rate: self.rate,
            channels: self.cache.iter().map(|c| c[from..].to_vec()).collect(),
        };
        self.cursor = end;
        self.trim_cache(self.cursor);
        Ok(Some(buf))
    }

    /// Decode everything from the current position to the end.
    pub fn read_to_end(&mut self) -> Result<AudioBuffer> {
        let mut out = AudioBuffer::empty(self.rate, self.channels);
        while let Some(c) = self.next_chunk()? {
            out.append(&c);
        }
        out.sample_rate = self.rate;
        Ok(out)
    }

    /// Continue from presentation sample `pos` (sample-exact).
    pub fn seek(&mut self, pos: u64) -> Result<()> {
        let pos = pos as i64;
        if pos >= self.cache_start && pos <= self.cache_end() {
            self.cursor = pos;
            return Ok(());
        }
        let pre = preroll(self.track.codec, self.rate);
        let target = (pos + self.priming - pre).max(0);
        let landed = self
            .demux
            .seek(self.track_index, self.samples_to_pts(target))?;
        self.decoder.reset();
        self.cache = vec![Vec::new(); self.channels.max(1)];
        self.cache_start = self.pts_to_samples(landed) - self.priming;
        self.eof = false;
        self.tolerant = pre > 0;
        self.cursor = pos;
        // samples decoded during the pre-roll are cached and then trimmed away
        self.fill_to(pos)?;
        Ok(())
    }

    /// `frames` samples from presentation sample `start` at `sample_rate` (0: the file's own
    /// rate). Positions before 0 or past the end read as silence. Reading forward sequentially is
    /// as fast as streaming; anything else seeks.
    pub fn read(&mut self, start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer> {
        let out_rate = if sample_rate == 0 {
            self.rate
        } else {
            sample_rate
        };
        if out_rate == self.rate {
            return self.read_native(start, frames);
        }
        // the input range covering the output range, with margins for the filter
        let r =
            |x: i64, from: u32, to: u32| (i128::from(x) * i128::from(to) / i128::from(from)) as i64;
        let in_start = r(start, out_rate, self.rate) - RESAMPLE_MARGIN;
        let in_len = r(frames as i64, out_rate, self.rate) + 2 * RESAMPLE_MARGIN + 2;
        let src = self.read_native(in_start, in_len.max(0) as usize)?;
        let mut rs = Resampler::new(self.rate, out_rate, self.channels.max(1));
        let mut out = rs.process(&src.channels);
        out.append(&rs.flush());
        // output sample k of `out` is at output position in_start × out/in + k
        let first = r(in_start, self.rate, out_rate);
        let skip = (start - first).max(0) as usize;
        let mut chans = Vec::with_capacity(out.channels.len());
        for c in out.channels {
            let mut v: Vec<f32> = c.into_iter().skip(skip).take(frames).collect();
            v.resize(frames, 0.0);
            chans.push(v);
        }
        Ok(AudioBuffer {
            sample_rate: out_rate,
            channels: chans,
        })
    }

    fn read_native(&mut self, start: i64, frames: usize) -> Result<AudioBuffer> {
        let ch = self.channels.max(1);
        let mut out = AudioBuffer::new(self.rate, ch, frames);
        let end = start + frames as i64;
        let lo = start.max(0);
        let hi = match self.end {
            Some(e) => end.min(e),
            None => end,
        };
        if hi <= lo {
            return Ok(out);
        }
        if lo < self.cache_start || lo > self.cache_end() + i64::from(self.rate) {
            // far away (or behind what we keep): seek
            self.seek(lo as u64)?;
        }
        self.fill_to(hi)?;
        let have_lo = lo.max(self.cache_start);
        let have_hi = hi.min(self.cache_end());
        if have_hi > have_lo {
            let src = (have_lo - self.cache_start) as usize;
            let dst = (have_lo - start) as usize;
            let n = (have_hi - have_lo) as usize;
            for (o, c) in out.channels.iter_mut().zip(&self.cache) {
                o[dst..dst + n].copy_from_slice(&c[src..src + n]);
            }
        }
        self.cursor = self.cursor.max(hi);
        self.trim_cache(lo);
        Ok(out)
    }
}

impl std::fmt::Debug for AudioFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioFile")
            .field("format", &self.demux.format_name())
            .field("codec", &self.track.name)
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("duration", &self.duration())
            .finish()
    }
}
