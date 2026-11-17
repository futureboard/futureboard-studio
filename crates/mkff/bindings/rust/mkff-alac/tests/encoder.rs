//! ALAC encoder round trips through the decoder (covers 20 / 32-bit and 8 channels, which the
//! ffmpeg-made fixtures cannot), plus hand-built packets for rare syntax.

use mkff_alac::{AlacConfig, AlacDecoder, AlacEncoder, AlacEncoderConfig};
use mkff_audio_core::bits::BitWriter;
use mkff_audio_core::{Decoder, Encoder, Packet};

fn signal(kind: u32, depth: u32, channels: usize, n: usize) -> Vec<Vec<i32>> {
    let max = ((1i64 << (depth - 1)) - 1) as f64;
    let (lo, hi) = (-(1i64 << (depth - 1)), (1i64 << (depth - 1)) - 1);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ u64::from(kind * 31 + depth);
    let mut noise = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    };
    (0..channels)
        .map(|c| {
            (0..n)
                .map(|i| {
                    let t = i as f64;
                    let v = match kind {
                        0 => {
                            max * (0.45 * (t * (0.011 + 0.007 * c as f64)).sin()
                                + 0.2 * (t * 0.05).sin())
                                + max * 0.002 * noise()
                        }
                        1 => max * 1.9 * noise(),
                        2 => 0.0,
                        3 => {
                            if (i / 50) % 2 == 0 {
                                max
                            } else {
                                -max - 1.0
                            }
                        }
                        // mostly silence with short bursts: zero runs
                        _ => {
                            if (i / 300) % 4 == 0 {
                                max * 0.3 * (t * 0.1).sin()
                            } else {
                                0.0
                            }
                        }
                    };
                    (v.round() as i64).clamp(lo, hi) as i32
                })
                .collect()
        })
        .collect()
}

fn roundtrip(cfg: AlacEncoderConfig, input: &[Vec<i32>]) -> usize {
    let mut enc = AlacEncoder::new(cfg).unwrap();
    let n = input[0].len();
    let mut packets: Vec<Packet> = Vec::new();
    let mut pos = 0;
    let mut step = 3;
    while pos < n {
        let end = (pos + step).min(n);
        let chunk: Vec<&[i32]> = input.iter().map(|c| &c[pos..end]).collect();
        packets.extend(enc.encode_i32(&chunk).unwrap());
        pos = end;
        step = step * 2 + 101;
    }
    packets.extend(enc.flush().unwrap());
    let cookie = enc.config();
    let parsed = AlacConfig::parse(&cookie).unwrap();
    assert_eq!(u32::from(parsed.bit_depth), cfg.bit_depth);
    assert_eq!(
        parsed.max_frame_bytes as usize,
        packets.iter().map(|p| p.data.len()).max().unwrap_or(0)
    );
    let mut dec = AlacDecoder::new(&cookie).unwrap();
    let mut out = vec![Vec::new(); input.len()];
    for (i, p) in packets.iter().enumerate() {
        let (pcm, bps) = dec
            .decode_i32(&p.data)
            .unwrap_or_else(|e| panic!("{cfg:?} packet {i}: {e}"));
        assert_eq!(bps, cfg.bit_depth);
        assert_eq!(pcm[0].len(), p.duration as usize);
        for (o, c) in out.iter_mut().zip(pcm) {
            o.extend_from_slice(&c);
        }
    }
    for (c, (o, i)) in out.iter().zip(input).enumerate() {
        if o != i {
            let at = o.iter().zip(i).position(|(a, b)| a != b);
            panic!(
                "{cfg:?}: channel {c} differs at {at:?} (len {} vs {})",
                o.len(),
                i.len()
            );
        }
    }
    packets.iter().map(|p| p.data.len()).sum()
}

#[test]
fn depths_and_signals() {
    for depth in [16, 20, 24, 32] {
        for kind in 0..5 {
            for channels in [1, 2] {
                let input = signal(kind, depth, channels, 10_000);
                let cfg = AlacEncoderConfig {
                    channels,
                    bit_depth: depth,
                    ..Default::default()
                };
                let size = roundtrip(cfg, &input);
                let raw = 10_000 * channels * depth as usize / 8;
                if kind == 0 {
                    // the tone carries noise of about depth - 9 bits
                    let bound = raw * (depth as usize - 5) / depth as usize;
                    assert!(size < bound, "depth {depth} ch {channels}: {size} of {raw}");
                }
                if kind == 2 {
                    // a 32-bit channel pair must shift at least one byte per sample out
                    // (its channels would need 33 bits otherwise), and those go verbatim
                    let floor = if depth == 32 && channels == 2 {
                        10_000 * 2
                    } else {
                        0
                    };
                    assert!(size < floor + 200 * channels, "silence: {size}");
                }
            }
        }
    }
}

#[test]
fn all_channel_counts() {
    for channels in 1..=8 {
        for depth in [16, 24] {
            let input = signal(0, depth, channels, 9000);
            roundtrip(
                AlacEncoderConfig {
                    channels,
                    bit_depth: depth,
                    ..Default::default()
                },
                &input,
            );
        }
    }
}

#[test]
fn frame_lengths_and_short_streams() {
    for fl in [16, 352, 1000, 4096, 8192] {
        let input = signal(0, 16, 2, 20_000);
        roundtrip(
            AlacEncoderConfig {
                frame_length: fl,
                ..Default::default()
            },
            &input,
        );
    }
    for n in [1, 2, 5, 31, 4096] {
        let input = signal(4, 24, 2, n);
        roundtrip(
            AlacEncoderConfig {
                bit_depth: 24,
                ..Default::default()
            },
            &input,
        );
    }
}

#[test]
fn channel_order_is_wav() {
    // a distinct constant per WAV channel survives the ALAC element layout both ways
    for channels in 1..=8 {
        let input: Vec<Vec<i32>> = (0..channels)
            .map(|c| vec![100 * c as i32 + 7; 500])
            .collect();
        roundtrip(
            AlacEncoderConfig {
                channels,
                ..Default::default()
            },
            &input,
        );
    }
}

#[test]
fn f32_input_and_trait() {
    let mut enc = AlacEncoder::new(AlacEncoderConfig {
        channels: 1,
        frame_length: 256,
        ..Default::default()
    })
    .unwrap();
    let x: Vec<f32> = (0..600).map(|i| (i as f32 - 300.0) / 200.0).collect();
    let mut packets = enc.encode(&[&x]).unwrap();
    packets.extend(enc.flush().unwrap());
    assert_eq!(packets.len(), 3);
    assert_eq!(
        (
            enc.frame_size(),
            enc.priming(),
            enc.channels(),
            enc.sample_rate()
        ),
        (256, 0, 1, 44100)
    );
    let mut dec = AlacDecoder::new(&enc.config()).unwrap();
    let mut out = Vec::new();
    for p in &packets {
        out.extend(dec.decode(&p.data).unwrap().channels.remove(0));
    }
    for (a, b) in x.iter().zip(&out) {
        let e = (f64::from(*a) * 32768.0).round().clamp(-32768.0, 32767.0) / 32768.0;
        assert_eq!(f64::from(*b), e);
    }
    assert!(
        enc.encode(&[&x]).is_err(),
        "input after a partial final packet"
    );
    assert!(AlacEncoder::new(AlacEncoderConfig {
        bit_depth: 18,
        ..Default::default()
    })
    .is_err());
    assert!(AlacEncoder::new(AlacEncoderConfig {
        channels: 9,
        ..Default::default()
    })
    .is_err());
}

/// SCE element header for `n` samples (partial), compressed.
fn sce_header(w: &mut BitWriter, n: u32) {
    w.write(0, 3); // SCE
    w.write(0, 4);
    w.write(0, 12);
    w.write(1, 1); // partial
    w.write(0, 2);
    w.write(0, 1);
    w.write(n, 32);
}

#[test]
fn mode15_dse_and_fil_elements() {
    // values 0..16 after the first-order pass are 0, 1, 1, 1, ... (all residuals 1 except the first)
    let n = 16;
    let mut w = BitWriter::new();
    // FIL with 2 bytes, DSE with 3 bytes (byte aligned), then the SCE
    w.write(6, 3);
    w.write(2, 4);
    w.write(0xABCD, 16);
    w.write(4, 3);
    w.write(0, 4);
    w.write(1, 1);
    w.write(3, 8);
    w.byte_align();
    w.write(0x123456, 24);
    sce_header(&mut w, n);
    w.write(0, 8); // mix bits
    w.write(0, 8); // mix res
    w.write(15, 4); // mode 15
    w.write(0, 4); // den shift
    w.write(4, 3); // pb factor
    w.write(0, 5); // no coefficients: the second pass is a copy
                   // Residuals are 0 then 1s (code 2), each written as an escape (9 ones + 16 raw bits) so the
                   // test does not depend on the adaptive Rice parameter. mb = 255 keeps the history above the
                   // zero-run threshold throughout.
    for i in 0..n {
        w.write(0x1FF, 9);
        w.write(if i == 0 { 0 } else { 2 }, 16);
    }
    w.write(7, 3);
    let pkt = w.finish();
    let cfg = AlacConfig {
        num_channels: 1,
        mb: 255,
        ..Default::default()
    };
    let mut dec = AlacDecoder::with_config(cfg).unwrap();
    match dec.decode_i32(&pkt) {
        Ok((pcm, _)) => assert_eq!(pcm[0], (0..n as i32).collect::<Vec<_>>()),
        Err(e) => panic!("{e}"),
    }
    // unsupported element types
    for tag in [2u32, 5] {
        let mut w = BitWriter::new();
        w.write(tag, 3);
        w.write(0, 29);
        assert!(dec.decode_i32(&w.finish()).is_err());
    }
}
