//! Decode the ffmpeg-made fixtures in `testdata/audio/flac` and compare bit-exactly with
//! ffmpeg's own decode (MD5 of its `s16le` / `s32le` output, see the README there) and with the
//! STREAMINFO MD5.

use mkff_audio_core::crc::crc16;
use mkff_audio_core::Decoder;
use mkff_flac::{find_frame_sync, FlacDecoder, Md5, StreamInfo};

fn testdata(rel: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../testdata/");
    std::fs::read(format!("{path}{rel}")).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Split a native FLAC file into its STREAMINFO block and its frames.
fn split(data: &[u8]) -> (&[u8], Vec<&[u8]>) {
    assert!(data.starts_with(b"fLaC"));
    let mut pos = 4;
    let mut si = &data[0..0];
    loop {
        let h = &data[pos..pos + 4];
        let len = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
        if h[0] & 0x7F == 0 {
            si = &data[pos..pos + 4 + len];
        }
        pos += 4 + len;
        if h[0] & 0x80 != 0 {
            break;
        }
    }
    let mut frames = Vec::new();
    while pos < data.len() {
        let mut end = data.len();
        let mut search = pos + 2;
        while let Some(off) = find_frame_sync(&data[search..]) {
            let cand = search + off;
            if crc16(0, &data[pos..cand]) == 0 {
                end = cand;
                break;
            }
            search = cand + 1;
        }
        frames.push(&data[pos..end]);
        pos = end;
    }
    (si, frames)
}

struct Decoded {
    info: StreamInfo,
    channels: Vec<Vec<i32>>,
    frames: usize,
}

fn decode_file(rel: &str) -> Decoded {
    let data = testdata(rel);
    let (si, frames) = split(&data);
    let info = StreamInfo::parse(si).unwrap();
    let mut dec = FlacDecoder::new(si).unwrap();
    let mut channels = vec![Vec::new(); usize::from(info.channels)];
    for (i, f) in frames.iter().enumerate() {
        let (pcm, bps) = dec
            .decode_i32(f)
            .unwrap_or_else(|e| panic!("{rel} frame {i}: {e}"));
        assert_eq!(bps, u32::from(info.bits_per_sample));
        assert_eq!(pcm.len(), channels.len());
        for (c, p) in channels.iter_mut().zip(pcm) {
            c.extend_from_slice(&p);
        }
    }
    assert_eq!(channels[0].len() as u64, info.total_samples, "{rel}");
    Decoded {
        info,
        channels,
        frames: frames.len(),
    }
}

/// MD5 of interleaved little-endian samples, `bytes` bytes each, after `shift` left.
fn pcm_md5(ch: &[Vec<i32>], bytes: usize, shift: u32) -> String {
    let mut h = Md5::new();
    let mut buf = Vec::new();
    for i in 0..ch[0].len() {
        for c in ch {
            buf.extend_from_slice(&(c[i] << shift).to_le_bytes()[..bytes]);
        }
    }
    h.update(&buf);
    Md5::hex(&h.finish())
}

fn check(rel: &str, ffmpeg_md5: &str) -> Decoded {
    let d = decode_file(rel);
    let bps = u32::from(d.info.bits_per_sample);
    // ffmpeg outputs 16-bit streams as s16le and wider ones left-justified in s32le
    let ff = if bps <= 16 {
        pcm_md5(&d.channels, 2, 16 - bps)
    } else {
        pcm_md5(&d.channels, 4, 32 - bps)
    };
    assert_eq!(ff, ffmpeg_md5, "{rel}: differs from ffmpeg's decode");
    let si = pcm_md5(&d.channels, bps.div_ceil(8) as usize, 0);
    assert_eq!(si, Md5::hex(&d.info.md5), "{rel}: STREAMINFO MD5");
    d
}

#[test]
fn tone_mono16() {
    check("codecs/tone.flac", "02e51bdbaa961229fcc721869bb43385");
}

#[test]
fn stereo16_level5() {
    let d = check(
        "audio/flac/stereo16_l5.flac",
        "b3b57b9f3c2b9709e62ddc606b43758c",
    );
    assert_eq!(d.info.sample_rate, 44100);
    assert!(d.frames > 3);
}

#[test]
fn stereo16_lpc_order32_precision15() {
    check(
        "audio/flac/stereo16_lpc32.flac",
        "a23ce5ba92e220bcde7d8ae4adbf8d16",
    );
}

#[test]
fn stereo24_96k() {
    check(
        "audio/flac/stereo24_96k.flac",
        "84b863cae8d1f026c7eae544cae5f1fb",
    );
}

#[test]
fn stereo32_side_channel_33_bits() {
    check(
        "audio/flac/stereo32.flac",
        "5a752567ccd5f6051806937ba337ae4a",
    );
}

#[test]
fn surround51() {
    let d = check(
        "audio/flac/surround51.flac",
        "e54c24cf1982c7562e511f6e7eed00ff",
    );
    assert_eq!(d.channels.len(), 6);
}

#[test]
fn mono_fixed_odd_block_11025() {
    let d = check(
        "audio/flac/mono_fixed_11025.flac",
        "04faca744878e78674283fb588a0db80",
    );
    assert_eq!(d.info.sample_rate, 11025);
}

#[test]
fn wasted_bits_24() {
    check(
        "audio/flac/wasted24.flac",
        "b43096b38a1f641a909bd39781c5a996",
    );
}

#[test]
fn f32_output_and_trait() {
    let data = testdata("audio/flac/stereo16_l5.flac");
    let (si, frames) = split(&data);
    let mut dec = FlacDecoder::new(si).unwrap();
    let mut int = FlacDecoder::new(&[]).unwrap(); // parameters from the frame headers only
    for f in &frames {
        let buf = dec.decode(f).unwrap();
        let (pcm, bps) = int.decode_i32(f).unwrap();
        assert_eq!(bps, 16);
        assert_eq!(buf.sample_rate, 44100);
        assert_eq!(buf.channels.len(), 2);
        for (fc, ic) in buf.channels.iter().zip(&pcm) {
            for (&x, &i) in fc.iter().zip(ic) {
                assert_eq!(x, i as f32 / 32768.0);
            }
        }
    }
    assert_eq!(dec.sample_rate(), 44100);
    assert_eq!(dec.channels(), 2);
    assert_eq!(int.sample_rate(), 44100);
}

#[test]
fn corrupt_and_truncated_frames_error_without_panic() {
    for rel in [
        "audio/flac/stereo16_l5.flac",
        "audio/flac/stereo32.flac",
        "audio/flac/surround51.flac",
    ] {
        let data = testdata(rel);
        let (si, frames) = split(&data);
        let mut dec = FlacDecoder::new(si).unwrap();
        let f = frames[1];
        // every truncation fails (the CRC-16 sits at the end)
        for len in (0..f.len()).step_by(7).chain(f.len() - 3..f.len()) {
            assert!(
                dec.decode_i32(&f[..len]).is_err(),
                "{rel} truncated to {len}"
            );
        }
        // bit flips: must not panic; anything that decodes must still pass the CRC
        let mut seed = 0x1234_5678u32;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let mut g = f.to_vec();
            let pos = (seed >> 8) as usize % g.len();
            g[pos] ^= 1 << (seed % 8);
            let _ = dec.decode(&g);
        }
        // the decoder still works afterwards
        assert!(dec.decode_i32(f).is_ok());
    }
}

#[test]
fn random_input_never_panics() {
    let mut dec = FlacDecoder::new(&[]).unwrap();
    let mut seed = 99u64;
    let mut buf = vec![0u8; 600];
    for round in 0..3000 {
        for b in buf.iter_mut() {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *b = (seed >> 56) as u8;
        }
        // start with a plausible sync code half of the time
        if round % 2 == 0 {
            buf[0] = 0xFF;
            buf[1] = 0xF8;
        }
        let len = 1 + round % buf.len();
        let _ = dec.decode(&buf[..len]);
        let _ = find_frame_sync(&buf[..len]);
        let _ = StreamInfo::parse(&buf[..len]);
    }
}
