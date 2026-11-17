//! Banner, help, input description and the `-codecs` / `-formats` / `-hwaccels` listings.

use mkff_codecs::{MediaFile, MediaKind};

use crate::units::{fmt_time, layout};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn banner() -> String {
    format!(
        "mkfx version {VERSION} — MediaKit Foundation media tool (no FFmpeg inside)\n  \
         audio: mkff-audio (own codecs) · video: mkff-codecs · GPU: mkff-mediakit accelerants\n"
    )
}

pub fn help() -> String {
    "usage: mkfx [options] [[infile options] -i infile]... {[outfile options] outfile}...

Getting help / information:
  -h                  this help              -version        show version
  -codecs  -encoders  -decoders  -formats    -hwaccels       list what MKFF can do

Global options:
  -y / -n             overwrite / never overwrite output files
  -hide_banner        -v quiet|error|warning|info|verbose      -stats / -nostats

Input / output timing (before -i: seek the input; before an output: trim it):
  -ss POS             start at POS ([HH:]MM:SS[.ms] or seconds)
  -t DUR / -to POS    stop after DUR / at POS

Output options:
  -f FMT              wav, mp3, adts, flac, ipod/m4a, mp4, mov, ogg (default: from extension)
  -map IN[:a|v][:N]   pick streams (default: first video + first audio)
  -an / -vn           drop audio / video
  -c:a CODEC          aac, mp3, flac, alac, pcm_s16le, pcm_s24le, pcm_f32le, copy
  -b:a RATE  -q:a N   audio bitrate (e.g. 192k) / MP3 quality 0 (best) - 9
  -ar HZ  -ac N       resample / remix (1-8 channels)
  -af volume=X        gain (linear or dB, e.g. volume=-6dB)
  -compression_level N   FLAC level 0-8
  -c:v CODEC          h264 (software), h264_hw / h264_nvenc / h264_qsv / h264_amf (GPU),
                      prores (-profile:v proxy|lt|standard|hq|4444|4444xq)
  -b:v RATE  -crf N   video bitrate / constant quality (H.264)
  -preset P  -g N  -bf N    speed (ultrafast..veryslow), keyframe interval, B-frames
  -r FPS  -s WxH  -vf scale=W:H   frame rate / size (-1 or -2 keeps the aspect ratio)
  -movflags +frag_keyframe        fragmented MP4 (crash-safe recording)

Probe a file:  mkfx -i input
Examples:
  mkfx -i song.flac -c:a mp3 -b:a 192k song.mp3
  mkfx -i clip.mov -c:v h264_hw -b:v 6M -c:a aac -b:a 160k clip.mp4
  mkfx -ss 30 -i talk.m4a -t 60 -ac 1 -ar 16000 excerpt.wav
  mkfx -i song.m4a -c:a copy song.mp4
"
    .into()
}

fn channels_desc(c: u32) -> String {
    layout(c)
}

/// FFmpeg-style description of an opened input.
pub fn describe_input(index: usize, path: &str, m: &MediaFile) -> String {
    let i = m.info();
    let mut s = format!(
        "Input #{index}, {}, from '{path}':\n",
        i.container.to_lowercase()
    );
    let bitrate = match (i.file_size, i.duration_seconds) {
        (Some(sz), d) if d > 0.0 => {
            format!("{} kb/s", ((sz as f64 * 8.0 / d) / 1000.0).round() as u64)
        }
        _ => "N/A".into(),
    };
    s += &format!(
        "  Duration: {}, bitrate: {bitrate}\n",
        if i.duration_seconds > 0.0 {
            fmt_time(i.duration_seconds)
        } else {
            "N/A".into()
        }
    );
    let mut n = 0;
    if let Some(v) = &i.video {
        let fps = if v.frame_rate.1 != 0 {
            v.frame_rate.0 as f64 / v.frame_rate.1 as f64
        } else {
            0.0
        };
        let kind = if i.kind == MediaKind::Still {
            "Video (still)"
        } else {
            "Video"
        };
        s += &format!(
            "  Stream #{index}:{n}: {kind}: {}, {}, {}x{}{}{}, {:.2} fps (decoder: mkff-codecs)\n",
            v.codec.to_lowercase(),
            v.pixel_format,
            v.width,
            v.height,
            if v.pixel_aspect != (1, 1) && v.pixel_aspect.1 != 0 {
                format!(" [SAR {}:{}]", v.pixel_aspect.0, v.pixel_aspect.1)
            } else {
                String::new()
            },
            if v.has_alpha { ", alpha" } else { "" },
            fps
        );
        n += 1;
    }
    for (k, a) in i.audio.iter().enumerate() {
        s += &format!(
            "  Stream #{index}:{n}: Audio: {}, {} Hz, {}, fltp{} (decoder: {})\n",
            a.codec.to_lowercase(),
            a.sample_rate,
            channels_desc(a.channels),
            a.bits_per_sample
                .map(|b| format!(", {b} bit"))
                .unwrap_or_default(),
            if m.audio_is_native(k) {
                "mkff-audio"
            } else {
                "filmcraft"
            }
        );
        n += 1;
    }
    s
}

/// (flags, name, description). Flags: D decode, E encode, A/V kind, H hardware available.
const CODECS: &[(&str, &str, &str)] = &[
    (
        "DEA",
        "aac",
        "AAC-LC (mkff-aac; HE-AAC decodes its LC core)",
    ),
    ("DEA", "mp3", "MPEG audio layer III (mkff-mp3)"),
    ("D.A", "mp2", "MPEG audio layer II (mkff-mp3)"),
    ("D.A", "mp1", "MPEG audio layer I (mkff-mp3)"),
    ("DEA", "flac", "FLAC (mkff-flac)"),
    ("DEA", "alac", "Apple Lossless (mkff-alac)"),
    ("D.A", "ac3", "Dolby Digital (mkff-ac3)"),
    ("D.A", "eac3", "Dolby Digital Plus (mkff-ac3)"),
    ("D.A", "opus", "Opus (mkff-opus)"),
    ("D.A", "vorbis", "Vorbis (mkff-vorbis)"),
    (
        "DEA",
        "pcm",
        "PCM s16 / s24 / s32 / f32 / f64, A-law, mu-law (mkff-audio-format)",
    ),
    (
        "DEV",
        "h264",
        "H.264 / AVC (software; GPU decode + encode via accelerants)",
    ),
    (
        "D.V",
        "hevc",
        "H.265 / HEVC Main / Main10 (GPU decode via accelerants)",
    ),
    ("D.V", "vp8", "VP8"),
    ("D.V", "vp9", "VP9 profiles 0-3"),
    ("D.V", "av1", "AV1"),
    ("D.V", "mpeg2video", "MPEG-1 / MPEG-2 video"),
    ("DEV", "prores", "Apple ProRes 422 / 4444"),
    ("D.V", "dnxhd", "Avid DNxHD / DNxHR"),
    ("D.V", "apv", "Advanced Professional Video"),
    ("D.V", "mjpeg", "Motion JPEG"),
];

pub fn codecs() -> String {
    let mut s = String::from("Codecs:\n D..... = Decoding supported\n .E.... = Encoding supported\n ..A... / ..V... = Audio / Video\n -------\n");
    for (f, n, d) in CODECS {
        s += &format!(" {f}... {n:<12} {d}\n");
    }
    s
}

pub fn encoders() -> String {
    let mut s = String::from("Encoders:\n");
    for (n, d) in [
        ("aac", "AAC-LC, 1-8 channels (mkff-aac)"),
        ("mp3", "MP3 layer III CBR (mkff-mp3); alias libmp3lame"),
        ("flac", "FLAC levels 0-8 (mkff-flac)"),
        ("alac", "Apple Lossless (mkff-alac)"),
        ("pcm_s16le / pcm_s24le / pcm_f32le", "PCM"),
        ("h264", "H.264 software encoder; alias libx264"),
        ("prores", "ProRes 422 / 4444; alias prores_ks"),
    ] {
        s += &format!(" {n:<34} {d}\n");
    }
    for e in mkff_hwenc::h264_encoders() {
        s += &format!(
            " {:<34} {} — {}\n",
            hw_name(e.vendor),
            e.vendor.api_name(),
            e.name
        );
    }
    s
}

pub fn hw_name(v: mkff_hwenc::Vendor) -> &'static str {
    match v {
        mkff_hwenc::Vendor::Nvidia => "h264_nvenc",
        mkff_hwenc::Vendor::Intel => "h264_qsv",
        mkff_hwenc::Vendor::Amd => "h264_amf",
        mkff_hwenc::Vendor::Other => "h264_hw",
    }
}

pub fn decoders() -> String {
    let mut s = String::from("Decoders:\n");
    for (f, n, d) in CODECS.iter().filter(|c| c.0.starts_with('D')) {
        let _ = f;
        s += &format!(" {n:<12} {d}\n");
    }
    s
}

pub fn formats() -> String {
    let mut s =
        String::from("File formats:\n D. = Demuxing supported\n .E = Muxing supported\n --\n");
    for (f, n, d) in [
        ("DE", "wav", "WAV / RF64 / BW64 (mkff-audio-format)"),
        ("D ", "aiff", "AIFF / AIFF-C (mkff-audio-format)"),
        (
            "DE",
            "mp3",
            "MPEG audio, ID3, Xing / LAME gapless (mkff-audio-format)",
        ),
        ("DE", "adts", "raw AAC (mkff-audio-format)"),
        ("DE", "flac", "native FLAC (mkff-audio-format)"),
        (
            "DE",
            "ogg",
            "Ogg Vorbis / Opus / FLAC (mkff-audio-format; mux: Opus copy)",
        ),
        (
            "DE",
            "ipod",
            "M4A: AAC / ALAC / FLAC / MP3 / PCM (mkff-audio-format)",
        ),
        (
            "DE",
            "mp4",
            "MP4: H.264 + AAC (mkff-codecs), audio-only M4A layout",
        ),
        ("DE", "mov", "QuickTime: H.264 / ProRes + AAC (mkff-codecs)"),
        ("D ", "matroska,webm", "Matroska / WebM (mkff-codecs)"),
        ("D ", "avi", "AVI / OpenDML (mkff-codecs)"),
        ("D ", "mxf", "MXF OP1a / OP-Atom (mkff-codecs)"),
        ("D ", "mpegts", "MPEG-TS / M2TS (mkff-codecs)"),
        ("D ", "mpeg", "MPEG-PS / VOB (mkff-codecs)"),
    ] {
        s += &format!(" {f} {n:<14} {d}\n");
    }
    s
}

/// The Media Kit's accelerants (one per GPU) and the hardware encoders.
pub fn hwaccels() -> String {
    let roster = mkff_mediakit::MediaRoster::roster();
    let accels = roster.accelerants();
    let mut s =
        String::from("Hardware acceleration (MKFF accelerants, BeOS-style: one per GPU):\n");
    if accels.is_empty() {
        s += "  none found (decode and encode run in software)\n";
    } else {
        for line in mkff_mediakit::accelerant::describe(accels).lines() {
            s += &format!("  {line}\n");
        }
    }
    let enc = mkff_hwenc::h264_encoders();
    if !enc.is_empty() {
        s += "Hardware encoders (-c:v):\n";
        for e in enc {
            s += &format!(
                "  {:<11} {} — {}\n",
                hw_name(e.vendor),
                e.vendor.api_name(),
                e.name
            );
        }
    }
    s
}
