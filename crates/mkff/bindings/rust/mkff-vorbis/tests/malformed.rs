//! Malformed, truncated and random input: the decoder must return errors (or decode garbage),
//! never panic. Also exercises floor 0 and residue types 0 / 1 through a synthetic stream.

mod common;

use common::*;
use mkff_audio_core::Decoder;
use mkff_vorbis::{Comments, IdentHeader, VorbisDecoder};
use std::collections::HashMap;

/// xorshift64* PRNG.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn ident(channels: u8, bs: u8) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(b"vorbis");
    v.extend_from_slice(&0u32.to_le_bytes());
    v.push(channels);
    v.extend_from_slice(&8000u32.to_le_bytes());
    v.extend_from_slice(&[0; 12]);
    v.push(bs);
    v.push(1);
    v
}

/// Options of the synthetic setup header.
#[derive(Clone, Copy, Default)]
struct Kind {
    floor0: bool,
    residue: u64,
}

/// A small valid setup header for 2 channels with blocksizes 128 / 256; any named field can be
/// overridden to make it invalid.
fn build_setup(kind: Kind, over: &HashMap<&str, u64>) -> Vec<u8> {
    let mut w = LsbWriter::default();
    let mut f = |name: &str, v: u64, bits: u32| {
        w.put(*over.get(name).unwrap_or(&v), bits);
    };
    for b in [
        5u64,
        b'v' as u64,
        b'o' as u64,
        b'r' as u64,
        b'b' as u64,
        b'i' as u64,
        b's' as u64,
    ] {
        f("", b, 8);
    }
    f("book_count", 2, 8); // 3 books
                           // book 0: scalar, 2 entries, lengths 1 1
    f("sync0", 0x56_4342, 24);
    f("dims0", 1, 16);
    f("entries0", 2, 24);
    f("ordered0", 0, 1);
    f("sparse0", 0, 1);
    f("len0a", 0, 5);
    f("len0b", 0, 5);
    f("lookup0", 0, 4);
    // book 1: VQ type 2, 4 entries × 2 dims, lengths 2 2 2 2, values 0..7 × 0.25 - 1
    f("sync1", 0x56_4342, 24);
    f("dims1", 2, 16);
    f("entries1", 4, 24);
    f("ordered1", 0, 1);
    f("sparse1", 0, 1);
    for i in 0..4 {
        f(["len1a", "len1b", "len1c", "len1d"][i], 1, 5);
    }
    f("lookup1", 2, 4);
    f("min1", (1 << 31) | (788 << 21) | 1, 32); // -1.0
    f("delta1", ((788 - 2) << 21) | 1, 32); // 0.25
    f("valbits1", 3, 4);
    f("seq1", 0, 1);
    for i in 0..8 {
        f("mult1", i, 4);
    }
    // book 2: VQ type 1, ordered lengths: 9 entries of length 4..., 2 dims → 3 values per dim
    f("sync2", 0x56_4342, 24);
    f("dims2", 2, 16);
    f("entries2", 9, 24);
    f("ordered2", 1, 1);
    f("ordlen2", 2, 5); // length 3
    f("ordnum2a", 7, 4); // 7 entries of length 3 (ilog(9) = 4 bits)
    f("ordnum2b", 2, 2); // 2 entries of length 4 (ilog(2) = 2 bits)
    f("lookup2", 1, 4);
    f("min2", 0, 32);
    f("delta2", ((788 - 3) << 21) | 1, 32); // 0.125
    f("valbits2", 1, 4);
    f("seq2", 1, 1);
    for i in 0..3 {
        f("mult2", i, 2);
    }
    // time domain
    f("times", 0, 6);
    f("time_type", 0, 16);
    // one floor
    f("floors", 0, 6);
    if kind.floor0 {
        f("floor_type", 0, 16);
        f("f0_order", 4, 8);
        f("f0_rate", 8000, 16);
        f("f0_bark", 32, 16);
        f("f0_ampbits", 6, 6);
        f("f0_ampoff", 40, 8);
        f("f0_nbooks", 0, 4);
        f("f0_book", 2, 8);
    } else {
        f("floor_type", 1, 16);
        f("f1_partitions", 2, 5);
        f("f1_class_a", 0, 4);
        f("f1_class_b", 1, 4);
        // class 0: 2 dims, no subclasses, book 0
        f("f1_c0_dims", 1, 3);
        f("f1_c0_sub", 0, 2);
        f("f1_c0_book", 1, 8);
        // class 1: 1 dim, 1 subclass bit, masterbook 0, subbooks none / 0
        f("f1_c1_dims", 0, 3);
        f("f1_c1_sub", 1, 2);
        f("f1_c1_master", 0, 8);
        f("f1_c1_book_a", 0, 8);
        f("f1_c1_book_b", 1, 8);
        f("f1_mult", 1, 2);
        f("f1_rangebits", 7, 4);
        f("f1_x_a", 20, 7);
        f("f1_x_b", 50, 7);
        f("f1_x_c", 90, 7);
    }
    // one residue
    f("residues", 0, 6);
    f("res_type", kind.residue, 16);
    f("res_begin", 0, 24);
    f("res_end", 128, 24);
    f("res_psize", 7, 24);
    f("res_classes", 1, 6);
    f("res_classbook", 0, 8);
    f("res_casc0_low", 0, 3);
    f("res_casc0_flag", 0, 1);
    f("res_casc1_low", 0b101, 3);
    f("res_casc1_flag", 0, 1);
    f("res_book_p0", 1, 8);
    f("res_book_p2", 1, 8);
    // one mapping with coupling 0 / 1
    f("mappings", 0, 6);
    f("map_type", 0, 16);
    f("map_submaps_flag", 0, 1);
    f("map_coupling_flag", 1, 1);
    f("map_steps", 0, 8);
    f("map_mag", 0, 1);
    f("map_ang", 1, 1);
    f("map_reserved", 0, 2);
    f("map_time", 0, 8);
    f("map_floor", 0, 8);
    f("map_residue", 0, 8);
    // two modes
    f("modes", 1, 6);
    for m in 0..2u64 {
        f("mode_blockflag", m, 1);
        f("mode_window", 0, 16);
        f("mode_transform", 0, 16);
        f("mode_mapping", 0, 8);
    }
    f("framing", 1, 1);
    w.bytes
}

const BS: u8 = (8 << 4) | 7; // 128 / 256

fn no_over() -> HashMap<&'static str, u64> {
    HashMap::new()
}

#[test]
fn synthetic_setup_is_valid() {
    for kind in [
        Kind {
            floor0: false,
            residue: 0,
        },
        Kind {
            floor0: false,
            residue: 1,
        },
        Kind {
            floor0: false,
            residue: 2,
        },
        Kind {
            floor0: true,
            residue: 0,
        },
        Kind {
            floor0: true,
            residue: 2,
        },
    ] {
        let setup = build_setup(kind, &no_over());
        let dec = VorbisDecoder::from_headers(&ident(2, BS), &setup).unwrap();
        assert!(dec.is_ready());
        // every strict prefix of the setup header is rejected
        for n in 0..setup.len() {
            assert!(
                VorbisDecoder::from_headers(&ident(2, BS), &setup[..n]).is_err(),
                "prefix {n}"
            );
        }
    }
}

#[test]
fn invalid_setup_fields_are_rejected() {
    let cases: &[(&str, u64)] = &[
        ("sync1", 0x56_4343),
        ("lookup0", 3),      // unknown lookup type
        ("dims1", 0),        // VQ book with zero dimensions
        ("len1a", 0),        // lengths 1 2 2 2: overspecified Huffman tree
        ("len1d", 2),        // lengths 2 2 2 3: underspecified, tolerated (see below)
        ("book_count", 200), // more books than present → truncated
        ("ordnum2a", 8),     // ordered lengths: 8 + 2 > 9 entries → overrun
        ("ordnum2a", 15),    // more than the remaining entries
        ("time_type", 1),
        ("floor_type", 2),
        ("f1_c0_book", 9),   // subclass book 8 out of range
        ("f1_c1_master", 3), // masterbook out of range
        ("f1_x_b", 20),      // duplicate X
        ("res_type", 3),
        ("res_classbook", 3), // classbook out of range
        ("res_book_p0", 0),   // VQ use of a scalar-only book
        ("res_book_p2", 7),
        ("map_type", 1),
        ("map_ang", 0), // coupling a channel with itself
        ("map_reserved", 1),
        ("map_floor", 1),
        ("map_residue", 4),
        ("mode_window", 1),
        ("mode_mapping", 1),
        ("framing", 0),
    ];
    for &(field, value) in cases {
        let over: HashMap<&str, u64> = [(field, value)].into_iter().collect();
        let setup = build_setup(Kind::default(), &over);
        let res = VorbisDecoder::from_headers(&ident(2, BS), &setup);
        if field == "len1d" {
            assert!(res.is_ok(), "{field}: underspecified trees are tolerated");
        } else {
            assert!(res.is_err(), "{field} = {value} accepted");
        }
    }
    // floor 0 with a scalar-only book, and zero order
    for (field, value) in [("f0_book", 0u64), ("f0_order", 0), ("f0_bark", 0)] {
        let over: HashMap<&str, u64> = [(field, value)].into_iter().collect();
        let setup = build_setup(
            Kind {
                floor0: true,
                residue: 0,
            },
            &over,
        );
        assert!(
            VorbisDecoder::from_headers(&ident(2, BS), &setup).is_err(),
            "{field}"
        );
    }
    // one channel: the coupling step can only name channel 0 twice
    assert!(
        VorbisDecoder::from_headers(&ident(1, BS), &build_setup(Kind::default(), &no_over()))
            .is_err()
    );
}

/// Huge sizes from a few header bits must be refused quickly instead of allocating.
#[test]
fn huge_codebooks_are_refused() {
    let t = std::time::Instant::now();
    // 2^24 - 1 entries in one ordered run of length 24
    let mut w = LsbWriter::default();
    w.put(5, 8);
    for &c in b"vorbis" {
        w.put(u64::from(c), 8);
    }
    w.put(0, 8);
    w.put(0x56_4342, 24)
        .put(1, 16)
        .put((1 << 24) - 1, 24)
        .put(1, 1)
        .put(23, 5)
        .put((1 << 24) - 1, 24);
    w.put(0, 4);
    w.put(0, 64);
    assert!(VorbisDecoder::from_headers(&ident(2, BS), &w.bytes).is_err());

    // 65535-dimension VQ book with lookup type 1 and 1000 entries: 65M values
    let mut w = LsbWriter::default();
    w.put(5, 8);
    for &c in b"vorbis" {
        w.put(u64::from(c), 8);
    }
    w.put(0, 8);
    w.put(0x56_4342, 24)
        .put(65535, 16)
        .put(1000, 24)
        .put(1, 1)
        .put(9, 5)
        .put(1000, 10);
    w.put(1, 4)
        .put(0, 32)
        .put(0, 32)
        .put(0, 4)
        .put(0, 1)
        .put(0, 8);
    assert!(VorbisDecoder::from_headers(&ident(2, BS), &w.bytes).is_err());

    // 4096 dims × 16 entries type 2 values without the bits to back them
    let over: HashMap<&str, u64> = [("dims1", 4096u64)].into_iter().collect();
    assert!(
        VorbisDecoder::from_headers(&ident(2, BS), &build_setup(Kind::default(), &over)).is_err()
    );
    assert!(t.elapsed().as_secs_f64() < 2.0);
}

/// Random audio packets through every synthetic configuration: no panics, consistent lengths.
#[test]
fn random_packets_on_synthetic_streams() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for kind in [
        Kind {
            floor0: false,
            residue: 0,
        },
        Kind {
            floor0: false,
            residue: 1,
        },
        Kind {
            floor0: false,
            residue: 2,
        },
        Kind {
            floor0: true,
            residue: 0,
        },
        Kind {
            floor0: true,
            residue: 1,
        },
    ] {
        let setup = build_setup(kind, &no_over());
        let mut dec = VorbisDecoder::from_headers(&ident(2, BS), &setup).unwrap();
        let mut prev: Option<Vec<u8>> = None;
        let mut nonzero = false;
        for _ in 0..3000 {
            let len = rng.below(64);
            let mut p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if let Some(b) = p.first_mut() {
                *b &= !1; // audio packet
            }
            let out = dec.decode(&p).unwrap_or_default();
            if out.frames() > 0 {
                assert_eq!(out.channel_count(), 2);
                let want = dec.packet_samples(prev.as_ref().unwrap(), &p).unwrap();
                assert_eq!(out.frames(), want);
                nonzero |= out.channels.iter().flatten().any(|&s| s != 0.0);
            }
            if dec.packet_block_size(&p).is_some() {
                prev = Some(p);
            }
        }
        assert!(nonzero, "synthetic stream never produced audio");
    }
}

/// A hand-built floor 0 packet decodes to a finite, nonzero signal.
#[test]
fn floor0_packet_decodes() {
    let setup = build_setup(
        Kind {
            floor0: true,
            residue: 1,
        },
        &no_over(),
    );
    let mut dec = VorbisDecoder::from_headers(&ident(2, BS), &setup).unwrap();
    let packet = || {
        let mut w = LsbWriter::default();
        w.put(0, 1); // audio
        w.put(0, 1); // mode 0: short block (128)
        for _ in 0..2 {
            w.put(40, 6); // amplitude
            w.put(0, 1); // book number (ilog(1) = 1 bit)
                         // 4 LSP coefficients = 2 VQ vectors from book 2 (codewords 100, 101: entries 4, 5)
            w.put(0b001, 3).put(0b101, 3);
        }
        // residue 1 on both channels: class codes, then vectors from book 1 (2-bit codes)
        for _ in 0..8 {
            w.put(1, 1).put(1, 1);
            for _ in 0..8 {
                w.put(0b10, 2);
            }
        }
        w.bytes
    };
    assert!(dec.decode(&packet()).unwrap().is_empty());
    let out = dec.decode(&packet()).unwrap();
    assert_eq!(out.frames(), 64);
    let energy: f64 = out
        .channels
        .iter()
        .flatten()
        .map(|&s| f64::from(s) * f64::from(s))
        .sum();
    assert!(energy > 0.0 && energy.is_finite());
}

/// Corrupted real setup headers (random byte changes) and truncated / corrupted audio packets.
#[test]
fn corrupted_real_streams() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for name in [
        "stereo_22k_q0.ogg",
        "surround51_48k_q2.ogg",
        "stereo_44k_ffenc.ogg",
    ] {
        let ogg = read_ogg(&fixture(name));
        let (id, com, setup) = (&ogg.packets[0], &ogg.packets[1], &ogg.packets[2]);
        let audio = &ogg.packets[3..40];

        for n in 0..id.len() {
            assert!(IdentHeader::parse(&id[..n]).is_err());
        }
        for n in 0..com.len() {
            let _ = Comments::parse(&com[..n]);
        }
        for n in (0..setup.len()).step_by(7) {
            assert!(
                VorbisDecoder::from_headers(id, &setup[..n]).is_err(),
                "{name}: setup prefix {n}"
            );
        }

        for _ in 0..300 {
            let mut s = setup.clone();
            for _ in 0..1 + rng.below(4) {
                let i = 7 + rng.below(s.len() - 7);
                s[i] ^= 1 << rng.below(8);
            }
            if let Ok(mut dec) = VorbisDecoder::from_headers(id, &s) {
                for p in audio.iter().take(6) {
                    let _ = dec.decode(p);
                }
            }
        }

        let mut dec = VorbisDecoder::from_headers(id, setup).unwrap();
        for p in audio {
            let mut q = p.clone();
            match rng.below(3) {
                0 => q.truncate(rng.below(q.len() + 1)),
                1 => {
                    let i = rng.below(q.len());
                    q[i] ^= 1 << rng.below(8);
                }
                _ => {
                    let i = rng.below(q.len());
                    for b in &mut q[i..] {
                        *b = rng.next() as u8;
                    }
                }
            }
            if let Ok(out) = dec.decode(&q) {
                assert_eq!(out.channel_count(), dec.channels());
            }
        }
    }
}

#[test]
fn garbage_configs_and_packets() {
    let mut rng = Rng(42);
    for _ in 0..2000 {
        let len = rng.below(80);
        let mut cfg: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if let Some(b) = cfg.first_mut() {
            *b = [0u8, 1, 2, 3][rng.below(4)];
        }
        let _ = VorbisDecoder::new(&cfg);
    }
    // Ogg path: audio before headers, setup before ident, garbage headers
    let mut dec = VorbisDecoder::new(&[]).unwrap();
    assert!(dec.decode(&[0, 1, 2, 3]).is_err());
    let mut setup = vec![5u8];
    setup.extend_from_slice(b"vorbis");
    assert!(dec.decode(&setup).is_err());
    let mut bad_id = ident(2, BS);
    bad_id[28] = 0xff;
    assert!(dec.decode(&bad_id).is_err());
    assert_eq!(dec.channels(), 0);
    assert!(dec.decode(&ident(2, BS)).unwrap().is_empty());
    assert_eq!((dec.channels(), dec.sample_rate()), (2, 8000));
    assert!(dec.decode(&setup).is_err());
    assert!(!dec.is_ready());
    // a 2-channel synthetic stream fed through the Ogg path
    assert!(dec
        .decode(&build_setup(Kind::default(), &no_over()))
        .unwrap()
        .is_empty());
    assert!(dec.is_ready());
    for _ in 0..200 {
        let len = rng.below(300);
        let p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let _ = dec.decode(&p);
    }
}
