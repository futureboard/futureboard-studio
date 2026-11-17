//! Demuxers on the fixtures in testdata/audio/format and testdata/codecs (expected values
//! cross-checked with ffprobe when the fixtures were made; see testdata/audio/format/README.md).

mod common;

use common::*;
use mkff_audio_format::{decode_pcm, probe, CodecId, PcmFormat};

struct Expect {
    format: &'static str,
    codec: CodecId,
    name: &'static str,
    rate: u32,
    channels: u32,
    packets: usize,
    /// Sum of packet durations.
    decoded: u64,
    duration: u64,
    priming: u64,
    padding: u64,
    config_len: usize,
}

fn check(path: std::path::PathBuf, track: usize, e: Expect) {
    let mut d = open(&path);
    assert_eq!(d.format_name(), e.format, "{}", path.display());
    let t = d.tracks()[track].clone();
    let what = format!("{} #{track}", path.display());
    assert_eq!(t.codec, e.codec, "{what}");
    assert_eq!(t.name, e.name, "{what}");
    assert_eq!((t.sample_rate, t.channels), (e.rate, e.channels), "{what}");
    assert_eq!(t.duration, Some(e.duration), "{what}");
    assert_eq!((t.priming, t.padding), (e.priming, e.padding), "{what}");
    assert_eq!(t.config.len(), e.config_len, "{what}");
    let pkts = read_all(d.as_mut());
    let (n, sum, first) = stats(&pkts, track);
    assert_eq!((n, sum, first), (e.packets, e.decoded, 0), "{what}");
    // timestamps are contiguous
    let mut next = 0;
    for p in pkts.iter().filter(|p| p.track == track) {
        assert_eq!(p.pts, next, "{what}");
        next += i64::from(p.duration);
    }
    // the probe agrees with open
    let head = std::fs::read(&path).unwrap();
    let head = &head[..head.len().min(64 * 1024)];
    assert_eq!(
        probe(head, &path.to_string_lossy()),
        Some(e.format),
        "{what}"
    );
}

macro_rules! expect {
    ($f:expr, $c:ident, $n:expr, $r:expr, $ch:expr, $p:expr, $dec:expr, $dur:expr, $pr:expr, $pad:expr, $cfg:expr) => {
        Expect {
            format: $f,
            codec: CodecId::$c,
            name: $n,
            rate: $r,
            channels: $ch,
            packets: $p,
            decoded: $dec,
            duration: $dur,
            priming: $pr,
            padding: $pad,
            config_len: $cfg,
        }
    };
}

#[test]
fn wav_variants() {
    check(
        fixture("s16_stereo.wav"),
        0,
        expect!("WAV", Pcm, "PCM s16le", 44100, 2, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("u8_mono.wav"),
        0,
        expect!("WAV", Pcm, "PCM u8", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("s24_51.wav"),
        0,
        expect!("WAV", Pcm, "PCM s24le", 48000, 6, 1, 2400, 2400, 0, 0, 0),
    );
    check(
        fixture("f32_mono.wav"),
        0,
        expect!("WAV", Pcm, "PCM f32le", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("f64_mono.wav"),
        0,
        expect!("WAV", Pcm, "PCM f64le", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("alaw.wav"),
        0,
        expect!("WAV", Pcm, "PCM alaw", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("mulaw.wav"),
        0,
        expect!("WAV", Pcm, "PCM mulaw", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("rf64.wav"),
        0,
        expect!("WAV", Pcm, "PCM s16le", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    let d = open(&fixture("s24_51.wav"));
    assert_eq!(d.tracks()[0].channel_mask, Some(0x3F));
    assert_eq!(d.tracks()[0].bits_per_sample, Some(24));
}

/// The same 440 Hz tone decodes to the same samples from every PCM container.
#[test]
fn pcm_containers_decode_alike() {
    let decode = |name: &str| {
        let mut d = open(&fixture(name));
        let t = d.tracks()[0].clone();
        let mut out = Vec::new();
        while let Some(p) = d.next_packet().unwrap() {
            let b = decode_pcm(&p.data, t.pcm.unwrap(), t.channels as usize);
            out.extend_from_slice(&b.channels[0]);
        }
        out
    };
    let reference = decode("f64_mono.wav");
    assert_eq!(reference.len(), 4410);
    // a 440 Hz sine at ffmpeg's default amplitude 1/8
    let peak = reference.iter().fold(0f32, |a, &b| a.max(b.abs()));
    assert!((peak - 0.125).abs() < 0.01, "{peak}");
    for (name, tol) in [
        ("f32_mono.wav", 1e-6),
        ("u8_mono.wav", 0.01),
        ("alaw.wav", 0.01),
        ("mulaw.wav", 0.01),
        ("rf64.wav", 1e-4),
        ("s24.aiff", 1e-6),
        ("f32.aifc", 1e-6),
        ("sowt.aifc", 1e-4),
        ("in24le.mov", 1e-6),
        ("fl32.mov", 1e-6),
    ] {
        let got = decode(name);
        assert_eq!(got.len(), reference.len(), "{name}");
        let err = got
            .iter()
            .zip(&reference)
            .fold(0f32, |a, (x, y)| a.max((x - y).abs()));
        assert!(err <= tol, "{name}: {err}");
    }
}

#[test]
fn aiff_variants() {
    check(
        fixture("s16.aiff"),
        0,
        expect!("AIFF", Pcm, "PCM s16be", 44100, 2, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("s24.aiff"),
        0,
        expect!("AIFF", Pcm, "PCM s24be", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("f32.aifc"),
        0,
        expect!("AIFF", Pcm, "PCM f32be", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("sowt.aifc"),
        0,
        expect!("AIFF", Pcm, "PCM s16le", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
}

#[test]
fn mpeg_audio() {
    // LAME tag: delay 576, padding 1071 -> priming 576 + 529, padding 1071 - 529
    check(
        clip("tone.mp3"),
        0,
        expect!("MP3", MpegAudio, "MP3", 44100, 1, 11, 12672, 11025, 1105, 542, 0),
    );
    // ID3v2 + ID3v1 tags, VBR with Xing TOC
    check(
        fixture("vbr.mp3"),
        0,
        expect!("MP3", MpegAudio, "MP3", 44100, 2, 40, 46080, 44100, 1105, 875, 0),
    );
    // MPEG-2 layer III without a Xing frame: every frame is audio
    check(
        fixture("mpeg2_noxing.mp3"),
        0,
        expect!("MP3", MpegAudio, "MP3", 22050, 1, 22, 12672, 12672, 0, 0, 0),
    );
    check(
        fixture("tone.mp2"),
        0,
        expect!("MP3", MpegAudio, "MP2", 48000, 1, 11, 12672, 12672, 0, 0, 0),
    );
    // packets are whole frames
    let mut d = open(&fixture("vbr.mp3"));
    for p in read_all(d.as_mut()) {
        let h = mkff_audio_format::MpegAudioHeader::parse(&p.data).unwrap();
        assert_eq!(h.frame_len, p.data.len());
    }
}

#[test]
fn adts() {
    check(
        clip("tone.aac"),
        0,
        expect!("ADTS", Aac, "AAC-LC", 48000, 1, 13, 13312, 13312, 0, 0, 2),
    );
    let d = open(&clip("tone.aac"));
    // AAC-LC, 48 kHz (index 3), mono
    assert_eq!(d.tracks()[0].config, vec![0x11, 0x88]);
}

#[test]
fn flac() {
    check(
        clip("tone.flac"),
        0,
        expect!("FLAC", Flac, "FLAC", 44100, 1, 3, 11025, 11025, 0, 0, 34),
    );
    check(
        fixture("seek.flac"),
        0,
        expect!("FLAC", Flac, "FLAC", 44100, 2, 39, 44100, 44100, 0, 0, 34),
    );
    let d = open(&fixture("seek.flac"));
    assert_eq!(d.tracks()[0].bits_per_sample, Some(16));
}

#[test]
fn ogg() {
    check(
        clip("tone.opus"),
        0,
        expect!("Ogg", Opus, "Opus", 48000, 1, 13, 12480, 12000, 312, 168, 19),
    );
    check(
        fixture("long.opus"),
        0,
        expect!("Ogg", Opus, "Opus", 48000, 1, 201, 192960, 192000, 312, 648, 19),
    );
    check(
        fixture("tone.ogg"),
        0,
        expect!("Ogg", Vorbis, "Vorbis", 44100, 1, 23, 22080, 22050, 0, 30, 3461),
    );
    check(
        fixture("long_vorbis.ogg"),
        0,
        expect!("Ogg", Vorbis, "Vorbis", 22050, 1, 131, 66560, 66150, 0, 410, 3310),
    );
    check(
        fixture("flac.oga"),
        0,
        expect!("Ogg", Flac, "FLAC", 44100, 1, 3, 11025, 11025, 0, 0, 34),
    );
    // two logical streams
    check(
        fixture("multi.ogg"),
        0,
        expect!("Ogg", Opus, "Opus", 48000, 1, 26, 24960, 24000, 312, 648, 19),
    );
    check(
        fixture("multi.ogg"),
        1,
        expect!("Ogg", Vorbis, "Vorbis", 44100, 1, 23, 22080, 22050, 0, 30, 3461),
    );
    // the Vorbis config is the three Xiph-laced headers
    let d = open(&fixture("tone.ogg"));
    let h = mkff_audio_format::xiph_unlace(&d.tracks()[0].config).unwrap();
    assert_eq!(h.len(), 3);
    assert_eq!(&h[0][..7], b"\x01vorbis");
    assert_eq!(&h[1][..7], b"\x03vorbis");
    assert_eq!(&h[2][..7], b"\x05vorbis");
    let d = open(&clip("tone.opus"));
    assert_eq!(&d.tracks()[0].config[..8], b"OpusHead");
}

#[test]
fn mp4_tracks() {
    check(
        fixture("aac.m4a"),
        0,
        expect!("MP4", Aac, "AAC-LC", 44100, 1, 12, 12049, 11025, 1024, 0, 5),
    );
    check(
        fixture("aac_frag.mp4"),
        0,
        expect!("MP4", Aac, "AAC-LC", 44100, 1, 12, 12049, 12049, 0, 0, 5),
    );
    check(
        fixture("flac.mp4"),
        0,
        expect!("MP4", Flac, "FLAC", 44100, 1, 3, 11025, 11025, 0, 0, 34),
    );
    check(
        fixture("opus.mp4"),
        0,
        expect!("MP4", Opus, "Opus", 48000, 1, 13, 12312, 12000, 312, 0, 19),
    );
    check(
        fixture("mp3.mp4"),
        0,
        expect!("MP4", MpegAudio, "MP3", 44100, 1, 11, 12130, 11025, 1105, 0, 0),
    );
    check(
        fixture("sowt.mov"),
        0,
        expect!("MP4", Pcm, "PCM s16le", 44100, 2, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("in24le.mov"),
        0,
        expect!("MP4", Pcm, "PCM s24le", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        fixture("fl32.mov"),
        0,
        expect!("MP4", Pcm, "PCM f32be", 44100, 1, 2, 4410, 4410, 0, 0, 0),
    );
    check(
        clip("tone_alac.m4a"),
        0,
        expect!("MP4", Alac, "ALAC", 44100, 1, 3, 11025, 11025, 0, 0, 24),
    );
    // AC-3 / E-AC-3: the real channel count from dac3 / dec3
    check(
        clip("tone51_ac3.mp4"),
        0,
        expect!("MP4", Ac3, "AC-3", 48000, 6, 8, 12000, 11712, 256, 32, 3),
    );
    check(
        clip("tone51_eac3.mp4"),
        0,
        expect!("MP4", Ac3, "E-AC-3", 48000, 6, 8, 12000, 11712, 256, 32, 6),
    );
    // audio next to a video track
    check(
        clip("av1_aac.mp4"),
        0,
        expect!("MP4", Aac, "AAC-LC", 48000, 1, 13, 13024, 12000, 1024, 0, 5),
    );
    check(
        clip("prores_hq.mov"),
        0,
        expect!("MP4", Pcm, "PCM s16le", 48000, 1, 6, 12000, 12000, 0, 0, 0),
    );
    let d = open(&clip("tone_alac.m4a"));
    assert_eq!(d.tracks()[0].bits_per_sample, Some(16));
    let d = open(&fixture("opus.mp4"));
    assert_eq!(&d.tracks()[0].config[..8], b"OpusHead");
}

/// After a seek, the next packet of the track starts at or before the target, its pts is what
/// `seek` returned, and it is the same packet a linear read gives.
fn check_seek(path: std::path::PathBuf, track: usize, exact: bool) {
    let mut d = open(&path);
    let info = d.tracks()[track].clone();
    let all: Vec<_> = read_all(d.as_mut())
        .into_iter()
        .filter(|p| p.track == track)
        .collect();
    let end = all.last().map(|p| p.pts + i64::from(p.duration)).unwrap();
    let pcm: Vec<u8> = all.iter().flat_map(|p| p.data.clone()).collect();
    let mut targets: Vec<i64> = vec![
        0,
        1,
        end / 3,
        end / 2,
        end - 1,
        all[all.len() / 2].pts,
        end + 1000,
    ];
    targets.extend(
        all.iter()
            .step_by(7)
            .map(|p| p.pts + i64::from(p.duration) / 2),
    );
    for t in targets {
        let got = d.seek(track, t).unwrap();
        let next = loop {
            match d.next_packet().unwrap() {
                Some(p) if p.track != track => continue,
                other => break other,
            }
        };
        let what = format!("{} seek {t}", path.display());
        let Some(p) = next else {
            assert!(
                t >= all.last().unwrap().pts,
                "{what}: nothing after the seek"
            );
            continue;
        };
        assert_eq!(p.pts, got, "{what}");
        assert!(p.pts <= t.max(0), "{what}: landed at {}", p.pts);
        if let Some(fmt) = info.pcm {
            // PCM lands on the exact frame, with the same bytes as a linear read
            assert_eq!(p.pts, t.clamp(0, end), "{what}");
            let at = p.pts as usize * fmt.bytes_per_sample() * info.channels as usize;
            assert_eq!(&pcm[at..at + p.data.len()], &p.data[..], "{what}");
            continue;
        }
        let r = all
            .iter()
            .find(|r| r.pts == p.pts)
            .unwrap_or_else(|| panic!("{what}: pts {} not in the stream", p.pts));
        assert_eq!(r.data, p.data, "{what}");
        if exact && t < end {
            // the packet holding the target
            let holder = all.iter().rev().find(|r| r.pts <= t.max(0)).unwrap();
            assert_eq!(p.pts, holder.pts, "{what}");
        }
    }
}

#[test]
fn seeking() {
    for (p, exact) in [
        (fixture("s16_stereo.wav"), true),
        (fixture("rf64.wav"), true),
        (fixture("s24.aiff"), true),
        (fixture("vbr.mp3"), true),
        (clip("tone.mp3"), true),
        (fixture("tone.mp2"), true),
        (clip("tone.aac"), true),
        (fixture("seek.flac"), true),
        (clip("tone.flac"), true),
        (fixture("long.opus"), false),
        (fixture("long_vorbis.ogg"), false),
        (fixture("flac.oga"), false),
        (fixture("aac.m4a"), true),
        (fixture("aac_frag.mp4"), true),
        (fixture("sowt.mov"), true),
        (clip("tone51_eac3.mp4"), true),
        (clip("prores_hq.mov"), true),
    ] {
        check_seek(p, 0, exact);
    }
    check_seek(fixture("multi.ogg"), 1, false);
    check_seek(fixture("multi.ogg"), 0, false);
}

#[test]
fn pcm_seek_is_sample_exact() {
    let mut d = open(&fixture("s16_stereo.wav"));
    assert_eq!(d.seek(0, 1234).unwrap(), 1234);
    let p = d.next_packet().unwrap().unwrap();
    assert_eq!(p.pts, 1234);
    let mut d = open(&fixture("sowt.mov"));
    assert_eq!(d.seek(0, 1234).unwrap(), 1234);
    assert_eq!(d.next_packet().unwrap().unwrap().pts, 1234);
}

#[test]
fn ogg_seek_lands_on_the_page_before() {
    let mut d = open(&fixture("long.opus"));
    for t in [48_000i64, 100_000, 150_000] {
        let got = d.seek(0, t).unwrap();
        // pages are ~0.5 s apart
        assert!(got <= t && t - got < 48_000, "{t} {got}");
    }
}

#[test]
fn probe_by_content_not_name() {
    let wav = std::fs::read(fixture("s16_stereo.wav")).unwrap();
    assert_eq!(probe(&wav, "song.mp3"), Some("WAV"));
    let mp3 = std::fs::read(clip("tone.mp3")).unwrap();
    assert_eq!(probe(&mp3, "x.bin"), Some("MP3"));
    let mut d = mkff_audio_format::open(Box::new(std::io::Cursor::new(mp3)), "").unwrap();
    assert_eq!(d.format_name(), "MP3");
    assert!(d.next_packet().unwrap().is_some());
    let _ = PcmFormat::s16le();
}
