//! Damaged, truncated and random input must never panic.

mod common;

use common::*;
use mkff_audio_core::Decoder;
use mkff_mp3::{find_sync, parse_header, parse_xing, Mp3Decoder};

const FILES: &[&str] = &[
    "l1_48k_js_384.mp1",
    "l2_44k_js_192_crc.mp2",
    "l2_22k_mono_64.mp2",
    "l3_44k_js_128.mp3",
    "l3_22k_js_32_is.mp3",
    "l3_8k_mono_8.mp3",
    "l3_44k_vbr_xing.mp3",
];

fn feed(data: &[u8], chunk: usize) {
    let mut d = Mp3Decoder::new();
    d.set_crc_check(chunk.is_multiple_of(2));
    for c in data.chunks(chunk.max(1)) {
        if let Ok(b) = d.decode(c) {
            assert!(b.channels.iter().flatten().all(|v| v.is_finite()));
        }
    }
    let _ = parse_xing(data);
    let _ = find_sync(data);
    let _ = parse_header(data);
}

#[test]
fn random_bytes() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for round in 0..200 {
        let len = (rng.next_u64() % 6000) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        // plant plausible headers so the frame decoders see garbage bodies
        if round % 2 == 0 {
            let mut p = 0;
            while p + 4 < data.len() {
                let hdr: [u8; 4] = match rng.next_u64() % 6 {
                    0 => [0xFF, 0xFB, 0x90, 0x64],
                    1 => [0xFF, 0xF3, 0x80, 0xC4],
                    2 => [0xFF, 0xFD, 0x90, 0x04],
                    3 => [0xFF, 0xFF, 0x90, 0x44],
                    4 => [0xFF, 0xE3, 0x18, 0x84],
                    _ => [0xFF, 0xFA, 0x00, 0x30],
                };
                data[p..p + 4].copy_from_slice(&hdr);
                p += 40 + (rng.next_u64() % 600) as usize;
            }
        }
        feed(&data, 1 + (rng.next_u64() % 3000) as usize);
    }
}

#[test]
fn random_bytes_is_an_error() {
    let mut rng = Rng(42);
    let data: Vec<u8> = (0..4000).map(|_| (rng.next_u64() as u8) & 0x7F).collect();
    let mut d = Mp3Decoder::new();
    assert!(d.decode(&data).is_err(), "no sync in the data");
    assert!(d.decode(&[]).is_ok());
}

#[test]
fn truncated_streams() {
    for name in FILES {
        let data = fixture(name);
        let step = (data.len() / 97).max(1);
        for cut in (0..data.len()).step_by(step) {
            feed(&data[..cut], 777);
            // a frame cut short at the end of a packet waits for more data
            let mut d = Mp3Decoder::new();
            let _ = d.decode(&data[..cut]);
            assert!(d.pending_bytes() <= cut);
        }
    }
}

#[test]
fn bit_flips() {
    let mut rng = Rng(7);
    for name in FILES {
        let data = fixture(name);
        for _ in 0..60 {
            let mut d = data.clone();
            for _ in 0..1 + rng.next_u64() % 40 {
                let i = (rng.next_u64() as usize) % d.len();
                d[i] ^= 1 << (rng.next_u64() % 8);
            }
            feed(&d, 512 + (rng.next_u64() % 4096) as usize);
        }
    }
}

#[test]
fn damaged_side_info() {
    // big_values > 288, invalid tables 4 / 14, a reserved block type, huge part2_3_length
    let data = fixture("l3_44k_js_128.mp3");
    let frames = split_frames(&data);
    for pattern in [0xFFu8, 0x00, 0xA5, 0x5A] {
        let mut out = Vec::new();
        for f in &frames {
            let mut f = f.to_vec();
            for b in &mut f[6..20] {
                *b ^= pattern;
            }
            out.extend_from_slice(&f);
        }
        let mut d = Mp3Decoder::new();
        let b = d.decode(&out).unwrap();
        assert_eq!(
            b.frames(),
            frames.len() * 1152,
            "damaged frames keep their duration"
        );
        assert!(b.channels.iter().flatten().all(|v| v.is_finite()));
    }
}
