//! Encoder tests: round trips through our decoder (ffmpeg was used during development to check
//! that it decodes the same streams identically; see the crate README).

use mkff_aac::{
    parse_adts, AacDecoder, AacEncoder, AacEncoderConfig, AudioSpecificConfig, SAMPLE_RATES,
};
use mkff_audio_core::{AudioBuffer, Decoder, Encoder, Error, Packet};
use std::f64::consts::PI;

fn tone(rate: u32, n: usize, f: f64, a: f64) -> Vec<f32> {
    (0..n)
        .map(|i| (a * (2.0 * PI * f * i as f64 / f64::from(rate)).sin()) as f32)
        .collect()
}

/// Linear sweep from `f0` to `f1` Hz.
fn sweep(rate: u32, n: usize, f0: f64, f1: f64, a: f64) -> Vec<f32> {
    let dur = n as f64 / f64::from(rate);
    let k = (f1 - f0) / dur;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            (a * (2.0 * PI * (f0 * t + 0.5 * k * t * t)).sin()) as f32
        })
        .collect()
}

fn encode(cfg: AacEncoderConfig, input: &[Vec<f32>], chunk: usize) -> (Vec<Packet>, Vec<u8>) {
    let mut enc = AacEncoder::new(cfg).unwrap();
    let n = input[0].len();
    let mut packets = Vec::new();
    let mut pos = 0;
    while pos < n {
        let end = (pos + chunk).min(n);
        let refs: Vec<&[f32]> = input.iter().map(|c| &c[pos..end]).collect();
        packets.extend(enc.encode(&refs).unwrap());
        pos = end;
    }
    packets.extend(enc.flush().unwrap());
    assert_eq!(packets.len(), n.div_ceil(1024) + 1);
    for (k, p) in packets.iter().enumerate() {
        assert_eq!(p.pts, 1024 * k as i64);
        assert_eq!(p.duration, 1024);
        assert!(
            p.data.len() * 8 <= 6144 * input.len(),
            "packet {k}: {} bytes",
            p.data.len()
        );
    }
    assert_eq!(enc.priming(), 1024);
    assert_eq!(enc.frame_size(), 1024);
    (packets, enc.config())
}

fn decode(asc: &[u8], packets: &[Packet]) -> (AudioBuffer, mkff_aac::DecoderStats) {
    let mut dec = AacDecoder::new(asc).unwrap();
    let mut out = AudioBuffer::default();
    for p in packets {
        out.append(&dec.decode(&p.data).unwrap());
    }
    (out, *dec.stats())
}

/// SNR (dB) of the decoded output against the input, after removing the 1024-sample priming,
/// skipping the first and last `edge` samples.
fn snr(input: &[f32], out: &[f32], edge: usize) -> f64 {
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for i in edge..input.len() - edge {
        let a = f64::from(input[i]);
        let d = a - f64::from(out[i + 1024]);
        s += a * a;
        e += d * d;
    }
    10.0 * (s / e.max(1e-30)).log10()
}

fn bitrate(packets: &[Packet], rate: u32) -> f64 {
    let bits: usize = packets.iter().map(|p| p.data.len() * 8).sum();
    bits as f64 / (packets.len() as f64 * 1024.0 / f64::from(rate))
}

#[test]
fn stereo_tones_and_sweeps_128k() {
    let rate = 44100;
    let n = rate as usize * 3;
    let input = vec![
        tone(rate, n, 1000.0, 0.5),
        sweep(rate, n, 50.0, 15000.0, 0.4),
    ];
    let (packets, asc) = encode(AacEncoderConfig::new(rate, 2, 128_000), &input, 1000);
    assert_eq!(
        AudioSpecificConfig::parse(&asc).unwrap(),
        AudioSpecificConfig::lc(rate, 2)
    );
    let br = bitrate(&packets, rate);
    let (out, _) = decode(&asc, &packets);
    let s = [
        snr(&input[0], &out.channels[0], 2048),
        snr(&input[1], &out.channels[1], 2048),
    ];
    eprintln!("128k stereo: {br:.0} bps, SNR {s:.1?} dB");
    assert!((br / 128_000.0 - 1.0).abs() < 0.03, "bitrate {br}");
    assert!(s[0] > 45.0 && s[1] > 35.0, "{s:?}");
}

#[test]
fn mono_tone_64k_and_low_rates() {
    for (rate, br, min) in [
        (48000, 64_000, 30.0),
        (22050, 32_000, 25.0),
        (8000, 16_000, 20.0),
    ] {
        let n = rate as usize * 2;
        let x: Vec<f32> = tone(rate, n, 440.0, 0.4)
            .iter()
            .zip(tone(rate, n, 1500.0, 0.2))
            .map(|(a, b)| a + b)
            .collect();
        let input = vec![x];
        let (packets, asc) = encode(AacEncoderConfig::new(rate, 1, br), &input, 4096);
        let (out, _) = decode(&asc, &packets);
        let s = snr(&input[0], &out.channels[0], 2048);
        let b = bitrate(&packets, rate);
        eprintln!("{rate} Hz mono {br}: {b:.0} bps, SNR {s:.1} dB");
        assert!(s > min, "{rate}: SNR {s}");
        assert!(
            (b / f64::from(br) - 1.0).abs() < 0.05,
            "{rate}: bitrate {b}"
        );
    }
}

#[test]
fn every_sample_rate() {
    for &rate in &SAMPLE_RATES {
        let n = 8192;
        let f = f64::from(rate) / 20.0;
        let input = vec![tone(rate, n, f, 0.5)];
        let br = (f64::from(rate) * 1.5) as u32;
        let (packets, asc) = encode(
            AacEncoderConfig::new(rate, 1, br.clamp(8000, 160_000)),
            &input,
            777,
        );
        let (out, _) = decode(&asc, &packets);
        assert_eq!(out.sample_rate, rate);
        let s = snr(&input[0], &out.channels[0], 2048);
        assert!(s > 25.0, "{rate}: SNR {s}");
    }
}

#[test]
fn noise_keeps_its_band_limited_energy() {
    let rate = 48000;
    let n = rate as usize;
    let mut seed = 1u32;
    let x: Vec<f32> = (0..n)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect();
    for pns in [false, true] {
        let mut cfg = AacEncoderConfig::new(rate, 1, 64_000);
        cfg.pns = pns;
        cfg.bandwidth_hz = 12000;
        let input = vec![x.clone()];
        let (packets, asc) = encode(cfg, &input, 2048);
        let (out, stats) = decode(&asc, &packets);
        let e_in: f64 = x[4096..n - 4096]
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum();
        let e_out: f64 = out.channels[0][4096 + 1024..n - 4096 + 1024]
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum();
        // half the band is kept: about half the energy
        let ratio = e_out / e_in;
        eprintln!(
            "noise pns={pns}: energy ratio {ratio:.3}, {} PNS bands",
            stats.pns_bands
        );
        assert!((0.4..0.65).contains(&ratio), "pns={pns}: ratio {ratio}");
        assert_eq!(stats.pns_bands > 0, pns);
    }
}

#[test]
fn transients_use_short_blocks_without_pre_echo() {
    let rate = 44100;
    let n = rate as usize * 2;
    let bg = tone(rate, n, 220.0, 0.03);
    let mut x = bg.clone();
    let clicks: Vec<usize> = (1..8).map(|k| k * rate as usize / 4 + 300 * k).collect();
    for &c in &clicks {
        for i in 0..180 {
            x[c + i] += (0.8 * (2.0 * PI * 2500.0 * i as f64 / f64::from(rate)).sin()) as f32;
        }
    }
    for tns in [false, true] {
        let mut cfg = AacEncoderConfig::new(rate, 1, 96_000);
        cfg.tns = tns;
        let input = vec![x.clone()];
        let (packets, asc) = encode(cfg, &input, 1024);
        let (out, stats) = decode(&asc, &packets);
        assert!(
            stats.window_sequences[2] >= clicks.len() as u64,
            "{stats:?}"
        );
        assert!(stats.window_sequences[1] > 0 && stats.window_sequences[3] > 0);
        assert_eq!(stats.tns_frames > 0, tns);
        // error from 30 ms to 6 ms before each click, relative to the background
        for &c in &clicks {
            let r = c - 1323..c - 265;
            let e: f64 = r
                .clone()
                .map(|i| f64::from(x[i] - out.channels[0][i + 1024]).powi(2))
                .sum();
            let s: f64 = r.map(|i| f64::from(bg[i]).powi(2)).sum();
            let db = 10.0 * (s / e).log10();
            assert!(
                db > 15.0,
                "tns={tns} click at {c}: background/pre-echo {db:.1} dB"
            );
        }
    }
}

#[test]
fn all_channel_layouts() {
    let rate = 48000;
    let n = 6000;
    for ch in 1..=8usize {
        // LFE (channel 3 from 6 channels on) gets a low tone
        let freqs: Vec<f64> = (0..ch)
            .map(|c| {
                if ch >= 6 && c == 3 {
                    60.0
                } else {
                    300.0 + 170.0 * c as f64
                }
            })
            .collect();
        let input: Vec<Vec<f32>> = freqs.iter().map(|&f| tone(rate, n, f, 0.3)).collect();
        let (packets, asc) = encode(
            AacEncoderConfig::new(rate, ch, 64_000 * ch as u32),
            &input,
            1500,
        );
        let parsed = AudioSpecificConfig::parse(&asc).unwrap();
        assert_eq!(parsed.channels(), ch);
        let (out, _) = decode(&asc, &packets);
        assert_eq!(out.channels.len(), ch);
        for (c, x) in input.iter().enumerate() {
            let s = snr(x, &out.channels[c], 1024);
            assert!(s > 30.0, "{ch} channels, channel {c}: SNR {s:.1}");
        }
        // ADTS framing decodes the same
        let enc = AacEncoder::new(AacEncoderConfig::new(rate, ch, 64_000 * ch as u32)).unwrap();
        let mut dec = AacDecoder::new(&[]).unwrap();
        let mut adts_out = AudioBuffer::default();
        for p in &packets {
            let f = enc.adts_frame(&p.data);
            let h = parse_adts(&f).unwrap();
            assert_eq!(h.frame_length, f.len());
            adts_out.append(&dec.decode(&f).unwrap());
        }
        assert_eq!(adts_out, out, "{ch} channels via ADTS");
    }
}

#[test]
fn silence_full_scale_and_bad_samples() {
    let rate = 44100;
    let n = 20000;
    let square: Vec<f32> = (0..n)
        .map(|i| if (i / 100) % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let mut odd = vec![0.0f32; n];
    odd[100] = f32::NAN;
    odd[200] = f32::INFINITY;
    odd[300] = 1e30;
    odd[5000..].fill(0.25);
    for br in [16_000, 128_000, 320_000] {
        let input = vec![vec![0.0f32; n], square.clone(), odd.clone()];
        let (packets, asc) = encode(AacEncoderConfig::new(rate, 3, br * 3 / 2), &input, 3333);
        let (out, _) = decode(&asc, &packets);
        assert!(out.channels[2].iter().all(|v| v.is_finite()));
        let e: f32 = out.channels[2].iter().map(|v| v * v).sum::<f32>();
        assert!(e.is_finite());
    }
}

#[test]
fn invalid_configurations() {
    assert!(matches!(
        AacEncoder::new(AacEncoderConfig::new(44000, 2, 128_000)),
        Err(Error::InvalidArgument(_))
    ));
    assert!(AacEncoder::new(AacEncoderConfig::new(44100, 0, 128_000)).is_err());
    assert!(AacEncoder::new(AacEncoderConfig::new(44100, 9, 128_000)).is_err());
    assert!(AacEncoder::new(AacEncoderConfig::new(44100, 2, 1_000)).is_err());
    assert!(AacEncoder::new(AacEncoderConfig::new(8000, 1, 300_000)).is_err());
    let mut enc = AacEncoder::new(AacEncoderConfig::new(44100, 2, 128_000)).unwrap();
    let a = vec![0.0f32; 100];
    let b = vec![0.0f32; 99];
    assert!(enc.encode(&[&a]).is_err());
    assert!(enc.encode(&[&a, &b]).is_err());
    assert!(enc.encode(&[&a, &a]).is_ok());
}

#[test]
fn seven_channels_use_a_program_config_element() {
    let mut enc = AacEncoder::new(AacEncoderConfig::new(48000, 7, 448_000)).unwrap();
    let asc = AudioSpecificConfig::parse(&enc.config()).unwrap();
    assert_eq!(asc.channel_config, 0);
    assert_eq!(asc.pce.as_ref().unwrap().channels(), 7);
    let x = tone(48000, 2048, 500.0, 0.3);
    let input: Vec<&[f32]> = (0..7).map(|_| &x[..]).collect();
    let mut packets = enc.encode(&input).unwrap();
    packets.extend(enc.flush().unwrap());
    // an ADTS stream (channel configuration 0) carries the PCE in band
    let mut dec = AacDecoder::new(&[]).unwrap();
    let out = dec.decode(&enc.adts_frame(&packets[1].data)).unwrap();
    assert_eq!(out.channels.len(), 7);
    assert!(dec.stats().pce > 0);
}
