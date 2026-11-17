//! Decode the ffmpeg-made ALAC fixtures and compare bit-exactly with ffmpeg's own decode (MD5 of
//! its `s16le` / `s32le` output; see `testdata/audio/alac/README.md`).

mod common;

use common::{md5_hex, read_mp4};
use mkff_alac::{AlacConfig, AlacDecoder};
use mkff_audio_core::Decoder;

fn testdata(rel: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../testdata/");
    std::fs::read(format!("{path}{rel}")).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

struct Decoded {
    cfg: AlacConfig,
    channels: Vec<Vec<i32>>,
    packets: usize,
}

fn decode_file(rel: &str) -> Decoded {
    let track = read_mp4(&testdata(rel));
    let mut dec = AlacDecoder::new(&track.cookie).unwrap();
    let cfg = *dec.config();
    let mut channels = vec![Vec::new(); usize::from(cfg.num_channels)];
    for (i, p) in track.samples.iter().enumerate() {
        let (pcm, bps) = dec
            .decode_i32(p)
            .unwrap_or_else(|e| panic!("{rel} packet {i}: {e}"));
        assert_eq!(bps, u32::from(cfg.bit_depth));
        for (c, s) in channels.iter_mut().zip(pcm) {
            c.extend_from_slice(&s);
        }
    }
    Decoded {
        cfg,
        channels,
        packets: track.samples.len(),
    }
}

/// ffmpeg's interleaved output: s16le for 16-bit, left-justified s32le otherwise.
fn ffmpeg_md5(d: &Decoded) -> String {
    let bps = u32::from(d.cfg.bit_depth);
    let mut buf = Vec::new();
    for i in 0..d.channels[0].len() {
        for c in &d.channels {
            if bps == 16 {
                buf.extend_from_slice(&(c[i] as i16).to_le_bytes());
            } else {
                buf.extend_from_slice(&(c[i] << (32 - bps)).to_le_bytes());
            }
        }
    }
    md5_hex(&buf)
}

fn check(rel: &str, md5: &str, channels: usize, bits: u8) -> Decoded {
    let d = decode_file(rel);
    assert_eq!(d.channels.len(), channels, "{rel}");
    assert_eq!(d.cfg.bit_depth, bits, "{rel}");
    assert_eq!(ffmpeg_md5(&d), md5, "{rel}: differs from ffmpeg's decode");
    d
}

#[test]
fn tone_mono16() {
    check(
        "codecs/tone_alac.m4a",
        "02e51bdbaa961229fcc721869bb43385",
        1,
        16,
    );
}

#[test]
fn stereo16() {
    // same source as the FLAC fixture stereo16_l5.flac: same PCM, same MD5
    let d = check(
        "audio/alac/stereo16.m4a",
        "b3b57b9f3c2b9709e62ddc606b43758c",
        2,
        16,
    );
    assert!(d.packets > 5);
    assert_eq!(d.channels[0].len(), 22050);
}

#[test]
fn stereo24_shifted_bytes() {
    check(
        "audio/alac/stereo24.m4a",
        "c6d3d02701017d5045b27ce2f6a73c43",
        2,
        24,
    );
}

#[test]
fn mono24_predictor_order30() {
    check(
        "audio/alac/mono24_order30.m4a",
        "885de01a0710be5b8d451bb53d8515f2",
        1,
        24,
    );
}

#[test]
fn stereo16_uncompressed_escape() {
    check(
        "audio/alac/stereo16_uncompressed.m4a",
        "c945692dd2439426911e821a925106ef",
        2,
        16,
    );
}

#[test]
fn surround30() {
    check(
        "audio/alac/surround30.m4a",
        "84223d2df51f581feeda9ed843d42897",
        3,
        16,
    );
}

#[test]
fn surround51() {
    check(
        "audio/alac/surround51.m4a",
        "aecf7148a3364fac2b34675a47a8c6ab",
        6,
        16,
    );
}

#[test]
fn surround61() {
    check(
        "audio/alac/surround61.m4a",
        "86fb711b87c1c6b5bfbe6b808ff24275",
        7,
        16,
    );
}

#[test]
fn f32_output_and_bare_cookie() {
    let track = read_mp4(&testdata("audio/alac/stereo16.m4a"));
    let mut dec = AlacDecoder::new(&track.cookie).unwrap();
    // the bare 24-byte ALACSpecificConfig works the same
    let bare = dec.config().to_bytes();
    let mut int = AlacDecoder::new(&bare).unwrap();
    assert_eq!(dec.sample_rate(), 44100);
    assert_eq!(dec.channels(), 2);
    for p in &track.samples {
        let buf = dec.decode(p).unwrap();
        let (pcm, _) = int.decode_i32(p).unwrap();
        assert_eq!(buf.sample_rate, 44100);
        for (fc, ic) in buf.channels.iter().zip(&pcm) {
            assert_eq!(fc.len(), ic.len());
            for (&x, &i) in fc.iter().zip(ic) {
                assert_eq!(x, i as f32 / 32768.0);
            }
        }
    }
}

#[test]
fn damaged_packets_error_without_panic() {
    for rel in [
        "audio/alac/stereo16.m4a",
        "audio/alac/stereo24.m4a",
        "audio/alac/surround51.m4a",
    ] {
        let track = read_mp4(&testdata(rel));
        let mut dec = AlacDecoder::new(&track.cookie).unwrap();
        let p = &track.samples[1];
        for len in 0..p.len().min(64) {
            assert!(
                dec.decode_i32(&p[..len]).is_err(),
                "{rel} truncated to {len}"
            );
        }
        for len in (64..p.len()).step_by(13) {
            let _ = dec.decode_i32(&p[..len]);
        }
        let mut seed = 0xACE1u32;
        for _ in 0..3000 {
            let mut g = p.clone();
            for _ in 0..1 + seed % 3 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let pos = (seed >> 8) as usize % g.len();
                g[pos] ^= 1 << (seed % 8);
            }
            let _ = dec.decode(&g);
        }
        assert!(dec.decode_i32(p).is_ok());
    }
}

#[test]
fn random_packets_never_panic() {
    let cfgs = [
        AlacConfig::default(),
        AlacConfig {
            bit_depth: 24,
            num_channels: 6,
            ..Default::default()
        },
        AlacConfig {
            bit_depth: 32,
            num_channels: 1,
            ..Default::default()
        },
        AlacConfig {
            bit_depth: 20,
            num_channels: 8,
            frame_length: 300,
            ..Default::default()
        },
    ];
    let mut seed = 5u64;
    let mut buf = vec![0u8; 900];
    for cfg in cfgs {
        let mut dec = AlacDecoder::with_config(cfg).unwrap();
        for round in 0..3000 {
            for b in buf.iter_mut() {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *b = (seed >> 56) as u8;
            }
            // plausible element headers half of the time: SCE / CPE tag, zero unused bits
            if round % 2 == 0 {
                buf[0] &= 0x3F;
                buf[1] = 0;
                buf[2] &= 0x0F;
            }
            let len = 1 + round % buf.len();
            let _ = dec.decode(&buf[..len]);
            let _ = AlacConfig::parse(&buf[..len % 40]);
        }
    }
}
