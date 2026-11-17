//! Encoder round trips through our decoder: bit-exact samples, STREAMINFO bookkeeping and MD5.

use mkff_audio_core::{Decoder, Encoder, Packet};
use mkff_flac::{parse_frame_header, FlacDecoder, FlacEncoder, FlacEncoderConfig, Md5, StreamInfo};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }
    /// Uniform in [-1, 1).
    fn f(&mut self) -> f64 {
        f64::from(self.next()) / 2_147_483_648.0 - 1.0
    }
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    Music,
    Noise,
    Silence,
    Extremes,
    Wasted,
    Dc,
}

fn make(signal: Signal, bps: u32, channels: usize, n: usize, seed: u64) -> Vec<Vec<i32>> {
    let mut rng = Rng(seed);
    let max = ((1i64 << (bps - 1)) - 1) as f64;
    let (lo, hi) = (-(1i64 << (bps - 1)), (1i64 << (bps - 1)) - 1);
    let clamp = |v: f64| (v.round() as i64).clamp(lo, hi) as i32;
    (0..channels)
        .map(|c| {
            let f1 = 0.01 + 0.013 * c as f64;
            (0..n)
                .map(|i| {
                    let t = i as f64;
                    match signal {
                        Signal::Music => clamp(
                            max * (0.5 * (t * f1).sin()
                                + 0.2 * (t * f1 * 3.7 + 1.0).sin() * (t * 0.0003).cos())
                                + max * 0.01 * rng.f(),
                        ),
                        Signal::Noise => clamp(max * rng.f()),
                        Signal::Silence => 0,
                        Signal::Extremes => {
                            if (i / (7 + c)) % 2 == 0 {
                                hi as i32
                            } else {
                                lo as i32
                            }
                        }
                        Signal::Wasted => {
                            let shift = (bps - 1).min(3 + c as u32);
                            let v = clamp(max * 0.6 * (t * f1).sin() + max * 0.05 * rng.f());
                            (v >> shift) << shift
                        }
                        Signal::Dc => (hi / 3) as i32 - c as i32,
                    }
                })
                .collect()
        })
        .collect()
}

fn md5_of(chans: &[Vec<i32>], bps: u32) -> [u8; 16] {
    let bytes = bps.div_ceil(8) as usize;
    let mut buf = Vec::new();
    for i in 0..chans[0].len() {
        for c in chans {
            buf.extend_from_slice(&c[i].to_le_bytes()[..bytes]);
        }
    }
    Md5::digest(&buf)
}

/// Encode `input` in uneven chunks, decode, compare. Returns the total coded size in bytes.
fn roundtrip(cfg: FlacEncoderConfig, input: &[Vec<i32>]) -> usize {
    let mut enc = FlacEncoder::new(cfg).unwrap();
    let n = input[0].len();
    let mut packets: Vec<Packet> = Vec::new();
    let mut pos = 0;
    let mut step = 1;
    while pos < n {
        let end = (pos + step).min(n);
        let chunk: Vec<&[i32]> = input.iter().map(|c| &c[pos..end]).collect();
        packets.extend(enc.encode_i32(&chunk).unwrap());
        pos = end;
        step = step * 3 + 17;
    }
    packets.extend(enc.flush().unwrap());
    let info = enc.stream_info();
    assert_eq!(info.total_samples, n as u64);
    assert_eq!(info.md5, md5_of(input, cfg.bits_per_sample), "MD5 {cfg:?}");
    assert_eq!(StreamInfo::parse(&enc.config()).unwrap(), info);

    let mut dec = FlacDecoder::new(&enc.config()).unwrap();
    let mut out = vec![Vec::new(); input.len()];
    let mut expect_pts = 0;
    let (mut min_f, mut max_f) = (u32::MAX, 0);
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(p.pts, expect_pts);
        expect_pts += i64::from(p.duration);
        let hdr = parse_frame_header(&p.data).unwrap();
        assert_eq!(hdr.number, i as u64);
        assert!(!hdr.variable_block_size);
        min_f = min_f.min(p.data.len() as u32);
        max_f = max_f.max(p.data.len() as u32);
        let (pcm, bps) = dec
            .decode_i32(&p.data)
            .unwrap_or_else(|e| panic!("{cfg:?} frame {i}: {e}"));
        assert_eq!(bps, cfg.bits_per_sample);
        assert_eq!(pcm[0].len(), p.duration as usize);
        for (o, c) in out.iter_mut().zip(pcm) {
            o.extend_from_slice(&c);
        }
    }
    if !packets.is_empty() {
        assert_eq!((info.min_frame, info.max_frame), (min_f, max_f));
    }
    for (c, (o, i)) in out.iter().zip(input).enumerate() {
        if o != i {
            let at = o
                .iter()
                .zip(i)
                .position(|(a, b)| a != b)
                .unwrap_or(o.len().min(i.len()));
            panic!(
                "{cfg:?}: channel {c} differs at sample {at} (len {} vs {})",
                o.len(),
                i.len()
            );
        }
    }
    packets.iter().map(|p| p.data.len()).sum()
}

fn cfg(channels: usize, bps: u32, block_size: usize, level: u8) -> FlacEncoderConfig {
    FlacEncoderConfig {
        sample_rate: 44100,
        channels,
        bits_per_sample: bps,
        block_size,
        level,
    }
}

#[test]
fn all_levels_stereo16() {
    let input = make(Signal::Music, 16, 2, 20_000, 1);
    let raw = 20_000 * 4;
    let mut sizes = Vec::new();
    for level in 0..=8 {
        let size = roundtrip(cfg(2, 16, 4096, level), &input);
        assert!(size < raw * 3 / 4, "level {level}: {size} bytes");
        sizes.push(size);
    }
    // LPC levels beat fixed-only levels on tonal input
    assert!(sizes[5] < sizes[0], "{sizes:?}");
    assert!(sizes[8] <= sizes[5] + sizes[5] / 50, "{sizes:?}");
}

#[test]
fn bit_depths() {
    for bps in [4, 8, 12, 16, 20, 24, 32] {
        for signal in [
            Signal::Music,
            Signal::Noise,
            Signal::Extremes,
            Signal::Wasted,
        ] {
            for level in [0, 5, 8] {
                let input = make(signal, bps, 2, 9000, u64::from(bps));
                roundtrip(cfg(2, bps, 4096, level), &input);
            }
        }
    }
}

#[test]
fn odd_bit_depths_and_rates() {
    for bps in [5, 7, 11, 17, 23, 31] {
        let input = make(Signal::Music, bps, 1, 5000, 7);
        roundtrip(cfg(1, bps, 1152, 5), &input);
    }
    for rate in [
        8000, 11025, 22050, 32000, 44100, 48000, 88200, 96000, 192000, 7350, 384000, 1_000_000,
    ] {
        let input = make(Signal::Music, 16, 1, 3000, 3);
        let c = FlacEncoderConfig {
            sample_rate: rate,
            ..cfg(1, 16, 1024, 3)
        };
        roundtrip(c, &input);
    }
}

#[test]
fn channel_counts() {
    for ch in 1..=8 {
        for level in [1, 4, 6] {
            let input = make(Signal::Music, 16, ch, 6000, ch as u64);
            roundtrip(cfg(ch, 16, 2048, level), &input);
        }
    }
}

#[test]
fn block_sizes() {
    let input = make(Signal::Music, 16, 2, 30_000, 5);
    for bs in [
        16, 17, 192, 255, 256, 257, 576, 1000, 1152, 4096, 4608, 8192, 16384, 32768, 65535,
    ] {
        roundtrip(cfg(2, 16, bs, 5), &input);
    }
}

#[test]
fn special_signals() {
    for signal in [
        Signal::Silence,
        Signal::Dc,
        Signal::Extremes,
        Signal::Noise,
        Signal::Wasted,
    ] {
        for bps in [16, 24] {
            let input = make(signal, bps, 2, 10_000, 11);
            let size = roundtrip(cfg(2, bps, 4096, 5), &input);
            if matches!(signal, Signal::Silence | Signal::Dc) {
                // constant subframes: a handful of bytes per frame
                assert!(size < 3 * 40, "{signal:?}: {size}");
            }
        }
    }
    // short streams: one sample, fewer samples than a block, exactly one block
    for n in [1, 2, 5, 15, 4096] {
        let input = make(Signal::Music, 16, 2, n, 2);
        roundtrip(cfg(2, 16, 4096, 8), &input);
    }
}

#[test]
fn wasted_bits_shrink_output() {
    let base = make(Signal::Music, 24, 1, 8192, 3);
    let shifted: Vec<Vec<i32>> = base
        .iter()
        .map(|c| c.iter().map(|&s| (s >> 8) << 8).collect())
        .collect();
    let plain: Vec<Vec<i32>> = base
        .iter()
        .map(|c| c.iter().map(|&s| s >> 8).collect())
        .collect();
    let a = roundtrip(cfg(1, 24, 4096, 5), &shifted);
    let b = roundtrip(cfg(1, 16, 4096, 5), &plain);
    assert!(a <= b + 16, "wasted-bit stream {a} vs 16-bit stream {b}");
}

#[test]
fn f32_input_is_quantized_with_clipping() {
    let mut enc = FlacEncoder::new(cfg(1, 16, 256, 5)).unwrap();
    let input: Vec<f32> = (0..300).map(|i| (i as f32 - 150.0) / 100.0).collect(); // -1.5 .. 1.49
    let mut packets = enc.encode(&[&input]).unwrap();
    packets.extend(enc.flush().unwrap());
    assert_eq!(packets.len(), 2);
    assert_eq!(enc.frame_size(), 256);
    assert_eq!(enc.priming(), 0);
    let mut dec = FlacDecoder::new(&enc.config()).unwrap();
    let mut out = Vec::new();
    for p in &packets {
        out.extend(dec.decode(&p.data).unwrap().channels.remove(0));
    }
    for (x, y) in input.iter().zip(&out) {
        let expect = (f64::from(*x) * 32768.0).round().clamp(-32768.0, 32767.0) / 32768.0;
        assert_eq!(f64::from(*y), expect);
    }
}

#[test]
fn bad_config_and_input() {
    assert!(FlacEncoder::new(cfg(0, 16, 4096, 5)).is_err());
    assert!(FlacEncoder::new(cfg(9, 16, 4096, 5)).is_err());
    assert!(FlacEncoder::new(cfg(2, 3, 4096, 5)).is_err());
    assert!(FlacEncoder::new(cfg(2, 33, 4096, 5)).is_err());
    assert!(FlacEncoder::new(cfg(2, 16, 15, 5)).is_err());
    assert!(FlacEncoder::new(cfg(2, 16, 65536, 5)).is_err());
    assert!(FlacEncoder::new(cfg(2, 16, 4096, 9)).is_err());
    let mut enc = FlacEncoder::new(cfg(2, 16, 4096, 5)).unwrap();
    assert!(enc.encode_i32(&[&[0, 1]]).is_err());
    assert!(enc.encode_i32(&[&[0, 1], &[0]]).is_err());
    assert!(enc.encode_i32(&[&[40000], &[0]]).is_err());
    enc.encode_i32(&[&[1, 2, 3], &[4, 5, 6]]).unwrap();
    assert_eq!(enc.flush().unwrap().len(), 1);
    assert!(enc.encode_i32(&[&[1], &[2]]).is_err());
}

#[test]
fn native_flac_file_layout() {
    // header + frames form a valid native stream for other tools
    let input = make(Signal::Music, 16, 2, 10_000, 9);
    let mut enc = FlacEncoder::new(cfg(2, 16, 4096, 5)).unwrap();
    let refs: Vec<&[i32]> = input.iter().map(|c| c.as_slice()).collect();
    let mut packets = enc.encode_i32(&refs).unwrap();
    packets.extend(enc.flush().unwrap());
    let mut file = enc.stream_info().to_stream_header();
    for p in &packets {
        file.extend_from_slice(&p.data);
    }
    let info = StreamInfo::parse(&file).unwrap();
    assert_eq!(info, enc.stream_info());
    assert_eq!(mkff_flac::find_frame_sync(&file), Some(42));
}
