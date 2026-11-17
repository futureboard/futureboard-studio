//! Encoder tests: round trips through the crate's decoder, bit rate, delay, gapless info.

mod common;

use common::*;
use mkff_audio_core::{AudioBuffer, Decoder, Encoder, Packet};
use mkff_mp3::{
    parse_header, parse_xing, Mp3Decoder, Mp3Encoder, Mp3EncoderConfig, StereoMode, DECODER_DELAY,
};

fn encode(cfg: Mp3EncoderConfig, input: &[Vec<f32>], chunk: usize) -> (Mp3Encoder, Vec<Packet>) {
    let mut e = Mp3Encoder::new(cfg).unwrap();
    let mut packets = Vec::new();
    let n = input[0].len();
    let mut pos = 0;
    while pos < n {
        let end = (pos + chunk).min(n);
        let s: Vec<&[f32]> = input.iter().map(|c| &c[pos..end]).collect();
        packets.extend(e.encode(&s).unwrap());
        pos = end;
    }
    packets.extend(e.flush().unwrap());
    (e, packets)
}

fn decode_packets(packets: &[Packet]) -> AudioBuffer {
    let mut d = Mp3Decoder::new();
    let mut out = AudioBuffer::default();
    for p in packets {
        out.append(&d.decode(&p.data).unwrap());
    }
    assert_eq!(d.frames_concealed(), 0, "every frame decodes");
    out
}

/// SNR of the decoded output (delay removed) against the input, all channels.
fn roundtrip_snr(e: &Mp3Encoder, input: &[Vec<f32>], out: &AudioBuffer) -> f64 {
    let d = e.priming() as usize;
    let n = input[0].len();
    assert!(out.frames() >= n + d, "output covers the input");
    let mut got = Vec::new();
    let mut want = Vec::new();
    for (c, ch) in input.iter().enumerate() {
        got.extend_from_slice(&out.channels[c][d..d + n]);
        want.extend_from_slice(ch);
    }
    snr_db(&got, &want)
}

fn check(rate: u32, channels: usize, kbps: u32, mode: StereoMode, min_snr: f64) {
    let input = test_signal(rate, channels, 2.0);
    let cfg = Mp3EncoderConfig {
        sample_rate: rate,
        channels,
        bitrate_kbps: kbps,
        mode,
        quality: 3,
    };
    let (e, packets) = encode(cfg, &input, 1000);
    // constant bit rate: every packet is a complete frame of the nominal size (+ padding)
    let mut bytes = 0usize;
    for (i, p) in packets.iter().enumerate() {
        let h = parse_header(&p.data).unwrap();
        assert_eq!(h.frame_len_bytes, p.data.len());
        assert_eq!(h.bitrate_kbps, kbps);
        assert_eq!(h.sample_rate, rate);
        assert_eq!(h.channels, channels);
        assert_eq!(p.pts, (i * e.frame_size()) as i64);
        assert_eq!(p.duration as usize, e.frame_size());
        bytes += p.data.len();
    }
    let secs = (packets.len() * e.frame_size()) as f64 / f64::from(rate);
    let actual = bytes as f64 * 8.0 / secs / 1000.0;
    assert!(
        (actual - f64::from(kbps)).abs() / f64::from(kbps) < 0.002,
        "bit rate {actual:.3} vs {kbps}"
    );
    let out = decode_packets(&packets);
    assert_eq!(out.sample_rate, rate);
    assert_eq!(out.frames(), packets.len() * e.frame_size());
    let snr = roundtrip_snr(&e, &input, &out);
    eprintln!(
        "{rate} Hz {channels} ch {kbps} kbit/s {mode:?}: SNR {snr:.2} dB, {actual:.2} kbit/s"
    );
    assert!(snr > min_snr, "SNR {snr:.2} dB below {min_snr}");
    assert!(out
        .channels
        .iter()
        .flatten()
        .all(|v| v.is_finite() && v.abs() < 2.0));
}

#[test]
fn mono_44k() {
    check(44100, 1, 128, StereoMode::Mono, 25.0);
    check(44100, 1, 64, StereoMode::Mono, 17.0);
}

#[test]
fn stereo_44k() {
    check(44100, 2, 128, StereoMode::JointStereo, 20.0);
    check(44100, 2, 192, StereoMode::Stereo, 22.0);
}

#[test]
fn stereo_48k_and_32k() {
    check(48000, 2, 256, StereoMode::JointStereo, 22.0);
    check(32000, 2, 96, StereoMode::JointStereo, 20.0);
}

#[test]
fn lsf_rates() {
    check(22050, 2, 64, StereoMode::JointStereo, 18.0);
    check(24000, 1, 32, StereoMode::Mono, 15.0);
    check(16000, 2, 160, StereoMode::Stereo, 25.0);
}

#[test]
fn mpeg25_rates() {
    check(12000, 1, 32, StereoMode::Mono, 18.0);
    check(8000, 2, 16, StereoMode::JointStereo, 12.0);
}

#[test]
fn extreme_bitrates() {
    check(44100, 2, 320, StereoMode::Stereo, 25.0);
    check(44100, 2, 32, StereoMode::JointStereo, 10.0);
    check(16000, 1, 8, StereoMode::Mono, 8.0);
}

#[test]
fn pure_tone_is_clean() {
    let rate = 44100;
    let n = rate as usize;
    let tone: Vec<f32> = (0..n)
        .map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin())
        .collect();
    let input = vec![tone];
    let (e, packets) = encode(Mp3EncoderConfig::new(rate, 1, 128), &input, 4096);
    let out = decode_packets(&packets);
    let snr = roundtrip_snr(&e, &input, &out);
    eprintln!("1 kHz tone, 128 kbit/s mono: SNR {snr:.2} dB");
    assert!(snr > 40.0, "SNR {snr:.2}");
}

#[test]
fn delay_is_priming() {
    // an impulse comes out `priming()` samples later
    let rate = 44100;
    let mut x = vec![0f32; 20000];
    x[5000] = 0.9;
    let (e, packets) = encode(Mp3EncoderConfig::new(rate, 1, 320), &[x], 20000);
    let out = decode_packets(&packets);
    let peak = out.channels[0]
        .iter()
        .enumerate()
        .fold(
            (0, 0f32),
            |m, (i, &v)| if v.abs() > m.1 { (i, v.abs()) } else { m },
        );
    assert_eq!(peak.0, 5000 + e.priming() as usize);
    assert_eq!(e.priming(), 1057);
}

#[test]
fn info_frame_for_gapless_playback() {
    let input = test_signal(44100, 2, 1.0);
    let n = input[0].len();
    let (e, packets) = encode(Mp3EncoderConfig::default(), &input, 777);
    let s = e.stats();
    assert_eq!(s.frames as usize, packets.len());
    assert_eq!(
        s.bytes as usize,
        packets.iter().map(|p| p.data.len()).sum::<usize>()
    );
    assert_eq!(s.input_samples as usize, n);
    let tag = e.xing_frame();
    let x = parse_xing(&tag).expect("Info frame");
    assert_eq!(x.frames, Some(packets.len() as u32));
    assert_eq!(x.encoder_delay, Some(e.priming() - DECODER_DELAY));
    assert_eq!(x.gapless_samples(1152), Some(n as u64));
    assert_eq!(x.tag_crc_ok, Some(true));
    assert_eq!(x.music_crc, Some(s.music_crc));
    // the tag frame has the stream's size and decodes to nothing
    assert_eq!(tag.len(), packets[0].data.len().min(tag.len()));
    let mut file = tag.clone();
    for p in &packets {
        file.extend_from_slice(&p.data);
    }
    let out = decode(&file, 1000);
    assert_eq!(out.frames(), packets.len() * 1152);
    // trimming as a player does recovers the input length exactly
    let delay = (x.encoder_delay.unwrap() + DECODER_DELAY) as usize;
    let mut trimmed = out.clone();
    trimmed.skip_front(delay);
    trimmed.truncate(n);
    assert_eq!(trimmed.frames(), n);
    let snr = snr_db(&trimmed.channels[0], &input[0]);
    assert!(snr > 15.0);
}

#[test]
fn chunking_does_not_change_output() {
    let input = test_signal(22050, 2, 0.7);
    let cfg = Mp3EncoderConfig::new(22050, 2, 48);
    let (_, a) = encode(cfg, &input, 1);
    let (_, b) = encode(cfg, &input, 100_000);
    assert_eq!(a, b);
}

#[test]
fn empty_and_tiny_input() {
    let mut e = Mp3Encoder::new(Mp3EncoderConfig::default()).unwrap();
    let p = e.flush().unwrap();
    assert!(!p.is_empty(), "the delay still produces frames");
    let out = decode_packets(&p);
    assert!(out.channels.iter().flatten().all(|&v| v == 0.0));
    let (_, p) = encode(Mp3EncoderConfig::new(44100, 1, 64), &[vec![0.3; 10]], 10);
    assert_eq!(p.len(), 1);
}

#[test]
fn rejects_bad_configs() {
    let bad = [
        Mp3EncoderConfig {
            sample_rate: 44000,
            ..Default::default()
        },
        Mp3EncoderConfig {
            bitrate_kbps: 130,
            ..Default::default()
        },
        Mp3EncoderConfig {
            sample_rate: 22050,
            bitrate_kbps: 192,
            ..Default::default()
        },
        Mp3EncoderConfig {
            channels: 3,
            ..Default::default()
        },
        Mp3EncoderConfig {
            mode: StereoMode::Mono,
            ..Default::default()
        },
    ];
    for cfg in bad {
        assert!(Mp3Encoder::new(cfg).is_err(), "{cfg:?}");
    }
    let mut e = Mp3Encoder::new(Mp3EncoderConfig::default()).unwrap();
    assert!(e.encode(&[&[0.0; 4]]).is_err(), "wrong channel count");
    assert!(
        e.encode(&[&[0.0; 4], &[0.0; 3]]).is_err(),
        "unequal lengths"
    );
    // non-finite input is sanitised
    let p = e
        .encode(&[&[f32::NAN; 5000], &[f32::INFINITY; 5000]])
        .unwrap();
    let p2 = e.flush().unwrap();
    let all: Vec<Packet> = p.into_iter().chain(p2).collect();
    let out = decode_packets(&all);
    assert!(out.channels.iter().flatten().all(|v| v.is_finite()));
}

#[test]
fn quality_levels() {
    let input = test_signal(44100, 2, 1.0);
    for q in [0u8, 5, 9] {
        let cfg = Mp3EncoderConfig {
            quality: q,
            ..Default::default()
        };
        let (e, packets) = encode(cfg, &input, 4096);
        let out = decode_packets(&packets);
        let snr = roundtrip_snr(&e, &input, &out);
        eprintln!("quality {q}: SNR {snr:.2} dB");
        assert!(snr > 15.0);
    }
}

#[test]
fn block_switching() {
    let rate = 44100;
    let n = 2 * rate as usize;
    let mut rng = Rng(99);
    // steady material: a tone plus stationary noise never switches
    let steady: Vec<f32> = (0..n)
        .map(|i| 0.3 * (i as f32 * 0.05).sin() + 0.05 * rng.uniform())
        .collect();
    let (e, _) = encode(Mp3EncoderConfig::new(rate, 1, 128), &[steady], 4096);
    // (only the onset at t = 0, from silence, may switch)
    assert!(e.stats().short_blocks <= 2, "{}", e.stats().short_blocks);
    // castanet-like clicks over silence switch to short blocks around each click
    let mut clicks = vec![0f32; n];
    for k in 0..8 {
        let at = 5000 + k * 10000;
        for i in 0..200 {
            clicks[at + i] = 0.6 * (-(i as f32) / 40.0).exp() * rng.uniform();
        }
    }
    let input = vec![clicks];
    let (e, packets) = encode(Mp3EncoderConfig::new(rate, 1, 128), &input, 4096);
    let s = e.stats().short_blocks;
    assert!((8..=40).contains(&s), "{s} short granules");
    // little pre-echo: before each click the output stays near silence
    let out = decode_packets(&packets);
    let d = e.priming() as usize;
    for k in 0..8 {
        let at = 5000 + k * 10000 + d;
        let pre: f32 = out.channels[0][at - 1200..at - 400]
            .iter()
            .map(|v| v * v)
            .sum();
        let post: f32 = out.channels[0][at..at + 200].iter().map(|v| v * v).sum();
        assert!(pre < post * 1e-3, "click {k}: pre-echo {pre:e} vs {post:e}");
    }
}
