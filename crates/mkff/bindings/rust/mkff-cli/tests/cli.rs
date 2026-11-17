//! Runs the `mkfx` binary on the checked-in fixtures and checks the files it writes with MKFF's
//! own readers.

use std::path::{Path, PathBuf};
use std::process::Command;

use mkff_audio::AudioFile;
use mkff_codecs::MediaFile;

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/codecs")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("mkfx-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run mkfx; returns (success, stderr).
fn mkfx(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mkfx"))
        .args(["-hide_banner", "-nostats"])
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout),
    )
}

fn ok(args: &[&str]) -> String {
    let (good, log) = mkfx(args);
    assert!(good, "mkfx {args:?} failed:\n{log}");
    log
}

fn pitch(x: &[f32], rate: u32) -> f64 {
    let crossings = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f64 * f64::from(rate) / x.len() as f64
}

/// Power of `freq` in `x` at 44.1 kHz (Goertzel).
fn power(x: &[f32], freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / 44_100.0;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = f64::from(v) + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2
}

#[test]
fn probe_describes_streams() {
    let (good, log) = mkfx(&["-i", &fixture("tone51_ac3.mp4")]);
    assert!(!good, "probe without output exits 1, like FFmpeg");
    assert!(log.contains("Input #0, mpeg-4"), "{log}");
    assert!(log.contains("Audio: ac-3, 48000 Hz, 5.1"), "{log}");
    assert!(log.contains("decoder: mkff-audio"), "{log}");
    assert!(log.contains("At least one output file must be specified"));
    let (_, log) = mkfx(&["-i", &fixture("prores_hq.mov")]);
    assert!(log.contains("Video: apple prores"), "{log}");
}

#[test]
fn flac_to_wav_is_lossless_and_trims_exactly() {
    let t = Tmp::new("wav");
    let out = t.path("a.wav");
    ok(&["-i", &fixture("tone.flac"), &out]);
    let src = AudioFile::open(fixture("tone.flac"))
        .unwrap()
        .read_to_end()
        .unwrap();
    let got = AudioFile::open(&out).unwrap().read_to_end().unwrap();
    assert_eq!(got.frames(), src.frames());
    assert_eq!(
        got.channels[0], src.channels[0],
        "16-bit FLAC → pcm_s16le must be exact"
    );
    // input seek + duration: exactly 0.1 s from 0.05 s
    let cut = t.path("cut.wav");
    ok(&[
        "-ss",
        "0.05",
        "-i",
        &fixture("tone.flac"),
        "-t",
        "100ms",
        &cut,
    ]);
    let got = AudioFile::open(&cut).unwrap().read_to_end().unwrap();
    assert_eq!(got.frames(), 4410);
    let off = (0.05f64 * 44_100.0).round() as usize;
    assert_eq!(got.channels[0][..], src.channels[0][off..off + 4410]);
    // output -ss / -to on the output timeline
    let cut2 = t.path("cut2.wav");
    ok(&[
        "-i",
        &fixture("tone.flac"),
        "-ss",
        "0.1",
        "-to",
        "0.15",
        &cut2,
    ]);
    assert_eq!(
        AudioFile::open(&cut2)
            .unwrap()
            .read_to_end()
            .unwrap()
            .frames(),
        2205
    );
}

#[test]
fn encodes_mp3_aac_flac_alac_gaplessly() {
    let t = Tmp::new("enc");
    let src = AudioFile::open(fixture("tone.flac"))
        .unwrap()
        .read_to_end()
        .unwrap();
    for (name, extra) in [
        ("o.mp3", vec!["-b:a", "128k"]),
        ("o.m4a", vec!["-c:a", "aac", "-b:a", "96k"]),
        ("alac.m4a", vec!["-c:a", "alac"]),
        ("o.flac", vec!["-compression_level", "8"]),
        ("o.aac", vec![]),
    ] {
        let out = t.path(name);
        let mut args = vec!["-i".to_string(), fixture("tone.flac")];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.push(out.clone());
        ok(&args.iter().map(String::as_str).collect::<Vec<_>>());
        let mut f = AudioFile::open(&out).unwrap();
        let got = f.read_to_end().unwrap();
        // ADTS cannot carry the encoder delay; everything else is gapless
        if !name.ends_with(".aac") {
            assert_eq!(got.frames(), src.frames(), "{name}: length");
        }
        let p = pitch(&got.channels[0][2000..9000], f.sample_rate());
        assert!((p - 440.0).abs() < 10.0, "{name}: {p} Hz");
    }
}

#[test]
fn resample_remix_and_volume() {
    let t = Tmp::new("mix");
    let out = t.path("o.wav");
    let log = ok(&[
        "-i",
        &fixture("tone51_eac3.mp4"),
        "-ar",
        "44100",
        "-ac",
        "2",
        "-af",
        "volume=0.5",
        "-c:a",
        "pcm_f32le",
        &out,
    ]);
    assert!(log.contains("44100 Hz, stereo"), "{log}");
    let got = AudioFile::open(&out).unwrap().read_to_end().unwrap();
    assert_eq!((got.sample_rate, got.channel_count()), (44_100, 2));
    // FL's 300 Hz lands only on the left, FR's 500 Hz only on the right
    let (l, r) = (&got.channels[0][2000..8000], &got.channels[1][2000..8000]);
    assert!(power(l, 300.0) > 100.0 * power(l, 500.0), "left has FR");
    assert!(power(r, 500.0) > 100.0 * power(r, 300.0), "right has FL");
    // the centre (700 Hz) goes to both
    assert!(power(l, 700.0) > 0.1 * power(l, 300.0) && power(r, 700.0) > 0.1 * power(r, 500.0));
    let peak = got.channels[0].iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(peak < 0.5, "volume=0.5 on a 0.3 tone downmix: peak {peak}");
}

#[test]
fn stream_copy_remuxes_packets() {
    use mkff_audio::format::open_path;
    let t = Tmp::new("copy");
    for (src, out) in [
        ("tone.aac", "a.m4a"),
        ("tone.mp3", "b.mp3"),
        ("tone.flac", "c.flac"),
        ("tone.opus", "d.opus"),
        ("tone_alac.m4a", "e.mp4"),
    ] {
        let o = t.path(out);
        let log = ok(&["-i", &fixture(src), "-c:a", "copy", &o]);
        assert!(log.contains("(copy)"), "{log}");
        let count = |p: &str| {
            let mut d = open_path(p).unwrap();
            let mut n = 0;
            while d.next_packet().unwrap().is_some() {
                n += 1;
            }
            n
        };
        assert_eq!(count(&o), count(&fixture(src)), "{src} → {out}");
        // and decodes to the same, gapless length
        let len = |p: &str| AudioFile::open(p).unwrap().read_to_end().unwrap().frames();
        assert_eq!(len(&o), len(&fixture(src)), "{src} → {out}: decoded length");
    }
    // codecs that don't fit the container are refused
    let (good, log) = mkfx(&[
        "-i",
        &fixture("tone.mp3"),
        "-c:a",
        "copy",
        &t.path("x.flac"),
    ]);
    assert!(!good && log.contains("cannot be copied"), "{log}");
    assert!(
        !Path::new(&t.path("x.flac")).exists(),
        "failed outputs are removed"
    );
}

#[test]
fn video_transcodes() {
    let t = Tmp::new("video");
    // ProRes + PCM → H.264 + AAC, scaled and at another frame rate
    let out = t.path("o.mp4");
    let log = ok(&[
        "-i",
        &fixture("prores_hq.mov"),
        "-vf",
        "scale=32:-2",
        "-r",
        "30",
        "-crf",
        "20",
        &out,
    ]);
    assert!(log.contains("-> h264 (mkff-codecs)"), "{log}");
    let m = MediaFile::open(&out).unwrap();
    let v = m.info().video.clone().unwrap();
    assert_eq!((v.width, v.height), (32, 24));
    assert_eq!(v.frame_rate.0, v.frame_rate.1 * 30);
    assert_eq!(m.info().audio.len(), 1);
    assert!(m.audio_is_native(0));
    assert!(m.frame_count() >= 7, "{} frames", m.frame_count());
    // the picture survives: the decoded frame is not flat
    let rgba = m.video_frame(3).unwrap().to_rgba8();
    let (lo, hi) = rgba
        .iter()
        .step_by(4)
        .fold((255u8, 0u8), |(a, b), &x| (a.min(x), b.max(x)));
    assert!(hi > lo + 20, "flat picture {lo}..{hi}");
    // → ProRes in MOV, video only
    let mov = t.path("p.mov");
    ok(&[
        "-i",
        &out,
        "-c:v",
        "prores",
        "-profile:v",
        "proxy",
        "-an",
        &mov,
    ]);
    let p = MediaFile::open(&mov).unwrap();
    assert!(p
        .info()
        .video
        .as_ref()
        .unwrap()
        .codec
        .to_lowercase()
        .contains("prores"));
    assert!(p.info().audio.is_empty());
    // ProRes is refused in MP4
    let (good, log) = mkfx(&["-i", &out, "-c:v", "prores", &t.path("bad.mp4")]);
    assert!(!good && log.contains(".mov"), "{log}");
}

#[test]
fn gpu_encode_when_available() {
    if mkff_hwenc::h264_encoders().is_empty() {
        eprintln!("skipping: no hardware H.264 encoder");
        return;
    }
    let t = Tmp::new("gpu");
    let out = t.path("hw.mp4");
    let log = ok(&[
        "-i",
        &fixture("prores_hq.mov"),
        "-c:v",
        "h264_hw",
        "-b:v",
        "1M",
        "-vf",
        "scale=128:96",
        &out,
    ]);
    assert!(log.contains("-> h264 ("), "{log}");
    let m = MediaFile::open(&out).unwrap();
    assert_eq!(
        m.info().video.as_ref().map(|v| (v.width, v.height)),
        Some((128, 96))
    );
    assert!(m.video_frame(2).is_ok());
}

#[test]
fn refuses_what_it_should() {
    let t = Tmp::new("err");
    let out = t.path("keep.wav");
    std::fs::write(&out, b"precious").unwrap();
    let (good, log) = mkfx(&["-i", &fixture("tone.flac"), &out]);
    assert!(!good && log.contains("already exists"), "{log}");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"precious",
        "existing file untouched"
    );
    let (good, _) = mkfx(&["-n", "-i", &fixture("tone.flac"), &out]);
    assert!(!good);
    ok(&["-y", "-i", &fixture("tone.flac"), &out]);
    assert_ne!(std::fs::read(&out).unwrap(), b"precious");

    let (good, log) = mkfx(&["-frobnicate", "1"]);
    assert!(!good && log.contains("Unrecognized option"), "{log}");
    let (good, log) = mkfx(&[
        "-i",
        &fixture("tone.flac"),
        "-c:a",
        "libvorbis",
        &t.path("x.ogg"),
    ]);
    assert!(!good && log.contains("no encoder yet"), "{log}");
    let (good, log) = mkfx(&["-i", &fixture("tone.flac"), &t.path("x.xyz")]);
    assert!(!good && log.contains("not supported"), "{log}");
    let (good, log) = mkfx(&["-i", "definitely-missing.wav", &t.path("y.wav")]);
    assert!(!good && log.contains("definitely-missing.wav"), "{log}");
}

#[test]
fn listings() {
    for q in [
        "-codecs",
        "-encoders",
        "-decoders",
        "-formats",
        "-hwaccels",
        "-h",
        "-version",
    ] {
        let log = ok(&[q]);
        assert!(!log.is_empty(), "{q}");
    }
    assert!(ok(&["-codecs"]).contains("mp3"));
    assert!(ok(&["-hwaccels"]).contains("accelerant"));
}
