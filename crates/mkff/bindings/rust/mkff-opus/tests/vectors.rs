//! Development check against the official Opus test vectors (RFC 6716 §A.4 / RFC 8251 §11).
//!
//! The vectors are not committed (about 75 MB). To run:
//! `OPUS_TESTVECTORS=/path/to/opus_newvectors cargo test -p mkff-opus --release -- --ignored`
//!
//! Each `.bit` file is a sequence of `[u32 BE length][u32 BE final range][packet]` records; the
//! `.dec` file is the reference 48 kHz stereo 16-bit decode.

use mkff_audio_core::Decoder;
use mkff_opus::{OpusDecoder, OpusHead};

fn snr_db(reference: &[f32], got: &[f32]) -> f64 {
    let mut sig = 0f64;
    let mut err = 0f64;
    for (r, g) in reference.iter().zip(got) {
        sig += f64::from(*r) * f64::from(*r);
        err += (f64::from(*r) - f64::from(*g)).powi(2);
    }
    if err == 0.0 {
        return 200.0;
    }
    10.0 * (sig / err).log10()
}

#[test]
#[ignore]
fn official_test_vectors() {
    let Ok(dir) = std::env::var("OPUS_TESTVECTORS") else {
        eprintln!("OPUS_TESTVECTORS not set; skipping");
        return;
    };
    let only = std::env::var("OPUS_VECTOR").ok();
    for v in 1..=12 {
        let name = format!("testvector{v:02}");
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let bit = std::fs::read(format!("{dir}/{name}.bit")).expect("read .bit");
        let dec = std::fs::read(format!("{dir}/{name}.dec")).expect("read .dec");
        let reference: Vec<f32> = dec
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
            .collect();
        let mut d = OpusDecoder::new(&OpusHead::simple(2)).unwrap();
        let mut out: Vec<f32> = Vec::new();
        let mut pos = 0;
        let mut packets = 0;
        let mut range_mismatch = 0;
        let mut first_mismatch = None;
        while pos + 8 <= bit.len() {
            let len = u32::from_be_bytes(bit[pos..pos + 4].try_into().unwrap()) as usize;
            let rng = u32::from_be_bytes(bit[pos + 4..pos + 8].try_into().unwrap());
            pos += 8;
            let pkt = &bit[pos..pos + len];
            pos += len;
            let buf = d.decode(pkt).expect("decode");
            if len > 0 && d.final_range() != rng {
                range_mismatch += 1;
                if first_mismatch.is_none() {
                    first_mismatch = Some((packets, pkt[0] >> 3, pkt.len()));
                }
            }
            if std::env::var("OPUS_TOC").is_ok() && packets < 400 {
                println!(
                    "pkt {packets} at sample {}: toc config {} stereo {} code {} len {}",
                    out.len() / 2,
                    pkt.first().map_or(0, |b| b >> 3),
                    pkt.first().map_or(0, |b| (b >> 2) & 1),
                    pkt.first().map_or(0, |b| b & 3),
                    len
                );
            }
            out.extend(buf.interleaved());
            packets += 1;
        }
        let n = out.len().min(reference.len());
        if std::env::var("OPUS_BLOCKS").is_ok() {
            for b in 0..16 {
                let r = &reference[b * 1920..(b + 1) * 1920];
                let o = &out[b * 1920..(b + 1) * 1920];
                let md = r
                    .iter()
                    .zip(o)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0f32, f32::max);
                let mr = r.iter().map(|a| a.abs()).fold(0f32, f32::max);
                println!("  block {b}: max ref {mr:.5} max diff {md:.6}");
                if b == 1 {
                    let idx: Vec<usize> = (0..1920)
                        .step_by(2)
                        .filter(|&i| r[i] != o[i])
                        .map(|i| i / 2)
                        .take(20)
                        .collect();
                    println!("    diffs at {idx:?}");
                    let i0 = idx.first().copied().unwrap_or(0);
                    println!(
                        "    got  {:?}",
                        (i0.saturating_sub(2)..i0 + 6)
                            .map(|i| (o[2 * i] * 32768.0) as i32)
                            .collect::<Vec<_>>()
                    );
                    println!(
                        "    want {:?}",
                        (i0.saturating_sub(2)..i0 + 6)
                            .map(|i| (r[2 * i] * 32768.0) as i32)
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
        if let Some(nz) = (0..n).find(|&i| reference[i] != 0.0) {
            println!("  first nonzero reference sample {}", nz / 2);
        }
        if let Some(i) = (0..n).find(|&i| (out[i] - reference[i]).abs() > 2e-3) {
            println!(
                "  first divergence at sample {} (ch {}): got {} want {}",
                i / 2,
                i % 2,
                out[i],
                reference[i]
            );
            let s = i / 2;
            let a = s.saturating_sub(3) * 2;
            println!("  got  {:?}", &out[a..(a + 16).min(n)]);
            println!("  want {:?}", &reference[a..(a + 16).min(n)]);
        }
        let snr = snr_db(&reference[..n], &out[..n]);
        println!(
            "{name}: {packets} packets, {} range mismatches (first {:?}), samples {} vs {}, SNR {snr:.2} dB",
            range_mismatch,
            first_mismatch,
            out.len() / 2,
            reference.len() / 2
        );
    }
}

/// Development fuzzing: decode bit-flipped / truncated packets of the official vectors.
#[test]
#[ignore]
fn fuzz_test_vector_packets() {
    let Ok(dir) = std::env::var("OPUS_TESTVECTORS") else {
        return;
    };
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let rounds: usize = std::env::var("OPUS_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    for v in 1..=12 {
        let bit = std::fs::read(format!("{dir}/testvector{v:02}.bit")).unwrap();
        let mut packets = Vec::new();
        let mut pos = 0;
        while pos + 8 <= bit.len() {
            let len = u32::from_be_bytes(bit[pos..pos + 4].try_into().unwrap()) as usize;
            packets.push(bit[pos + 8..pos + 8 + len].to_vec());
            pos += 8 + len;
        }
        for r in 0..rounds {
            let mut d = OpusDecoder::new(&OpusHead::simple(2)).unwrap();
            for p in &packets {
                let mut q = p.clone();
                match (r + rnd() as usize) % 5 {
                    0 => q.truncate(rnd() as usize % (q.len() + 1)),
                    1 | 2 => {
                        for _ in 0..1 + rnd() % 3 {
                            if !q.is_empty() {
                                let i = rnd() as usize % q.len();
                                q[i] ^= 1 << (rnd() % 8);
                            }
                        }
                    }
                    3 => {
                        for b in q.iter_mut().skip(1) {
                            *b = rnd() as u8;
                        }
                    }
                    _ => q.clear(),
                }
                if let Ok(b) = d.decode(&q) {
                    assert!(b.channels.iter().flatten().all(|v| v.is_finite()));
                }
            }
        }
        println!(
            "testvector{v:02}: fuzzed {} packets x {rounds}",
            packets.len()
        );
    }
}
