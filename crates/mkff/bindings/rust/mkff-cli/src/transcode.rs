//! Turning inputs into one output: stream selection, timing, audio / video encode and stream copy.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use mkff_audio::format::{
    AdtsWriter, FlacWriter, FlacWriterOptions, M4aTrack, M4aWriter, MpaGapless, MpaInfoTag,
    MpaWriter, OggOpusWriter,
};
use mkff_audio::{AudioWriter, CodecId, Container as AudioContainer, EncoderSettings};
use mkff_codecs::encode::{
    AudioEncoder, Container as MovieContainer, H264Preset, H264Rate, H264Settings, MediaWriter,
    Packet, ProResProfile, TrackId, VideoEncodeCodec, VideoEncoder, VideoEncoderConfig,
};
use mkff_codecs::{AudioBuffer, MediaFile, MediaKind};

use crate::args::{AudioFilter, Global, InputSpec, Level, OutputSpec};
use crate::progress::Progress;
use crate::units::layout;

/// An opened input.
pub struct Input {
    pub spec: InputSpec,
    pub media: MediaFile,
}

type R<T> = Result<T, String>;

fn err<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// What kind of file an output is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutKind {
    Audio(AudioContainer),
    Ogg,
    Movie(MovieContainer),
}

fn out_kind(o: &OutputSpec) -> R<OutKind> {
    let ext = Path::new(&o.path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let f = o.format.clone().unwrap_or(ext);
    Ok(match f.as_str() {
        "wav" | "wave" => OutKind::Audio(AudioContainer::Wav),
        "mp3" | "mp2" => OutKind::Audio(AudioContainer::Mpa),
        "adts" | "aac" => OutKind::Audio(AudioContainer::Adts),
        "flac" => OutKind::Audio(AudioContainer::Flac),
        "ipod" | "m4a" | "m4b" => OutKind::Audio(AudioContainer::M4a),
        "mp4" | "m4v" => OutKind::Movie(MovieContainer::Mp4),
        "mov" | "qt" => OutKind::Movie(MovieContainer::Mov),
        "ogg" | "opus" | "oga" => OutKind::Ogg,
        "" => {
            return Err(format!(
                "Unable to choose an output format for '{}'; use a standard extension or -f",
                o.path
            ))
        }
        other => {
            return Err(format!(
                "Requested output format '{other}' is not supported (see mkfx -formats)"
            ))
        }
    })
}

/// The streams an output takes: (input, audio stream) and the video's input.
struct Pick {
    video: Option<usize>,
    audio: Option<(usize, usize)>,
}

fn select(o: &OutputSpec, inputs: &[Input], kind: OutKind) -> R<Pick> {
    let has_video =
        |i: &Input| i.media.info().video.is_some() && i.media.info().kind == MediaKind::Movie;
    let mut p = Pick {
        video: None,
        audio: None,
    };
    if o.maps.is_empty() {
        p.video = inputs.iter().position(has_video);
        p.audio = inputs
            .iter()
            .position(|i| !i.media.info().audio.is_empty())
            .map(|k| (k, 0));
    } else {
        for m in &o.maps {
            let inp = inputs
                .get(m.input)
                .ok_or_else(|| format!("Invalid input file index: {}", m.input))?;
            if m.video != Some(false)
                && has_video(inp)
                && m.index.unwrap_or(0) == 0
                && p.video.is_none()
            {
                p.video = Some(m.input);
            }
            if m.video != Some(true) && p.audio.is_none() {
                let n = m.index.unwrap_or(0);
                if n < inp.media.info().audio.len() {
                    p.audio = Some((m.input, n));
                } else if m.video == Some(false) {
                    return Err(format!(
                        "Stream map '{}:a:{n}' matches no streams.",
                        m.input
                    ));
                }
            }
        }
    }
    if o.no_video || !matches!(kind, OutKind::Movie(_)) {
        p.video = None;
    }
    if o.no_audio {
        p.audio = None;
    }
    if p.video.is_none() && p.audio.is_none() {
        return Err(format!(
            "Output file '{}' does not contain any stream",
            o.path
        ));
    }
    Ok(p)
}

/// Start (seconds on the input's timeline) and length of what an output takes from an input.
fn window(inp: &Input, o: &OutputSpec) -> (f64, f64) {
    let in_start = inp.spec.window.start.unwrap_or(0.0).max(0.0);
    let out_start = o.window.start.unwrap_or(0.0).max(0.0);
    let start = in_start + out_start;
    let avail = (inp.media.info().duration_seconds - start).max(0.0);
    let mut len = avail;
    if let Some(l) = inp.spec.window.limit(in_start) {
        len = len.min((l - out_start).max(0.0));
    }
    // output -t / -to are on the output's own timeline (which starts at its -ss)
    if let Some(l) = o.window.limit(out_start) {
        len = len.min(l.max(0.0));
    }
    (start, len)
}

// ---- audio ------------------------------------------------------------------------------------

/// Sequential reader over a window of one audio stream, at the output rate.
struct AudioReader<'a> {
    media: &'a MediaFile,
    stream: usize,
    rate: u32,
    pos: i64,
    end: i64,
    start: i64,
}

impl<'a> AudioReader<'a> {
    fn new(media: &'a MediaFile, stream: usize, rate: u32, start: f64, len: f64) -> Self {
        let pos = (start * f64::from(rate)).round() as i64;
        let end = pos + (len * f64::from(rate)).round() as i64;
        AudioReader {
            media,
            stream,
            rate,
            pos,
            end,
            start: pos,
        }
    }

    fn read(&mut self, n: usize) -> R<Option<AudioBuffer>> {
        if self.pos >= self.end {
            return Ok(None);
        }
        let k = (n as i64).min(self.end - self.pos) as usize;
        let b = self
            .media
            .audio_stream(self.stream, self.pos, k, self.rate)
            .map_err(err("decode audio"))?;
        self.pos += k as i64;
        Ok(Some(b))
    }

    /// Samples produced so far.
    fn done(&self) -> i64 {
        self.pos - self.start
    }

    fn seconds(&self) -> f64 {
        self.done() as f64 / f64::from(self.rate)
    }
}

/// Mix WAV-ordered channels to `out` channels (standard downmix coefficients, normalised so the
/// result cannot clip; upmixes copy the front channels).
fn remix(chans: Vec<Vec<f32>>, out: usize) -> Vec<Vec<f32>> {
    let inn = chans.len();
    if inn == out || inn == 0 {
        return chans;
    }
    let n = chans[0].len();
    if inn == 1 {
        let mut v = vec![chans[0].clone(); out.min(2)];
        v.resize(out, vec![0.0; n]);
        return v;
    }
    if out > inn {
        let mut v = chans;
        v.resize(out, vec![0.0; n]);
        return v;
    }
    // per input channel: (gain to left, gain to right); WAV order by channel count
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let lr: Vec<(f32, f32)> = match inn {
        3 => vec![(1.0, 0.0), (0.0, 1.0), (C, C)],
        4 => vec![(1.0, 0.0), (0.0, 1.0), (C, 0.0), (0.0, C)],
        5 => vec![(1.0, 0.0), (0.0, 1.0), (C, C), (C, 0.0), (0.0, C)],
        6 => vec![
            (1.0, 0.0),
            (0.0, 1.0),
            (C, C),
            (0.0, 0.0),
            (C, 0.0),
            (0.0, C),
        ],
        7 => vec![
            (1.0, 0.0),
            (0.0, 1.0),
            (C, C),
            (0.0, 0.0),
            (0.5, 0.5),
            (C, 0.0),
            (0.0, C),
        ],
        _ => {
            let mut v = vec![
                (1.0, 0.0),
                (0.0, 1.0),
                (C, C),
                (0.0, 0.0),
                (C, 0.0),
                (0.0, C),
                (C, 0.0),
                (0.0, C),
            ];
            v.resize(inn, (0.0, 0.0));
            v
        }
    };
    let norm = 1.0 / lr.iter().map(|g| g.0).sum::<f32>().max(1.0);
    let mut l = vec![0.0f32; n];
    let mut r = vec![0.0f32; n];
    for (c, (gl, gr)) in chans.iter().zip(&lr) {
        for i in 0..n {
            l[i] += c[i] * gl;
            r[i] += c[i] * gr;
        }
    }
    for i in 0..n {
        l[i] *= norm;
        r[i] *= norm;
    }
    if out == 1 {
        return vec![l.iter().zip(&r).map(|(a, b)| (a + b) * 0.5).collect()];
    }
    let mut v = vec![l, r];
    v.resize(out, vec![0.0; n]);
    v
}

fn process(buf: AudioBuffer, out_ch: usize, filters: &[AudioFilter]) -> Vec<Vec<f32>> {
    let mut ch = remix(buf.channels, out_ch);
    for f in filters {
        match f {
            AudioFilter::Volume(g) => {
                for c in &mut ch {
                    for s in c.iter_mut() {
                        *s *= g;
                    }
                }
            }
        }
    }
    ch
}

/// An audio codec choice.
enum AudioCodecChoice {
    Encode {
        codec: CodecId,
        bits: u32,
        name: &'static str,
    },
    Copy,
}

fn audio_codec(name: &str) -> R<AudioCodecChoice> {
    use AudioCodecChoice::*;
    Ok(match name.to_ascii_lowercase().as_str() {
        "copy" => Copy,
        "aac" | "libfdk_aac" | "aac_mf" => Encode { codec: CodecId::Aac, bits: 0, name: "aac" },
        "mp3" | "libmp3lame" | "mp3lame" | "libshine" => Encode { codec: CodecId::MpegAudio, bits: 0, name: "mp3" },
        "flac" => Encode { codec: CodecId::Flac, bits: 0, name: "flac" },
        "alac" => Encode { codec: CodecId::Alac, bits: 0, name: "alac" },
        "pcm_s16le" | "pcm_s16be" | "pcm" => Encode { codec: CodecId::Pcm, bits: 16, name: "pcm_s16le" },
        "pcm_s24le" | "pcm_s24be" => Encode { codec: CodecId::Pcm, bits: 24, name: "pcm_s24le" },
        "pcm_f32le" | "pcm_f32be" | "pcm_s32le" => Encode { codec: CodecId::Pcm, bits: 32, name: "pcm_f32le" },
        "opus" | "libopus" | "vorbis" | "libvorbis" | "ac3" | "eac3" => {
            return Err(format!("Encoder '{name}' is not available: MKFF decodes it but has no encoder yet (try aac, mp3, flac, alac, pcm_s16le)"))
        }
        other => return Err(format!("Unknown encoder '{other}' (see mkfx -encoders)")),
    })
}

fn default_audio_codec(kind: OutKind) -> &'static str {
    match kind {
        OutKind::Audio(AudioContainer::Wav) => "pcm_s16le",
        OutKind::Audio(AudioContainer::Mpa) => "mp3",
        OutKind::Audio(AudioContainer::Flac) => "flac",
        OutKind::Ogg => "copy",
        _ => "aac",
    }
}

// ---- video ------------------------------------------------------------------------------------

enum VideoEnc {
    Sw(Box<VideoEncoder>),
    Hw(Box<mkff_hwenc::HwVideoEncoder>),
}

impl VideoEnc {
    fn encode(&mut self, rgba: &[u8], i: u64) -> R<Vec<Packet>> {
        match self {
            VideoEnc::Sw(e) => e.encode_rgba(rgba, i).map_err(err("encode video")),
            VideoEnc::Hw(e) => e.encode_rgba(rgba, i).map_err(err("encode video")),
        }
    }

    fn flush(&mut self) -> R<Vec<Packet>> {
        match self {
            VideoEnc::Sw(e) => e.flush().map_err(err("encode video")),
            VideoEnc::Hw(e) => e.flush().map_err(err("encode video")),
        }
    }

    fn label(&self) -> String {
        match self {
            VideoEnc::Sw(e) => format!("{} (mkff-codecs)", e.config().codec.name()),
            VideoEnc::Hw(e) => format!("h264 ({} — {})", e.info().vendor.api_name(), e.info().name),
        }
    }
}

/// Bilinear RGBA resize.
fn scale_rgba(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if sw == dw && sh == dh {
        return src.to_vec();
    }
    let mut out = vec![0u8; dw * dh * 4];
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = sy - y0 as f32;
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = sx - x0 as f32;
            for c in 0..4 {
                let p = |xx: usize, yy: usize| f32::from(src[(yy * sw + xx) * 4 + c]);
                let top = p(x0, y0) * (1.0 - wx) + p(x1, y0) * wx;
                let bot = p(x0, y1) * (1.0 - wx) + p(x1, y1) * wx;
                out[(y * dw + x) * 4 + c] = (top * (1.0 - wy) + bot * wy).round() as u8;
            }
        }
    }
    out
}

/// Output size from the input size and `-s` / `scale=` (negative: keep aspect), even for 4:2:0.
fn target_size(w: u32, h: u32, req: Option<(i32, i32)>) -> (u32, u32) {
    let (mut tw, mut th) = match req {
        None => (w as f64, h as f64),
        Some((rw, rh)) if rw > 0 && rh > 0 => (f64::from(rw), f64::from(rh)),
        Some((rw, _)) if rw > 0 => (f64::from(rw), f64::from(rw) * f64::from(h) / f64::from(w)),
        Some((_, rh)) => (f64::from(rh) * f64::from(w) / f64::from(h), f64::from(rh)),
    };
    tw = tw.round().max(2.0);
    th = th.round().max(2.0);
    ((tw as u32) & !1, (th as u32) & !1)
}

fn prores_profile(p: Option<&str>) -> R<ProResProfile> {
    Ok(match p.unwrap_or("hq").to_ascii_lowercase().as_str() {
        "0" | "proxy" => ProResProfile::Proxy,
        "1" | "lt" => ProResProfile::Lt,
        "2" | "standard" | "std" => ProResProfile::Standard,
        "3" | "hq" => ProResProfile::Hq,
        "4" | "4444" => ProResProfile::P4444,
        "5" | "4444xq" | "xq" => ProResProfile::P4444Xq,
        other => return Err(format!("Unknown ProRes profile '{other}'")),
    })
}

fn preset(p: Option<&str>) -> H264Preset {
    match p.unwrap_or("veryfast") {
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "speed" => H264Preset::Speed,
        "medium" | "balanced" => H264Preset::Balanced,
        _ => H264Preset::Quality,
    }
}

fn open_video_encoder(
    o: &OutputSpec,
    kind: MovieContainer,
    w: u32,
    h: u32,
    fps: (u32, u32),
) -> R<VideoEnc> {
    let name = o
        .vcodec
        .clone()
        .unwrap_or_else(|| "h264".into())
        .to_ascii_lowercase();
    let hw_vendor = match name.as_str() {
        "h264_hw" | "h264_mf" | "h264_d3d11va" => Some(None),
        "h264_nvenc" | "nvenc" | "nvenc_h264" => Some(Some(mkff_hwenc::Vendor::Nvidia)),
        "h264_qsv" => Some(Some(mkff_hwenc::Vendor::Intel)),
        "h264_amf" => Some(Some(mkff_hwenc::Vendor::Amd)),
        _ => None,
    };
    if let Some(vendor) = hw_vendor {
        let kbps = o.video_bitrate.map(|b| b / 1000).unwrap_or(8_000).max(100);
        let mut cfg = mkff_hwenc::HwEncoderConfig::cbr(w, h, fps.0, fps.1, kbps);
        if let Some(q) = o.crf {
            cfg.rate = mkff_hwenc::RateControl::Quality(
                (100.0 - q * 100.0 / 51.0).clamp(1.0, 100.0) as u8,
            );
        } else if o.video_bitrate.is_some() {
            cfg.rate = mkff_hwenc::RateControl::Vbr {
                kbps,
                max_kbps: kbps * 3 / 2,
            };
        }
        cfg.keyint = o.gop.unwrap_or(0);
        cfg.low_latency = o.bframes.unwrap_or(0) == 0;
        let enc = mkff_hwenc::HwVideoEncoder::new(vendor, cfg)
            .map_err(|e| format!("{name}: {e} (try -c:v h264 for the software encoder)"))?;
        return Ok(VideoEnc::Hw(Box::new(enc)));
    }
    let cfg = match name.as_str() {
        "h264" | "libx264" | "avc" | "x264" => {
            let mut s = H264Settings { preset: preset(o.preset.as_deref()), ..H264Settings::default() };
            if let Some(q) = o.crf {
                s.rate = H264Rate::Crf(q);
            } else if let Some(b) = o.video_bitrate {
                s.rate = H264Rate::Vbr { target_kbps: b / 1000, max_kbps: b / 1000 * 3 / 2 };
            }
            s.keyint = o.gop;
            s.bframes = o.bframes.unwrap_or(0).min(3);
            VideoEncoderConfig { width: w, height: h, fps_num: fps.0, fps_den: fps.1, codec: VideoEncodeCodec::H264(s) }
        }
        "prores" | "prores_ks" | "prores_aw" => {
            if kind != MovieContainer::Mov {
                return Err("ProRes goes in a .mov file".into());
            }
            VideoEncoderConfig::prores(w, h, fps.0, fps.1, prores_profile(o.video_profile.as_deref())?)
        }
        "copy" => return Err("Video stream copy is not supported yet: MKFF re-encodes video (-c:v h264, h264_hw, prores)".into()),
        other => return Err(format!("Unknown video encoder '{other}' (see mkfx -encoders)")),
    };
    Ok(VideoEnc::Sw(Box::new(
        VideoEncoder::new(cfg).map_err(err("video encoder"))?,
    )))
}

// ---- outputs ----------------------------------------------------------------------------------

pub struct Ctx<'a> {
    pub global: &'a Global,
}

impl Ctx<'_> {
    fn info(&self, s: &str) {
        if self.global.level >= Level::Info {
            eprint!("{s}");
        }
    }
}

fn check_overwrite(path: &str, g: &Global) -> R<()> {
    if Path::new(path).exists() {
        match g.overwrite {
            Some(true) => {}
            Some(false) => return Err(format!("File '{path}' already exists. Exiting.")),
            None => {
                return Err(format!(
                    "File '{path}' already exists. Use -y to overwrite."
                ))
            }
        }
    }
    Ok(())
}

/// Produce output `k`.
pub fn run_output(ctx: &Ctx, k: usize, o: &OutputSpec, inputs: &[Input]) -> R<()> {
    let kind = out_kind(o)?;
    let pick = select(o, inputs, kind)?;
    check_overwrite(&o.path, ctx.global)?;
    match (kind, pick.video) {
        (OutKind::Movie(c), Some(vi)) => movie(ctx, k, o, inputs, c, vi, pick.audio),
        (OutKind::Movie(_), None) => audio_only(
            ctx,
            k,
            o,
            inputs,
            AudioContainer::M4a,
            pick.audio.ok_or("no audio")?,
        ),
        (OutKind::Audio(c), _) => audio_only(
            ctx,
            k,
            o,
            inputs,
            c,
            pick.audio
                .ok_or_else(|| format!("Output file '{}' needs an audio stream", o.path))?,
        ),
        (OutKind::Ogg, _) => audio_only(
            ctx,
            k,
            o,
            inputs,
            AudioContainer::M4a,
            pick.audio.ok_or("no audio")?,
        ),
    }
}

fn audio_only(
    ctx: &Ctx,
    k: usize,
    o: &OutputSpec,
    inputs: &[Input],
    container: AudioContainer,
    (ii, si): (usize, usize),
) -> R<()> {
    let inp = &inputs[ii];
    let kind = out_kind(o)?;
    let codec_name = o
        .acodec
        .clone()
        .unwrap_or_else(|| default_audio_codec(kind).to_owned());
    let choice = audio_codec(&codec_name)?;
    let info = &inp.media.info().audio[si];
    let (start, len) = window(inp, o);
    let AudioCodecChoice::Encode { codec, bits, name } = choice else {
        return stream_copy(ctx, k, o, inp, ii, si, kind, start, len);
    };
    if kind == OutKind::Ogg {
        return Err("Ogg output takes stream copy only (-c:a copy of Opus): MKFF has no Opus / Vorbis encoder yet".into());
    }
    let rate = o.sample_rate.unwrap_or(info.sample_rate);
    let ch = o.channels.unwrap_or(info.channels).clamp(1, 8);
    let mut s = EncoderSettings::new(codec, rate, ch as usize);
    if let Some(b) = o.audio_bitrate {
        s.bitrate = b;
    }
    if bits > 0 {
        s.bits_per_sample = bits;
    }
    if let Some(l) = o.compression_level.or(o.audio_quality) {
        s.level = l;
    }
    let mut w = AudioWriter::create_as(&o.path, container, s).map_err(err(&o.path))?;
    ctx.info(&format!(
        "Stream mapping:\n  Stream #{ii}:{} -> #{k}:0 ({} ({}) -> {name} (mkff))\nOutput #{k}, {}, to '{}':\n  Stream #{k}:0: Audio: {name}, {rate} Hz, {}{}\n",
        si + usize::from(inp.media.info().video.is_some()),
        info.codec.to_lowercase(),
        if inp.media.audio_is_native(si) { "mkff-audio" } else { "filmcraft" },
        container_label(kind),
        o.path,
        layout(ch),
        if matches!(codec, CodecId::Aac | CodecId::MpegAudio) {
            format!(", {} kb/s", w_bitrate(o.audio_bitrate, codec, ch) / 1000)
        } else {
            String::new()
        },
    ));
    let mut reader = AudioReader::new(&inp.media, si, rate, start, len);
    let mut prog = Progress::new(ctx.global, &o.path);
    while let Some(buf) = reader.read(8192)? {
        let chans = process(buf, ch as usize, &o.audio_filters);
        let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        w.write(&refs).map_err(err("encode audio"))?;
        prog.update(None, reader.seconds());
    }
    w.finish().map_err(err(&o.path))?;
    prog.finish(None, reader.seconds());
    Ok(())
}

fn w_bitrate(b: Option<u32>, codec: CodecId, ch: u32) -> u32 {
    b.unwrap_or(match codec {
        CodecId::Aac => (64_000 * ch).min(640_000),
        _ => (64_000 * ch).min(320_000),
    })
}

fn container_label(k: OutKind) -> &'static str {
    match k {
        OutKind::Audio(AudioContainer::Wav) => "wav",
        OutKind::Audio(AudioContainer::Mpa) => "mp3",
        OutKind::Audio(AudioContainer::Adts) => "adts",
        OutKind::Audio(AudioContainer::Flac) => "flac",
        OutKind::Audio(AudioContainer::M4a) => "ipod",
        OutKind::Ogg => "ogg",
        OutKind::Movie(MovieContainer::Mp4) => "mp4",
        OutKind::Movie(MovieContainer::Mov) => "mov",
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_copy(
    ctx: &Ctx,
    k: usize,
    o: &OutputSpec,
    inp: &Input,
    ii: usize,
    si: usize,
    kind: OutKind,
    start: f64,
    len: f64,
) -> R<()> {
    if !inp.media.audio_is_native(si) {
        return Err(format!("Stream copy from '{}' is not supported yet (MKFF does not demux this container itself)", inp.spec.path));
    }
    let mut d = mkff_audio::format::open_path(&inp.spec.path).map_err(err(&inp.spec.path))?;
    let track = d.tracks().get(si).cloned().ok_or("no such audio track")?;
    let tb = f64::from(track.time_base.max(1));
    // packet timestamps include the encoder priming (in samples at the track's rate)
    let priming = track.priming as f64 * tb / f64::from(track.sample_rate.max(1));
    let want = (start * tb + priming).round() as i64;
    let mut first = 0i64;
    if start > 0.0 {
        first = d.seek(si, want).map_err(err("seek"))?;
    }
    let cut_end = len + 1e-6 < inp.media.info().duration_seconds - start;
    let end = if cut_end {
        want + (len * tb).round() as i64
    } else {
        i64::MAX
    };
    let trimmed = start > 0.0 || cut_end;
    let file = BufWriter::new(File::create(&o.path).map_err(err(&o.path))?);
    ctx.info(&format!(
        "Stream mapping:\n  Stream #{ii}:{} -> #{k}:0 (copy)\nOutput #{k}, {}, to '{}':\n  Stream #{k}:0: Audio: {}, {} Hz, {}\n",
        si + usize::from(inp.media.info().video.is_some()),
        container_label(kind),
        o.path,
        track.name.to_lowercase(),
        track.sample_rate,
        layout(track.channels)
    ));
    enum Sink {
        M4a(M4aWriter<BufWriter<File>>, usize),
        Mpa(MpaWriter<BufWriter<File>>),
        Adts(AdtsWriter<BufWriter<File>>),
        Flac(FlacWriter<BufWriter<File>>),
        Ogg(OggOpusWriter<BufWriter<File>>),
    }
    let wrong = |c: &str| format!("Codec {} cannot be copied into {c}", track.name);
    let mut sink = match kind {
        OutKind::Audio(AudioContainer::M4a) | OutKind::Movie(_) => {
            let mut w = M4aWriter::new(file).map_err(err(&o.path))?;
            let mut t = M4aTrack::from_track(&track);
            if trimmed {
                t.priming = if start > 0.0 { 0 } else { t.priming };
                t.duration = None;
            }
            let id = w
                .add_track(t)
                .map_err(|e| format!("{}: {e}", wrong("MP4")))?;
            Sink::M4a(w, id)
        }
        OutKind::Audio(AudioContainer::Mpa) if track.codec == CodecId::MpegAudio => {
            Sink::Mpa(MpaWriter::new(file, true).map_err(err(&o.path))?)
        }
        OutKind::Audio(AudioContainer::Adts) if track.codec == CodecId::Aac => {
            Sink::Adts(AdtsWriter::new(file, &track.config).map_err(err(&o.path))?)
        }
        OutKind::Audio(AudioContainer::Flac) if track.codec == CodecId::Flac => Sink::Flac(
            FlacWriter::new(
                file,
                &track.config,
                FlacWriterOptions {
                    seek_points: 0,
                    padding: 0,
                },
            )
            .map_err(err(&o.path))?,
        ),
        OutKind::Ogg if track.codec == CodecId::Opus => Sink::Ogg(
            OggOpusWriter::new(file, 0x6d6b_6678, &track.config, "mkfx").map_err(err(&o.path))?,
        ),
        OutKind::Audio(AudioContainer::Wav) => {
            return Err(
                "Stream copy into WAV: use -c:a pcm_s16le (lossless for PCM sources)".into(),
            )
        }
        other => return Err(wrong(container_label(other))),
    };
    let mut prog = Progress::new(ctx.global, &o.path);
    let mut last = first;
    while let Some(p) = d.next_packet().map_err(err("demux"))? {
        if p.track != si {
            continue;
        }
        if p.pts >= end {
            break;
        }
        last = p.pts + i64::from(p.duration);
        match &mut sink {
            Sink::M4a(w, id) => w.write_packet(*id, &p.data, p.duration),
            Sink::Mpa(w) => w.write_frame(&p.data),
            Sink::Adts(w) => w.write_packet(&p.data),
            Sink::Flac(w) => w.write_frame(&p.data),
            Sink::Ogg(w) => w.write_packet(&p.data),
        }
        .map_err(err("mux"))?;
        prog.update(None, (last - first) as f64 / tb);
    }
    let done = |r: std::result::Result<BufWriter<File>, mkff_audio::Error>| -> R<()> {
        r.map_err(err(&o.path))?
            .into_inner()
            .map_err(|e| format!("{}: {e}", o.path))
            .map(drop)
    };
    match sink {
        Sink::M4a(w, _) => done(w.finish())?,
        Sink::Mpa(w) => {
            // keep gapless playback: LAME delay excludes, LAME padding includes, the 529-sample
            // decoder delay (the demuxer's priming / padding include / exclude it)
            let gapless = (!trimmed).then(|| MpaGapless {
                encoder_delay: track.priming.saturating_sub(529).min(4095) as u32,
                padding: (track.padding + 529).min(4095) as u32,
            });
            done(w.finish(MpaInfoTag::Auto(gapless)))?
        }
        Sink::Adts(w) => done(w.finish())?,
        Sink::Flac(w) => done(w.finish(None))?,
        // the real length (48 kHz, without pre-skip) lets readers trim the end padding
        Sink::Ogg(w) => done(w.finish(if trimmed { None } else { track.duration }))?,
    }
    prog.finish(None, (last - first) as f64 / tb);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn movie(
    ctx: &Ctx,
    k: usize,
    o: &OutputSpec,
    inputs: &[Input],
    container: MovieContainer,
    vi: usize,
    audio: Option<(usize, usize)>,
) -> R<()> {
    let vin = &inputs[vi];
    let v = vin.media.info().video.clone().ok_or("no video")?;
    let (start, len) = window(vin, o);
    let fps = o.frame_rate.unwrap_or_else(|| {
        let (n, d) = v.frame_rate;
        if n > 0 && d > 0 && n <= u32::MAX as i64 && d <= u32::MAX as i64 {
            (n as u32, d as u32)
        } else {
            (30, 1)
        }
    });
    let (w, h) = target_size(v.width, v.height, o.size);
    let mut venc = open_video_encoder(o, container, w, h, fps)?;
    // audio: AAC only next to video in MP4 / MOV
    let mut aenc = None;
    let mut reader = None;
    let mut out_ch = 2;
    if let Some((ai, si)) = audio {
        let name = o.acodec.clone().unwrap_or_else(|| "aac".into());
        match audio_codec(&name)? {
            AudioCodecChoice::Encode {
                codec: CodecId::Aac,
                ..
            } => {}
            _ => {
                return Err(format!(
                    "Audio next to video in MP4 / MOV is AAC only (got '{name}')"
                ))
            }
        }
        let ainfo = &inputs[ai].media.info().audio[si];
        let rate = o.sample_rate.unwrap_or(ainfo.sample_rate);
        out_ch = o.channels.unwrap_or(ainfo.channels).clamp(1, 8) as usize;
        let br = o
            .audio_bitrate
            .unwrap_or((64_000 * out_ch as u32).min(640_000));
        aenc = Some(AudioEncoder::aac(rate, out_ch, br).map_err(err("audio encoder"))?);
        let (astart, _) = window(&inputs[ai], o);
        reader = Some(AudioReader::new(&inputs[ai].media, si, rate, astart, len));
    }
    let fps_f = f64::from(fps.0) / f64::from(fps.1);
    let frames = ((len * fps_f).round() as u64).max(1);
    ctx.info(&format!(
        "Stream mapping:\n  Stream #{vi}:0 -> #{k}:0 ({} (mkff-codecs) -> {})\n{}Output #{k}, {}, to '{}':\n  Stream #{k}:0: Video: {w}x{h}, {:.2} fps\n{}",
        v.codec.to_lowercase(),
        venc.label(),
        audio.map(|(ai, si)| format!("  Stream #{ai}:{} -> #{k}:1 ({} -> aac (mkff-aac))\n", si + 1, inputs[ai].media.info().audio[si].codec.to_lowercase())).unwrap_or_default(),
        container_label(OutKind::Movie(container)),
        o.path,
        fps_f,
        aenc.as_ref().map(|a| format!("  Stream #{k}:1: Audio: aac, {} Hz, {}\n", a.sample_rate(), layout(a.channels() as u32))).unwrap_or_default(),
    ));

    let mut writer = MediaWriter::create(&o.path, container, o.fragmented).map_err(err(&o.path))?;
    let mut vtrack: Option<TrackId> = None;
    let mut atrack: Option<TrackId> = None;
    let mut pending_v: Vec<Packet> = Vec::new();
    let mut pending_a: Vec<Packet> = Vec::new();
    // software encoders know their sample entry up front; hardware ones after the first keyframe
    let add_tracks = |writer: &mut MediaWriter,
                      venc: &VideoEnc,
                      aenc: &Option<AudioEncoder>,
                      vtrack: &mut Option<TrackId>,
                      atrack: &mut Option<TrackId>|
     -> R<bool> {
        if vtrack.is_some() {
            return Ok(true);
        }
        let id = match venc {
            VideoEnc::Sw(e) => writer.add_video_track(e).map_err(err("mux"))?,
            VideoEnc::Hw(e) => match e.avcc() {
                Some(avcc) => writer
                    .add_h264_track(avcc, w as u16, h as u16, e.timescale())
                    .map_err(err("mux"))?,
                None => return Ok(false),
            },
        };
        *vtrack = Some(id);
        if let Some(a) = aenc {
            *atrack = Some(writer.add_audio_track(a).map_err(err("mux"))?);
        }
        Ok(true)
    };
    let put = |writer: &mut MediaWriter,
               ready: bool,
               vtrack: Option<TrackId>,
               atrack: Option<TrackId>,
               vp: Vec<Packet>,
               ap: Vec<Packet>,
               pending_v: &mut Vec<Packet>,
               pending_a: &mut Vec<Packet>|
     -> R<()> {
        pending_v.extend(vp);
        pending_a.extend(ap);
        if !ready {
            return Ok(());
        }
        if let Some(t) = vtrack {
            for p in pending_v.drain(..) {
                writer.write(t, &p).map_err(err("mux"))?;
            }
        }
        if let Some(t) = atrack {
            for p in pending_a.drain(..) {
                writer.write(t, &p).map_err(err("mux"))?;
            }
        }
        Ok(())
    };

    let mut prog = Progress::new(ctx.global, &o.path);
    let mut ready = add_tracks(&mut writer, &venc, &aenc, &mut vtrack, &mut atrack)?;
    for i in 0..frames {
        let t = start + i as f64 / fps_f;
        let frame = vin.media.video_frame_at(t).map_err(err("decode video"))?;
        let rgba = frame.to_rgba8();
        let rgba = scale_rgba(
            &rgba,
            frame.width() as usize,
            frame.height() as usize,
            w as usize,
            h as usize,
        );
        let vp = venc.encode(&rgba, i)?;
        // audio up to the end of this frame
        let mut ap = Vec::new();
        if let (Some(r), Some(a)) = (reader.as_mut(), aenc.as_mut()) {
            let target = (((i + 1) as f64 / fps_f) * f64::from(r.rate)).round() as i64;
            while r.done() < target {
                let n = (target - r.done()) as usize;
                let Some(buf) = r.read(n.min(8192))? else {
                    break;
                };
                let chans = process(buf, out_ch, &o.audio_filters);
                let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
                ap.extend(a.encode(&refs).map_err(err("encode audio"))?);
            }
        }
        if !ready {
            ready = add_tracks(&mut writer, &venc, &aenc, &mut vtrack, &mut atrack)?;
        }
        put(
            &mut writer,
            ready,
            vtrack,
            atrack,
            vp,
            ap,
            &mut pending_v,
            &mut pending_a,
        )?;
        prog.update(Some(i + 1), (i + 1) as f64 / fps_f);
    }
    let vp = venc.flush()?;
    let mut ap = Vec::new();
    if let (Some(r), Some(a)) = (reader.as_mut(), aenc.as_mut()) {
        while let Some(buf) = r.read(8192)? {
            let chans = process(buf, out_ch, &o.audio_filters);
            let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
            ap.extend(a.encode(&refs).map_err(err("encode audio"))?);
        }
        ap.extend(a.flush().map_err(err("encode audio"))?);
    }
    if !ready {
        ready = add_tracks(&mut writer, &venc, &aenc, &mut vtrack, &mut atrack)?;
        if !ready {
            return Err("the hardware encoder produced no keyframe".into());
        }
    }
    put(
        &mut writer,
        ready,
        vtrack,
        atrack,
        vp,
        ap,
        &mut pending_v,
        &mut pending_a,
    )?;
    writer.finish().map_err(err(&o.path))?;
    prog.finish(Some(frames), frames as f64 / fps_f);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_51_keeps_level() {
        let n = 4;
        let ch: Vec<Vec<f32>> = (0..6)
            .map(|c| vec![if c == 0 { 1.0 } else { 0.0 }; n])
            .collect();
        let st = remix(ch.clone(), 2);
        assert_eq!(st.len(), 2);
        assert!(st[0][0] > 0.3 && st[1][0] == 0.0);
        // full-scale everywhere cannot clip
        let full: Vec<Vec<f32>> = (0..6).map(|_| vec![1.0; n]).collect();
        assert!(remix(full, 2)[0][0] <= 1.0 + 1e-6);
        let mono = remix(vec![vec![0.5; n]], 2);
        assert_eq!(mono, vec![vec![0.5; n], vec![0.5; n]]);
        assert_eq!(
            remix(vec![vec![1.0; n], vec![0.0; n]], 1),
            vec![vec![0.5; n]]
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(target_size(1920, 1080, None), (1920, 1080));
        assert_eq!(target_size(1920, 1080, Some((1280, -2))), (1280, 720));
        assert_eq!(target_size(1921, 1081, None), (1920, 1080));
        assert_eq!(target_size(64, 48, Some((-1, 24))), (32, 24));
    }

    #[test]
    fn scaler_preserves_flat_colour() {
        let src = [10u8, 20, 30, 255].repeat(8 * 6);
        let out = scale_rgba(&src, 8, 6, 3, 5);
        assert!(out.chunks(4).all(|p| p == [10, 20, 30, 255]));
    }
}
