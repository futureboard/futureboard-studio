//! Software encode and mux: H.264 / ProRes video, AAC-LC audio, MP4 / MOV files.
//!
//! The encoders take straight (non-premultiplied) RGBA8 frames, or planar 8-bit 4:2:0 for H.264,
//! and produce [`Packet`]s timed in their track's timescale. [`MediaWriter`] puts those packets in
//! a progressive MP4 / MOV (`moov` written on [`MediaWriter::finish`]) or in a fragmented MP4 that
//! stays playable if the process dies mid-recording.
//!
//! ```no_run
//! use mkff_codecs::encode::{AudioEncoder, Container, MediaWriter, VideoEncoder, VideoEncoderConfig};
//!
//! let mut video = VideoEncoder::new(VideoEncoderConfig::h264(1920, 1080, 60, 1))?;
//! let mut audio = AudioEncoder::aac(48_000, 2, 192_000)?;
//! let mut out = MediaWriter::create("show.mp4", Container::Mp4, false)?;
//! let v = out.add_video_track(&video)?;
//! let a = out.add_audio_track(&audio)?;
//! let rgba = vec![0u8; 1920 * 1080 * 4];
//! for i in 0..60 {
//!     for p in video.encode_rgba(&rgba, i)? { out.write(v, &p)?; }
//! }
//! for p in audio.encode(&[&[0.0; 48_000], &[0.0; 48_000]])? { out.write(a, &p)?; }
//! for p in video.flush()? { out.write(v, &p)?; }
//! for p in audio.flush()? { out.write(a, &p)?; }
//! out.finish()?;
//! # Ok::<(), mkff_codecs::Error>(())
//! ```

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use filmcraft_isobmff::{
    AvcConfig, Brand, FourCc, FragmentedWriter, Mp4Writer, SampleEntry, TrackConfig, WriteSample,
    WriterOptions,
};
use rayon::prelude::*;

use crate::{Error, Result};

// ---------------------------------------------------------------------------------------------
// Video

/// H.264 profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum H264Profile {
    /// Constrained Baseline: CAVLC, no B-frames (lowest latency, widest decoder support).
    Baseline,
    Main,
    #[default]
    High,
}

/// H.264 speed / quality trade-off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum H264Preset {
    /// For live capture: the fastest mode decisions.
    #[default]
    Speed,
    Balanced,
    Quality,
}

/// H.264 rate control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum H264Rate {
    /// Constant quality, roughly the P-frame QP (0–51, lower is better).
    Crf(f32),
    /// Constant bitrate (1-second VBV), for streaming.
    Cbr { kbps: u32 },
    /// Average `target_kbps`, peaks limited to `max_kbps`.
    Vbr { target_kbps: u32, max_kbps: u32 },
}

/// H.264 encoder settings.
#[derive(Clone, Debug, PartialEq)]
pub struct H264Settings {
    pub profile: H264Profile,
    pub preset: H264Preset,
    pub rate: H264Rate,
    /// Frames between IDR pictures; `None` = two seconds.
    pub keyint: Option<u32>,
    /// Consecutive B-frames (0–3; 0 for the lowest latency). Ignored for Baseline.
    pub bframes: u8,
    /// Worker threads (0 = all cores).
    pub threads: usize,
}

impl Default for H264Settings {
    fn default() -> Self {
        H264Settings {
            profile: H264Profile::High,
            preset: H264Preset::Speed,
            rate: H264Rate::Vbr { target_kbps: 12_000, max_kbps: 18_000 },
            keyint: None,
            bframes: 0,
            threads: 0,
        }
    }
}

/// Apple ProRes flavour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProResProfile {
    Proxy,
    Lt,
    Standard,
    #[default]
    Hq,
    /// 4:4:4 with optional alpha.
    P4444,
    /// 4:4:4 XQ with optional alpha.
    P4444Xq,
}

impl ProResProfile {
    fn filmcraft(self) -> filmcraft_prores::Profile {
        use filmcraft_prores::Profile as P;
        match self {
            ProResProfile::Proxy => P::Proxy,
            ProResProfile::Lt => P::Lt,
            ProResProfile::Standard => P::Standard,
            ProResProfile::Hq => P::Hq,
            ProResProfile::P4444 => P::P4444,
            ProResProfile::P4444Xq => P::P4444Xq,
        }
    }

    /// Whether the profile can carry an alpha channel.
    pub fn has_alpha(self) -> bool {
        matches!(self, ProResProfile::P4444 | ProResProfile::P4444Xq)
    }
}

/// Video codecs with a software encoder.
#[derive(Clone, Debug, PartialEq)]
pub enum VideoEncodeCodec {
    H264(H264Settings),
    /// `alpha`: code the RGBA alpha channel (4444 profiles only).
    ProRes { profile: ProResProfile, alpha: bool },
}

impl VideoEncodeCodec {
    pub fn name(&self) -> &'static str {
        match self {
            VideoEncodeCodec::H264(_) => "h264",
            VideoEncodeCodec::ProRes { .. } => "prores",
        }
    }
}

/// What to encode: size, frame rate and codec.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoEncoderConfig {
    pub width: u32,
    pub height: u32,
    /// Frame rate as a fraction (`60/1`, `60000/1001`, …).
    pub fps_num: u32,
    pub fps_den: u32,
    pub codec: VideoEncodeCodec,
}

impl VideoEncoderConfig {
    /// H.264 High with the live defaults of [`H264Settings`].
    pub fn h264(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Self {
        VideoEncoderConfig {
            width,
            height,
            fps_num,
            fps_den,
            codec: VideoEncodeCodec::H264(H264Settings::default()),
        }
    }

    /// ProRes (all-intra; large files, cheap to decode and to seek).
    pub fn prores(width: u32, height: u32, fps_num: u32, fps_den: u32, profile: ProResProfile) -> Self {
        VideoEncoderConfig {
            width,
            height,
            fps_num,
            fps_den,
            codec: VideoEncodeCodec::ProRes { profile, alpha: profile.has_alpha() },
        }
    }
}

/// One coded picture or audio access unit, timed in its track's timescale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub duration: u32,
    pub keyframe: bool,
}

enum VideoInner {
    H264 {
        enc: Box<filmcraft_h264enc::Encoder>,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    },
    ProRes {
        enc: Box<filmcraft_prores::Encoder>,
        profile: filmcraft_prores::Profile,
        alpha: bool,
    },
}

/// A video encoder for frames of one size and rate.
pub struct VideoEncoder {
    cfg: VideoEncoderConfig,
    inner: VideoInner,
}

impl VideoEncoder {
    pub fn new(cfg: VideoEncoderConfig) -> Result<Self> {
        if cfg.width == 0 || cfg.height == 0 || cfg.width > u16::MAX as u32 || cfg.height > u16::MAX as u32 {
            return Err(Error::InvalidArgument(format!("frame size {}x{}", cfg.width, cfg.height)));
        }
        if cfg.fps_num == 0 || cfg.fps_den == 0 {
            return Err(Error::InvalidArgument(format!("frame rate {}/{}", cfg.fps_num, cfg.fps_den)));
        }
        let inner = match &cfg.codec {
            VideoEncodeCodec::H264(s) => {
                if cfg.width % 2 != 0 || cfg.height % 2 != 0 {
                    return Err(Error::InvalidArgument("H.264 4:2:0 needs an even frame size".into()));
                }
                let mut c = filmcraft_h264enc::EncoderConfig::new(cfg.width, cfg.height, cfg.fps_num, cfg.fps_den);
                c.profile = match s.profile {
                    H264Profile::Baseline => filmcraft_h264enc::Profile::Baseline,
                    H264Profile::Main => filmcraft_h264enc::Profile::Main,
                    H264Profile::High => filmcraft_h264enc::Profile::High,
                };
                c.preset = match s.preset {
                    H264Preset::Speed => filmcraft_h264enc::Preset::Speed,
                    H264Preset::Balanced => filmcraft_h264enc::Preset::Balanced,
                    H264Preset::Quality => filmcraft_h264enc::Preset::Quality,
                };
                c.rate = match s.rate {
                    H264Rate::Crf(q) => filmcraft_h264enc::RateControl::Crf(if q.is_finite() { q } else { 23.0 }),
                    H264Rate::Cbr { kbps } => filmcraft_h264enc::RateControl::Cbr { kbps: kbps.max(100) },
                    H264Rate::Vbr { target_kbps, max_kbps } => filmcraft_h264enc::RateControl::Vbr {
                        target_kbps: target_kbps.max(100),
                        max_kbps: max_kbps.max(target_kbps.max(100)),
                    },
                };
                c.keyint = s.keyint.filter(|k| *k > 0).unwrap_or_else(|| (2 * cfg.fps_num).div_ceil(cfg.fps_den).max(1));
                c.bframes = s.bframes.min(3);
                c.threads = s.threads;
                // MP4 samples: length-prefixed NAL units, parameter sets in the `avcC`.
                c.format = filmcraft_h264enc::PacketFormat::LengthPrefixed;
                c.aud = false;
                // Same slicing as FilmCraft's exporter: about four macroblock rows per slice.
                c.slices = (cfg.height.div_ceil(16) as usize).div_ceil(4).max(1);
                let enc = filmcraft_h264enc::Encoder::new(c).map_err(|e| Error::Encode(format!("H.264: {e}")))?;
                VideoInner::H264 { enc: Box::new(enc), y: Vec::new(), u: Vec::new(), v: Vec::new() }
            }
            VideoEncodeCodec::ProRes { profile, alpha } => {
                let p = profile.filmcraft();
                let mut c = filmcraft_prores::EncoderConfig::new(p, cfg.width, cfg.height);
                c.encode_alpha = *alpha && profile.has_alpha();
                VideoInner::ProRes {
                    enc: Box::new(filmcraft_prores::Encoder::with_config(c)),
                    profile: p,
                    alpha: *alpha && profile.has_alpha(),
                }
            }
        };
        Ok(VideoEncoder { cfg, inner })
    }

    pub fn config(&self) -> &VideoEncoderConfig {
        &self.cfg
    }

    /// Track timescale: a multiple of `fps_num` of at least 10 000 ticks per second, so one frame
    /// lasts [`frame_duration`](Self::frame_duration) ticks. (Readers round lookups to the
    /// timescale; with `fps_num` itself, e.g. 30, half a frame would already round to the next.)
    pub fn timescale(&self) -> u32 {
        self.cfg.fps_num * self.tick_scale()
    }

    /// Ticks per frame in [`timescale`](Self::timescale).
    pub fn frame_duration(&self) -> u32 {
        self.cfg.fps_den * self.tick_scale()
    }

    fn tick_scale(&self) -> u32 {
        10_000u32.div_ceil(self.cfg.fps_num).max(1)
    }

    /// Frames the encoder holds back before emitting packets (B-frame reordering).
    pub fn delay(&self) -> usize {
        match &self.inner {
            VideoInner::H264 { enc, .. } => enc.delay(),
            VideoInner::ProRes { .. } => 0,
        }
    }

    /// The MP4 / MOV sample entry for this stream.
    pub fn sample_entry(&self) -> SampleEntry {
        let (w, h) = (self.cfg.width as u16, self.cfg.height as u16);
        match &self.inner {
            VideoInner::H264 { enc, .. } => {
                let avc = AvcConfig::parse(&enc.avcc()).unwrap_or_else(|_| {
                    let (sps, pps) = enc.sps_pps();
                    AvcConfig::new(vec![sps], vec![pps], 4)
                });
                SampleEntry::avc(avc, w, h)
            }
            VideoInner::ProRes { profile, .. } => SampleEntry::prores(FourCc(profile.fourcc()), w, h),
        }
    }

    /// Encode straight RGBA8 (sRGB / BT.709 primaries), `width * height * 4` bytes, as frame
    /// number `index` of the stream.
    pub fn encode_rgba(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
        if rgba.len() < w * h * 4 {
            return Err(Error::InvalidArgument(format!("RGBA frame of {} bytes for {w}x{h}", rgba.len())));
        }
        let pts = self.pts(index);
        let den = self.frame_duration();
        match &mut self.inner {
            VideoInner::H264 { enc, y, u, v } => {
                rgba_to_yuv420_8(rgba, w, h, y, u, v);
                let frame = filmcraft_h264enc::YuvFrame { y, u, v, y_stride: w, uv_stride: w.div_ceil(2) };
                let ps = enc.try_encode(&frame, pts).map_err(|e| Error::Encode(format!("H.264: {e}")))?;
                Ok(h264_packets(ps, den))
            }
            VideoInner::ProRes { enc, profile, alpha } => {
                let chroma = profile.chroma();
                let mut f = filmcraft_prores::Frame::new(w as u32, h as u32, chroma, 10, *alpha);
                match chroma {
                    filmcraft_prores::ChromaFormat::Yuv422 => rgba_to_yuv422_10(rgba, w, h, &mut f.y, &mut f.cb, &mut f.cr),
                    filmcraft_prores::ChromaFormat::Yuv444 => {
                        rgba_to_yuv444_10(rgba, w, h, &mut f.y, &mut f.cb, &mut f.cr, f.alpha.as_deref_mut())
                    }
                }
                let data = enc.encode(&f).map_err(|e| Error::Encode(format!("ProRes: {e}")))?;
                Ok(vec![Packet { data, pts, dts: pts, duration: den, keyframe: true }])
            }
        }
    }

    /// Encode a planar 8-bit 4:2:0 BT.709 limited-range picture (H.264 only), e.g. one converted
    /// on the GPU.
    pub fn encode_yuv420(&mut self, y: &[u8], u: &[u8], v: &[u8], y_stride: usize, uv_stride: usize, index: u64) -> Result<Vec<Packet>> {
        let pts = self.pts(index);
        let den = self.frame_duration();
        match &mut self.inner {
            VideoInner::H264 { enc, .. } => {
                let frame = filmcraft_h264enc::YuvFrame { y, u, v, y_stride, uv_stride };
                let ps = enc.try_encode(&frame, pts).map_err(|e| Error::Encode(format!("H.264: {e}")))?;
                Ok(h264_packets(ps, den))
            }
            VideoInner::ProRes { .. } => Err(Error::Unsupported("ProRes takes RGBA input".into())),
        }
    }

    /// Drain the frames the encoder still holds (call once, at the end).
    pub fn flush(&mut self) -> Result<Vec<Packet>> {
        let den = self.frame_duration();
        Ok(match &mut self.inner {
            VideoInner::H264 { enc, .. } => h264_packets(enc.flush(), den),
            VideoInner::ProRes { .. } => Vec::new(),
        })
    }

    /// Media time of the first presented frame (one frame with B-frames, whose first DTS precedes
    /// the first PTS); written as the track's edit so playback starts at 0.
    pub fn media_start(&self) -> Option<i64> {
        (self.delay() > 0).then_some(self.frame_duration() as i64)
    }

    fn pts(&self, index: u64) -> i64 {
        index as i64 * self.frame_duration() as i64
    }
}

fn h264_packets(ps: Vec<filmcraft_h264enc::Packet>, duration: u32) -> Vec<Packet> {
    ps.into_iter()
        .map(|p| Packet { data: p.data, pts: p.pts, dts: p.dts, duration, keyframe: p.keyframe })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Audio

/// An AAC-LC audio encoder (1024 samples per channel per packet): MKFF's own (`mkff-aac`).
pub struct AudioEncoder {
    enc: mkff_aac::AacEncoder,
    sample_rate: u32,
    channels: usize,
}

impl AudioEncoder {
    /// Constant-bitrate AAC-LC. `channels` 1–8, planar input in WAV order (L R C LFE Ls Rs …).
    pub fn aac(sample_rate: u32, channels: usize, bitrate_bps: u32) -> Result<Self> {
        if channels == 0 || channels > 8 {
            return Err(Error::InvalidArgument(format!("{channels} audio channels")));
        }
        // An AAC frame carries at most 6144 bits per channel (ISO/IEC 14496-3 §4.5.3.2).
        let max = (6 * sample_rate as u64 * channels as u64).min(u32::MAX as u64) as u32;
        let cfg = mkff_aac::AacEncoderConfig::new(sample_rate, channels, bitrate_bps.clamp(16_000, max));
        let enc = mkff_aac::AacEncoder::new(cfg).map_err(|e| Error::Encode(format!("AAC: {e}")))?;
        Ok(AudioEncoder { enc, sample_rate, channels })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Track timescale: the sample rate.
    pub fn timescale(&self) -> u32 {
        self.sample_rate
    }

    /// Encoder delay in samples (the track's edit skips it).
    pub fn priming(&self) -> u32 {
        mkff_audio::Encoder::priming(&self.enc)
    }

    pub fn sample_entry(&self) -> SampleEntry {
        SampleEntry::aac(mkff_audio::Encoder::config(&self.enc), self.channels as u32, self.sample_rate)
    }

    /// Encode planar f32 (one slice per channel, any length); returns the whole packets ready.
    pub fn encode(&mut self, planar: &[&[f32]]) -> Result<Vec<Packet>> {
        if planar.len() != self.channels {
            return Err(Error::InvalidArgument(format!("{} channels for a {}-channel encoder", planar.len(), self.channels)));
        }
        let ps = mkff_audio::Encoder::encode(&mut self.enc, planar).map_err(|e| Error::Encode(format!("AAC: {e}")))?;
        Ok(Self::packets(ps))
    }

    /// Pad and drain the last frames (call once, at the end).
    pub fn flush(&mut self) -> Result<Vec<Packet>> {
        let ps = mkff_audio::Encoder::flush(&mut self.enc).map_err(|e| Error::Encode(format!("AAC: {e}")))?;
        Ok(Self::packets(ps))
    }

    fn packets(ps: Vec<mkff_audio::Packet>) -> Vec<Packet> {
        ps.into_iter()
            .map(|p| Packet { data: p.data, pts: p.pts, dts: p.pts, duration: p.duration, keyframe: true })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Mux

/// Output container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Container {
    #[default]
    Mp4,
    /// QuickTime; use for ProRes.
    Mov,
}

/// A track handle from [`MediaWriter::add_video_track`] / [`MediaWriter::add_audio_track`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TrackId(usize);

enum Sink {
    Progressive(Mp4Writer<BufWriter<File>>),
    Fragmented(FragmentedWriter<BufWriter<File>>),
}

/// Writes encoded packets to an MP4 / MOV file.
///
/// Progressive files get their index (`moov`) at [`finish`](Self::finish) and are unplayable
/// until then. Fragmented files write a fragment at every video keyframe, so a recording cut
/// short by a crash or power loss is still playable up to the last keyframe.
pub struct MediaWriter {
    sink: Sink,
    video_track: Option<usize>,
    /// Fragmented: a fragment is cut at a video keyframe once this many samples are buffered.
    buffered: usize,
}

impl MediaWriter {
    pub fn create(path: impl AsRef<Path>, container: Container, fragmented: bool) -> Result<Self> {
        let file = BufWriter::with_capacity(1 << 20, File::create(path.as_ref())?);
        let mut opts = WriterOptions::new(match container {
            Container::Mp4 => Brand::Mp4,
            Container::Mov => Brand::Mov,
        });
        opts.metadata.push(("\u{a9}too".into(), "MediaKit Foundation".into()));
        let sink = if fragmented {
            Sink::Fragmented(FragmentedWriter::new(file, opts))
        } else {
            Sink::Progressive(Mp4Writer::new(file, opts).map_err(mux_err)?)
        };
        Ok(MediaWriter { sink, video_track: None, buffered: 0 })
    }

    /// Add the video track (before the first packet).
    pub fn add_video_track(&mut self, enc: &VideoEncoder) -> Result<TrackId> {
        let mut cfg = TrackConfig::new(enc.sample_entry(), enc.timescale());
        cfg.media_start = enc.media_start();
        let id = self.add_track(cfg)?;
        self.video_track.get_or_insert(id);
        Ok(TrackId(id))
    }

    /// Add an H.264 track for packets from another encoder (e.g. the hardware encoders of
    /// `mkff-hwenc`), before the first packet. `avcc` is the AVCDecoderConfigurationRecord;
    /// packets carry 4-byte length-prefixed NAL units timed in `timescale`.
    pub fn add_h264_track(&mut self, avcc: &[u8], width: u16, height: u16, timescale: u32) -> Result<TrackId> {
        let avc = AvcConfig::parse(avcc).map_err(mux_err)?;
        let id = self.add_track(TrackConfig::new(SampleEntry::avc(avc, width, height), timescale))?;
        self.video_track.get_or_insert(id);
        Ok(TrackId(id))
    }

    /// Add an audio track (before the first packet).
    pub fn add_audio_track(&mut self, enc: &AudioEncoder) -> Result<TrackId> {
        let mut cfg = TrackConfig::new(enc.sample_entry(), enc.timescale());
        cfg.media_start = Some(enc.priming() as i64);
        Ok(TrackId(self.add_track(cfg)?))
    }

    fn add_track(&mut self, cfg: TrackConfig) -> Result<usize> {
        match &mut self.sink {
            Sink::Progressive(w) => w.add_track(cfg),
            Sink::Fragmented(w) => w.add_track(cfg),
        }
        .map_err(mux_err)
    }

    /// Write one packet (in decode order within its track).
    pub fn write(&mut self, track: TrackId, p: &Packet) -> Result<()> {
        let s = WriteSample {
            data: &p.data,
            duration: p.duration,
            composition_offset: i32::try_from(p.pts - p.dts).unwrap_or(0),
            is_sync: p.keyframe,
        };
        match &mut self.sink {
            Sink::Progressive(w) => w.write_sample(track.0, s).map_err(mux_err),
            Sink::Fragmented(w) => {
                if p.keyframe && Some(track.0) == self.video_track && self.buffered > 0 {
                    w.flush_fragment().map_err(mux_err)?;
                    self.buffered = 0;
                }
                w.write_sample(track.0, s).map_err(mux_err)?;
                self.buffered += 1;
                Ok(())
            }
        }
    }

    /// Write the index (progressive) or the last fragment, and close the file.
    pub fn finish(self) -> Result<()> {
        use std::io::Write;
        let mut f = match self.sink {
            Sink::Progressive(w) => w.finish().map_err(mux_err)?,
            Sink::Fragmented(w) => w.finish().map_err(mux_err)?,
        };
        f.flush()?;
        Ok(())
    }
}

fn mux_err(e: filmcraft_isobmff::Error) -> Error {
    match e {
        filmcraft_isobmff::Error::Io(e) => Error::Io(e.to_string()),
        other => Error::Encode(format!("mux: {other}")),
    }
}

// ---------------------------------------------------------------------------------------------
// RGBA → Y'CbCr (BT.709, limited range)

const KR: f32 = 0.2126;
const KB: f32 = 0.0722;
const KG: f32 = 1.0 - KR - KB;

/// Normalised (Y' 0..1, Cb / Cr −0.5..0.5) from straight RGBA8.
#[inline]
fn ycbcr(p: &[u8]) -> (f32, f32, f32) {
    let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
    let y = KR * r + KG * g + KB * b;
    (y, (b - y) / (2.0 * (1.0 - KB)), (r - y) / (2.0 * (1.0 - KR)))
}

#[inline]
fn q8(y: f32, chroma: bool) -> u8 {
    let v = if chroma { 128.0 + 224.0 * y } else { 16.0 + 219.0 * y };
    v.round().clamp(0.0, 255.0) as u8
}

#[inline]
fn q10(y: f32, chroma: bool) -> u16 {
    let v = if chroma { 512.0 + 896.0 * y } else { 64.0 + 876.0 * y };
    v.round().clamp(0.0, 1023.0) as u16
}

/// 8-bit 4:2:0 (2×2 chroma average) from RGBA8.
pub fn rgba_to_yuv420_8(rgba: &[u8], w: usize, h: usize, y: &mut Vec<u8>, u: &mut Vec<u8>, v: &mut Vec<u8>) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    y.resize(w * h, 0);
    u.resize(cw * ch, 0);
    v.resize(cw * ch, 0);
    y.par_chunks_mut(w * 2)
        .zip(u.par_chunks_mut(cw).zip(v.par_chunks_mut(cw)))
        .enumerate()
        .for_each(|(cy, (yr, (ur, vr)))| {
            let rows = yr.len() / w;
            for cx in 0..cw {
                let (mut sb, mut sr, mut n) = (0.0f32, 0.0f32, 0.0f32);
                for dy in 0..rows {
                    let row = cy * 2 + dy;
                    for dx in 0..2 {
                        let x = cx * 2 + dx;
                        if x >= w {
                            continue;
                        }
                        let i = (row * w + x) * 4;
                        let (yy, cb, cr) = ycbcr(&rgba[i..i + 4]);
                        yr[dy * w + x] = q8(yy, false);
                        sb += cb;
                        sr += cr;
                        n += 1.0;
                    }
                }
                ur[cx] = q8(sb / n, true);
                vr[cx] = q8(sr / n, true);
            }
        });
}

/// 10-bit 4:2:2 (horizontal chroma average) from RGBA8.
pub fn rgba_to_yuv422_10(rgba: &[u8], w: usize, h: usize, y: &mut [u16], cb: &mut [u16], cr: &mut [u16]) {
    let cw = w.div_ceil(2);
    y.par_chunks_mut(w)
        .zip(cb.par_chunks_mut(cw).zip(cr.par_chunks_mut(cw)))
        .take(h)
        .enumerate()
        .for_each(|(row, (yr, (br, rr)))| {
            for cx in 0..cw {
                let (mut sb, mut sr, mut n) = (0.0f32, 0.0f32, 0.0f32);
                for x in cx * 2..(cx * 2 + 2).min(w) {
                    let i = (row * w + x) * 4;
                    let (yy, b, r) = ycbcr(&rgba[i..i + 4]);
                    yr[x] = q10(yy, false);
                    sb += b;
                    sr += r;
                    n += 1.0;
                }
                br[cx] = q10(sb / n, true);
                rr[cx] = q10(sr / n, true);
            }
        });
}

/// 10-bit 4:4:4 from RGBA8, plus full-range 10-bit alpha when `alpha` is given.
pub fn rgba_to_yuv444_10(rgba: &[u8], w: usize, h: usize, y: &mut [u16], cb: &mut [u16], cr: &mut [u16], alpha: Option<&mut [u16]>) {
    y.par_chunks_mut(w)
        .zip(cb.par_chunks_mut(w).zip(cr.par_chunks_mut(w)))
        .take(h)
        .enumerate()
        .for_each(|(row, (yr, (br, rr)))| {
            for x in 0..w {
                let i = (row * w + x) * 4;
                let (yy, b, r) = ycbcr(&rgba[i..i + 4]);
                yr[x] = q10(yy, false);
                br[x] = q10(b, true);
                rr[x] = q10(r, true);
            }
        });
    if let Some(a) = alpha {
        a.par_iter_mut().take(w * h).enumerate().for_each(|(i, a)| {
            *a = (rgba[i * 4 + 3] as u16) * 1023 / 255;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_ranges() {
        let (mut y, mut u, mut v) = (Vec::new(), Vec::new(), Vec::new());
        rgba_to_yuv420_8(&[255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255], 2, 2, &mut y, &mut u, &mut v);
        assert_eq!(y, [235, 16, 235, 16]);
        assert_eq!((u[0], v[0]), (128, 128));
    }
}
