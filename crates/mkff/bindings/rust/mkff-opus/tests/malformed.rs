//! Malformed input must never panic: random packets, truncated and corrupted real packets,
//! and bad headers.

use mkff_audio_core::Decoder;
use mkff_opus::{OpusDecoder, OpusHead};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

fn fixture_packets(name: &str) -> Vec<Vec<u8>> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata/audio/opus")
        .join(name);
    let data = std::fs::read(path).unwrap();
    let mut packets = Vec::new();
    let mut cur = Vec::new();
    let mut pos = 0;
    while pos + 27 <= data.len() {
        let nseg = usize::from(data[pos + 26]);
        let mut body = pos + 27 + nseg;
        for &l in &data[pos + 27..pos + 27 + nseg] {
            cur.extend_from_slice(&data[body..body + usize::from(l)]);
            body += usize::from(l);
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        pos = body;
    }
    packets.split_off(2)
}

fn check_output(buf: &mkff_audio_core::AudioBuffer) {
    for c in &buf.channels {
        assert!(c.iter().all(|v| v.is_finite()), "non-finite output");
    }
}

#[test]
fn random_packets_do_not_panic() {
    let mut rng = Lcg(0x1234_5678);
    let iterations = std::env::var("OPUS_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    for channels in [1u8, 2, 6] {
        let mut d = OpusDecoder::new(&OpusHead::simple(channels)).unwrap();
        for _ in 0..iterations {
            let len = (rng.next() % 300) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if !p.is_empty() && rng.next().is_multiple_of(2) {
                // Plain code 0 packets reach the codec layers more often.
                p[0] &= !3;
            }
            if let Ok(b) = d.decode(&p) {
                check_output(&b);
            }
        }
    }
}

#[test]
fn corrupted_real_packets_do_not_panic() {
    let mut rng = Lcg(42);
    for name in [
        "celt_stereo_20ms.opus",
        "celt_stereo_60ms.opus",
        "silk_mb_60ms.opus",
        "silk_wb_stereo_40ms.opus",
        "hybrid_fb_stereo.opus",
        "surround51.opus",
    ] {
        let packets = fixture_packets(name);
        let channels = if name.starts_with("surround") { 6 } else { 2 };
        let mut d = OpusDecoder::new(&OpusHead::simple(channels)).unwrap();
        for round in 0..40 {
            for p in &packets {
                let mut q = p.clone();
                match round % 4 {
                    0 => q.truncate((rng.next() as usize) % (q.len() + 1)),
                    1 => {
                        for _ in 0..1 + rng.next() % 4 {
                            if !q.is_empty() {
                                let i = rng.next() as usize % q.len();
                                q[i] ^= 1 << (rng.next() % 8);
                            }
                        }
                    }
                    2 => {
                        // Keep the TOC, randomise the payload.
                        for b in q.iter_mut().skip(1) {
                            *b = rng.next() as u8;
                        }
                    }
                    _ => {
                        if q.len() > 1 {
                            let i = 1 + rng.next() as usize % (q.len() - 1);
                            q[i..].fill(if rng.next().is_multiple_of(2) {
                                0
                            } else {
                                0xff
                            });
                        }
                    }
                }
                if let Ok(b) = d.decode(&q) {
                    check_output(&b);
                }
            }
        }
    }
}

#[test]
fn every_toc_with_short_payloads() {
    let mut rng = Lcg(7);
    let mut d = OpusDecoder::new(&OpusHead::simple(2)).unwrap();
    for toc in 0..=255u8 {
        for len in [0usize, 1, 2, 3, 8, 40, 200] {
            let mut p = vec![toc];
            p.extend((0..len).map(|_| rng.next() as u8));
            if let Ok(b) = d.decode(&p) {
                check_output(&b);
                assert_eq!(b.frames(), mkff_opus::packet_samples(&p).unwrap());
            }
        }
        // Loss after any mode.
        let b = d.decode(&[]).unwrap();
        check_output(&b);
    }
}

#[test]
fn lost_packets_are_concealed() {
    for name in [
        "celt_stereo_20ms.opus",
        "silk_nb_mono.opus",
        "hybrid_swb_10ms.opus",
    ] {
        let packets = fixture_packets(name);
        let mut d = OpusDecoder::new(&OpusHead::simple(2)).unwrap();
        for (i, p) in packets.iter().enumerate() {
            let b = if i % 3 == 1 {
                d.decode(&[]).unwrap()
            } else {
                d.decode(p).unwrap()
            };
            check_output(&b);
            assert_eq!(
                b.frames(),
                mkff_opus::packet_samples(p).unwrap(),
                "{name} packet {i}"
            );
        }
    }
}

#[test]
fn multistream_truncation_is_an_error() {
    let packets = fixture_packets("surround51.opus");
    let mut d = OpusDecoder::new(&OpusHead::simple(6)).unwrap();
    // Four streams need at least 2 * 4 - 1 bytes.
    assert!(d.decode(&packets[0][..5]).is_err());
    // Cutting inside a self-delimited stream is detected.
    assert!(d.decode(&packets[0][..20]).is_err());
    assert!(d.decode(&packets[0]).is_ok());
}

#[test]
fn bad_headers() {
    assert!(OpusDecoder::from_config(b"OpusHead").is_err());
    assert!(OpusDecoder::from_config(&[0, 0, 0]).is_err());
    let mut h = OpusHead::simple(6);
    h.mapping[2] = 200;
    assert!(OpusDecoder::new(&h).is_err());
    let mut h = OpusHead::simple(2);
    h.channels = 0;
    assert!(OpusDecoder::new(&h).is_err());
}
