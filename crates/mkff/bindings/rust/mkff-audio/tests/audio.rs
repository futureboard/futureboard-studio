//! End to end: files → AudioFile, encoders → AudioWriter → AudioFile, and the Media Kit add-on.

use std::path::PathBuf;

use mkff_audio::{AudioFile, AudioWriter, CodecId, EncoderSettings};

fn codecs_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/codecs")
        .join(name)
}

/// Frequency of a (mostly) pure tone from its zero crossings.
fn pitch(x: &[f32], rate: u32) -> f64 {
    let crossings = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f64 * f64::from(rate) / x.len() as f64
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

#[test]
fn every_fixture_decodes_to_its_tone() {
    // (file, rate, channels, expected samples after trimming (None: not signalled), tolerance)
    let cases: &[(&str, u32, usize, Option<u64>)] = &[
        ("tone.mp3", 44_100, 1, Some(11_025)),
        ("tone.flac", 44_100, 1, Some(11_025)),
        ("tone_alac.m4a", 44_100, 1, Some(11_025)),
        ("tone.opus", 48_000, 1, Some(12_000)),
        ("tone.aac", 48_000, 1, None),
        ("tone51_ac3.mp4", 48_000, 6, None),
        ("tone51_eac3.mp4", 48_000, 6, None),
    ];
    for &(name, rate, ch, len) in cases {
        let mut f = AudioFile::open(codecs_dir(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((f.sample_rate(), f.channels()), (rate, ch), "{name}");
        let all = f.read_to_end().unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Some(n) = len {
            assert_eq!(all.frames() as u64, n, "{name}: gapless length");
            assert_eq!(f.duration(), Some(n), "{name}");
        } else {
            assert!(
                all.frames() as f64 >= f64::from(rate) * 0.24,
                "{name}: {} samples",
                all.frames()
            );
        }
        // the middle of channel 0 is the tone (440 Hz; 300 Hz on FL of the 5.1 clips)
        let want = if ch == 6 { 300.0 } else { 440.0 };
        let mid = &all.channels[0][all.frames() / 5..all.frames() * 4 / 5];
        let p = pitch(mid, rate);
        assert!((p - want).abs() < 15.0, "{name}: {p:.1} Hz");
        assert!(rms(mid) > 0.03, "{name}: rms {}", rms(mid));
    }
}

#[test]
fn surround_channels_keep_their_tones() {
    for name in ["tone51_ac3.mp4", "tone51_eac3.mp4"] {
        let mut f = AudioFile::open(codecs_dir(name)).unwrap();
        let all = f.read_to_end().unwrap();
        // WAV order: FL FR FC LFE SL SR
        for (c, want) in [300.0, 500.0, 700.0, 60.0, 900.0, 1100.0]
            .iter()
            .enumerate()
        {
            let mid = &all.channels[c][all.frames() / 5..all.frames() * 4 / 5];
            let p = pitch(mid, 48_000);
            assert!(
                (p - want).abs() < want * 0.08 + 8.0,
                "{name} ch{c}: {p:.1} Hz, want {want}"
            );
        }
    }
}

#[test]
fn seek_matches_linear_decode() {
    for name in [
        "tone.mp3",
        "tone.flac",
        "tone_alac.m4a",
        "tone.opus",
        "tone.aac",
        "tone51_ac3.mp4",
    ] {
        let mut f = AudioFile::open(codecs_dir(name)).unwrap();
        let all = f.read_to_end().unwrap();
        for pos in [0usize, 1, 1000, 4097, all.frames() / 2, all.frames() - 10] {
            let mut g = AudioFile::open(codecs_dir(name)).unwrap();
            g.seek(pos as u64).unwrap();
            let rest = g.read_to_end().unwrap();
            assert_eq!(rest.frames(), all.frames() - pos, "{name} @ {pos}: length");
            let worst = rest.channels[0]
                .iter()
                .zip(&all.channels[0][pos..])
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            // lossless: exact; lossy: the pre-roll converges to the same output (AC-3 adds
            // random dither to zero-bit mantissas, as the spec asks, so it differs by the dither)
            let tol = match f.codec() {
                CodecId::Flac | CodecId::Alac => 0.0,
                CodecId::Ac3 => 1e-2,
                _ => 2e-3,
            };
            assert!(worst <= tol, "{name} @ {pos}: max diff {worst}");
        }
    }
}

#[test]
fn random_access_reads_resample() {
    let mut f = AudioFile::open(codecs_dir("tone.flac")).unwrap();
    let a = f.read(1000, 2000, 0).unwrap();
    let b = f.read(500, 4000, 0).unwrap();
    assert_eq!(a.channels[0][..], b.channels[0][500..2500]);
    // past the end and before the start are silence
    let edge = f.read(-100, 200, 0).unwrap();
    assert!(edge.channels[0][..100].iter().all(|v| *v == 0.0));
    assert!(edge.channels[0][100..].iter().any(|v| *v != 0.0));
    let tail = f.read(11_000, 100, 0).unwrap();
    assert!(tail.channels[0][25..].iter().all(|v| *v == 0.0));
    // resampled to 48 kHz the tone is still 440 Hz
    let r = f.read(0, 12_000, 48_000).unwrap();
    assert_eq!(r.sample_rate, 48_000);
    let p = pitch(&r.channels[0][2000..10_000], 48_000);
    assert!((p - 440.0).abs() < 8.0, "{p}");
}

fn sine(rate: u32, freq: f32, n: usize, phase: f32) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 * (std::f32::consts::TAU * freq * i as f32 / rate as f32 + phase).sin())
        .collect()
}

/// SNR of `got` against `want` (aligned, same length).
fn snr(want: &[f32], got: &[f32]) -> f64 {
    let s: f64 = want.iter().map(|v| f64::from(*v).powi(2)).sum();
    let n: f64 = want
        .iter()
        .zip(got)
        .map(|(a, b)| f64::from(a - b).powi(2))
        .sum();
    10.0 * (s / n.max(1e-30)).log10()
}

#[test]
fn encode_to_every_container_and_back() {
    let dir = std::env::temp_dir().join(format!("mkff-audio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rate = 44_100;
    let n = rate as usize * 2 + 123;
    let l = sine(rate, 440.0, n, 0.0);
    let r = sine(rate, 660.0, n, 1.0);
    // (file, codec, bits, exact length expected, minimum SNR)
    let cases: &[(&str, CodecId, u32, bool, f64)] = &[
        ("out.wav", CodecId::Pcm, 24, true, 100.0),
        ("out.flac", CodecId::Flac, 24, true, 100.0),
        ("alac.m4a", CodecId::Alac, 24, true, 100.0),
        ("pcm.m4a", CodecId::Pcm, 16, true, 80.0),
        ("out.mp3", CodecId::MpegAudio, 0, true, 15.0),
        ("mp3.m4a", CodecId::MpegAudio, 0, true, 15.0),
        ("aac.m4a", CodecId::Aac, 0, true, 15.0),
        ("out.aac", CodecId::Aac, 0, false, 0.0),
    ];
    for &(file, codec, bits, exact, min_snr) in cases {
        let path = dir.join(file);
        let s = EncoderSettings::new(codec, rate, 2)
            .with_bitrate(192_000)
            .with_bits(bits);
        let mut w = AudioWriter::create(&path, s).unwrap_or_else(|e| panic!("{file}: {e}"));
        for (a, b) in l.chunks(10_000).zip(r.chunks(10_000)) {
            w.write(&[a, b]).unwrap();
        }
        w.finish().unwrap_or_else(|e| panic!("{file}: {e}"));
        let mut f = AudioFile::open(&path).unwrap_or_else(|e| panic!("{file}: {e}"));
        let back = f.read_to_end().unwrap();
        assert_eq!((f.sample_rate(), f.channels()), (rate, 2), "{file}");
        if exact {
            assert_eq!(back.frames(), n, "{file}: gapless length");
        } else {
            assert!(back.frames() >= n, "{file}: {} < {n}", back.frames());
        }
        // ADTS cannot carry the priming: only check it decodes to the right tone
        if min_snr > 0.0 {
            let m = n.min(back.frames());
            let got = snr(&l[..m], &back.channels[0][..m]);
            assert!(
                got >= min_snr,
                "{file}: left SNR {got:.1} dB (misaligned or broken)"
            );
            let got = snr(&r[..m], &back.channels[1][..m]);
            assert!(got >= min_snr, "{file}: right SNR {got:.1} dB");
        } else {
            let p = pitch(&back.channels[1][5000..60_000], rate);
            assert!((p - 660.0).abs() < 10.0, "{file}: {p}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn media_kit_picks_the_software_codecs() {
    use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat, RawAudioFormat, SampleFormat};
    let roster = mkff_audio::addon::roster();
    for fam in [
        CodecFamily::MpegAudio,
        CodecFamily::Aac,
        CodecFamily::Flac,
        CodecFamily::Alac,
        CodecFamily::Ac3,
        CodecFamily::Opus,
        CodecFamily::Vorbis,
    ] {
        let fmt = MediaFormat::encoded(fam, Vec::new(), 0, 0);
        let flavors = roster.find_flavors(
            mkff_mediakit::Kinds::DECODER,
            &fmt,
            &MediaFormat::Wildcard,
            AccelPolicy::Auto,
        );
        assert!(!flavors.is_empty(), "{fam}: no decoder flavor");
    }
    // decode a real stream through the roster
    let mut f = mkff_audio::format::open_path(codecs_dir("tone.flac")).unwrap();
    let t = f.tracks()[0].clone();
    let fmt = MediaFormat::encoded(
        CodecFamily::Flac,
        t.config.clone(),
        t.sample_rate,
        t.channels,
    );
    let (flavor, mut dec) = roster.audio_decoder(&fmt, AccelPolicy::Auto).unwrap();
    assert!(!flavor.info.accel.is_hardware());
    let mut total = 0;
    while let Some(p) = f.next_packet().unwrap() {
        total += dec.decode(&p.data, p.pts).unwrap().frames();
    }
    assert_eq!(total, 11_025);
    // and encode
    let input = MediaFormat::RawAudio(RawAudioFormat {
        frame_rate: 48_000,
        channels: 2,
        format: SampleFormat::F32Planar,
        buffer_frames: 0,
    });
    let (_, mut enc) = roster
        .audio_encoder(
            &input,
            &MediaFormat::encoded(CodecFamily::Aac, Vec::new(), 0, 0),
            AccelPolicy::Auto,
        )
        .unwrap();
    let x = sine(48_000, 1000.0, 48_000, 0.0);
    let mut chunks = enc.encode(&[&x, &x]).unwrap();
    chunks.extend(enc.flush().unwrap());
    assert!(chunks.len() >= 47);
    assert!(matches!(enc.output_format(), MediaFormat::EncodedAudio(e) if e.config.len() == 2));
}
