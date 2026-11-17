//! Fixture preparation used once during development (ignored tests, run by hand).
//!
//! `make_nodither_fixture` rewrites a raw AC-3 / E-AC-3 stream so that no zero-bit mantissa is
//! dithered: AC-3 `dithflag` bits are cleared in place; E-AC-3 frames get `dithflage = 1` and
//! explicit zero `dithflag` bits in every block, paid for with unused auxiliary bits. CRCs are
//! recomputed. Reference decoders then produce dither-free output that can be compared to
//! within float precision.
//!
//! With `MKFF_AC3_BLKSW=1`, AC-3 `blksw` bits are also set in a fixed pattern (they do not
//! change the AC-3 syntax), which turns any stream into block-switching test material.
//!
//! ```text
//! MKFF_AC3_IN=in.ac3 MKFF_AC3_OUT=out.ac3 cargo test -p mkff-ac3 --release make_nodither -- --ignored
//! ```

use mkff_audio_core::crc::crc16;

use crate::frame::{Options, Stats, Substream};
use crate::header::{parse_frame, StreamType};
use crate::imdct::Imdct;

fn bits_of(data: &[u8]) -> Vec<u8> {
    data.iter()
        .flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1))
        .collect()
}

fn bytes_of(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
        .collect()
}

/// The 16-bit word `x` such that the CRC of `x || data` is zero.
fn solve_leading_crc(data: &[u8]) -> u16 {
    let mut buf = vec![0u8; data.len() + 2];
    buf[2..].copy_from_slice(data);
    let target = crc16(0, &buf);
    let mut basis = [0u16; 16];
    for (i, b) in basis.iter_mut().enumerate() {
        let mut z = vec![0u8; data.len() + 2];
        z[0] = ((1u16 << i) >> 8) as u8;
        z[1] = (1u16 << i) as u8;
        *b = crc16(0, &z);
    }
    (0..=u16::MAX)
        .find(|x| {
            (0..16)
                .filter(|i| x & (1 << i) != 0)
                .fold(0, |a, i| a ^ basis[i])
                == target
        })
        .expect("CRC solvable")
}

/// `blksw`: also force block switching (AC-3 only, where `blksw` does not change the syntax)
/// in a pattern over blocks and channels. The pattern makes the channels' transforms differ
/// already in the first block: FFmpeg's decoder copies the left delay line into the right one
/// the first time the channels' transforms differ (an upmix of its initially "downmixed"
/// state), which only leaves its output intact while the delay lines are still silent.
pub(crate) fn patch_stream(input: &[u8], blksw: bool) -> Vec<u8> {
    let mut frame_no = 0usize;
    let mut out = Vec::new();
    let mut subs: Vec<Substream> = (0..9).map(|_| Substream::new()).collect();
    let mut imdct = Imdct::new();
    let opt = Options {
        drc_scale: 0.0,
        dither: false,
    };
    let mut pos = crate::header::find_sync(input).expect("sync");
    while pos < input.len() {
        let info = parse_frame(&input[pos..]).expect("frame");
        let frame = &input[pos..pos + info.frame_size];
        pos += info.frame_size;
        let slot = if info.stream_type == StreamType::Dependent {
            1 + info.substream_id
        } else {
            0
        };
        let sub = &mut subs[usize::from(slot)];
        sub.decode_frame(frame, &info, &mut imdct, &opt)
            .expect("decode");
        let nf = sub.nfchans;
        let mut bits = bits_of(frame);
        if !info.is_eac3() {
            for b in 0..6 {
                for ch in 0..nf {
                    bits[sub.block_bits[b] + nf + ch] = 0;
                    if blksw {
                        bits[sub.block_bits[b] + ch] =
                            u8::from((frame_no + b + ch).is_multiple_of(3));
                    }
                }
            }
            frame_no += 1;
            let mut f = bytes_of(&bits);
            let words = f.len() / 2;
            let five8 = ((words >> 1) + (words >> 3)) * 2;
            let c1 = solve_leading_crc(&f[4..five8]);
            f[2..4].copy_from_slice(&c1.to_be_bytes());
            let n = f.len();
            let c2 = crc16(0, &f[five8..n - 2]);
            f[n - 2..].copy_from_slice(&c2.to_be_bytes());
            assert!(crate::header::check_crc(&f, &info));
            out.extend_from_slice(&f);
        } else {
            // audfrm: [expstre ahte] snroffststr(2) transproce blkswe dithflage
            let dpos = sub.audfrm_bit + if info.blocks_per_frame == 6 { 2 } else { 0 } + 4;
            assert_eq!(bits[dpos], 0, "dithflage already set");
            bits[dpos] = 1;
            let blkswe = sub.blkswe;
            let audio_end = sub.block_bits[info.blocks_per_frame];
            let tail = bits.len() - 18;
            let inserted = nf * info.blocks_per_frame;
            assert!(tail >= audio_end + inserted, "not enough padding");
            assert!(bits[audio_end..audio_end + inserted]
                .iter()
                .all(|&b| b == 0));
            let mut nb = Vec::with_capacity(bits.len());
            let mut cursor = 0;
            for b in 0..info.blocks_per_frame {
                let at = sub.block_bits[b] + if blkswe { nf } else { 0 };
                nb.extend_from_slice(&bits[cursor..at]);
                nb.extend(std::iter::repeat_n(0u8, nf));
                cursor = at;
            }
            nb.extend_from_slice(&bits[cursor..audio_end]);
            nb.extend_from_slice(&bits[audio_end + inserted..]);
            assert_eq!(nb.len(), bits.len());
            let mut f = bytes_of(&nb);
            let n = f.len();
            let c = crc16(0, &f[2..n - 2]);
            f[n - 2..].copy_from_slice(&c.to_be_bytes());
            assert!(crate::header::check_crc(&f, &info));
            out.extend_from_slice(&f);
        }
    }
    out
}

#[test]
#[ignore]
fn make_nodither_fixture() {
    let input = std::env::var("MKFF_AC3_IN").expect("MKFF_AC3_IN");
    let output = std::env::var("MKFF_AC3_OUT").expect("MKFF_AC3_OUT");
    let data = std::fs::read(input).unwrap();
    let patched = patch_stream(&data, std::env::var("MKFF_AC3_BLKSW").is_ok());
    std::fs::write(output, patched).unwrap();
}

/// Print coding tool usage of a stream: `MKFF_AC3_IN=x cargo test ... stream_stats -- --ignored --nocapture`.
#[test]
#[ignore]
fn stream_stats() {
    let input = std::fs::read(std::env::var("MKFF_AC3_IN").expect("MKFF_AC3_IN")).unwrap();
    let mut subs: Vec<Substream> = (0..9).map(|_| Substream::new()).collect();
    let mut imdct = Imdct::new();
    let opt = Options {
        drc_scale: 0.0,
        dither: false,
    };
    let mut pos = crate::header::find_sync(&input).expect("sync");
    while pos + 8 < input.len() {
        let info = parse_frame(&input[pos..]).expect("frame");
        let frame = &input[pos..pos + info.frame_size];
        pos += info.frame_size;
        let slot = if info.stream_type == StreamType::Dependent {
            1 + info.substream_id
        } else {
            0
        };
        subs[usize::from(slot)]
            .decode_frame(frame, &info, &mut imdct, &opt)
            .expect("decode");
    }
    for (i, s) in subs.iter().enumerate() {
        if s.stats.blocks > 0 {
            println!("substream slot {i}: {:?} {:?}", s.stats, s.info);
        }
    }
}

/// Turn the frames of an E-AC-3 independent stream into dependent substream frames with a
/// custom channel map (`strmtyp` 1, `chanmape` 1, no converter fields), keeping the audio
/// bits; the frame size is preserved by taking the difference from the unused auxiliary bits.
pub(crate) fn make_dependent(input: &[u8], chanmap: u16) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut sub = Substream::new();
    let mut imdct = Imdct::new();
    let opt = Options {
        drc_scale: 0.0,
        dither: false,
    };
    let mut pos = crate::header::find_sync(input).expect("sync");
    while pos + 8 < input.len() {
        let info = parse_frame(&input[pos..]).expect("frame");
        assert_eq!(info.stream_type, StreamType::Independent);
        let frame = &input[pos..pos + info.frame_size];
        pos += info.frame_size;
        sub.decode_frame(frame, &info, &mut imdct, &opt)
            .expect("decode");
        let bits = bits_of(frame);
        let m = sub.marks;
        // cut points in ascending order: (start, end) ranges to drop
        let mut drops = vec![m.convexp];
        drops.extend_from_slice(&m.convsnr[..info.blocks_per_frame]);
        let mut nb: Vec<u8> = Vec::with_capacity(bits.len() + 32);
        nb.extend_from_slice(&bits[..m.chanmap_at]);
        nb.push(1);
        nb.extend((0..16).rev().map(|i| ((chanmap >> i) & 1) as u8));
        let mut cursor = m.chanmap_at;
        for (a, b) in drops {
            nb.extend_from_slice(&bits[cursor..a]);
            cursor = b;
        }
        let audio_end = sub.block_bits[info.blocks_per_frame];
        nb.extend_from_slice(&bits[cursor..audio_end]);
        let tail = bits.len() - 18;
        let needed = bits.len() - 18 - nb.len();
        assert!(needed <= tail - audio_end || nb.len() <= tail, "no room");
        // fill the auxiliary area with zeros up to the error check fields
        nb.resize(tail, 0);
        nb.extend_from_slice(&bits[tail..]);
        assert_eq!(nb.len(), bits.len());
        // strmtyp = 1
        nb[16] = 0;
        nb[17] = 1;
        let mut f = bytes_of(&nb);
        let n = f.len();
        let c = crc16(0, &f[2..n - 2]);
        f[n - 2..].copy_from_slice(&c.to_be_bytes());
        frames.push(f);
    }
    frames
}

/// Build a 7.1 E-AC-3 stream from a 5.1 independent stream and a stereo stream carrying the
/// rear surrounds: `MKFF_AC3_IN=51.eac3 MKFF_AC3_IN2=rear.eac3 MKFF_AC3_OUT=71.eac3`.
#[test]
#[ignore]
fn make_71_fixture() {
    let main = std::fs::read(std::env::var("MKFF_AC3_IN").unwrap()).unwrap();
    let rear = std::fs::read(std::env::var("MKFF_AC3_IN2").unwrap()).unwrap();
    // chanmap bit 6: Lrs/Rrs pair
    let deps = make_dependent(&rear, 0x8000 >> 6);
    let mut out = Vec::new();
    let mut pos = 0;
    let mut i = 0;
    while pos + 8 < main.len() {
        let info = parse_frame(&main[pos..]).expect("frame");
        out.extend_from_slice(&main[pos..pos + info.frame_size]);
        pos += info.frame_size;
        if let Some(d) = deps.get(i) {
            out.extend_from_slice(d);
        }
        i += 1;
    }
    std::fs::write(std::env::var("MKFF_AC3_OUT").unwrap(), out).unwrap();
}

/// Flip bits in the headers and side information of E-AC-3 frames so that rarely used tools
/// (AHT, spectral extension, block switching syntax, delta bit allocation...) get parsed from
/// garbage: decoding must fail cleanly or produce finite audio, and the tools must be reached.
#[test]
fn fuzz_side_information() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../testdata/audio/ac3/");
    let mut reached = Stats::default();
    let mut x = 0x9E37_79B9u32;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    let rounds: usize = std::env::var("MKFF_AC3_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    for name in ["eac3_stereo_48k.eac3", "eac3_51_48k.eac3", "ac3_51_48k.ac3"] {
        let data = std::fs::read(format!("{root}{name}")).unwrap();
        let info = parse_frame(&data).unwrap();
        let frame = &data[..info.frame_size];
        let mut imdct = Imdct::new();
        let opt = Options {
            drc_scale: 1.0,
            dither: true,
        };
        let mut clean = Substream::new();
        clean.decode_frame(frame, &info, &mut imdct, &opt).unwrap();
        let audfrm = clean.audfrm_bit;
        for round in 0..rounds {
            let mut f = frame.to_vec();
            if info.is_eac3() && round % 2 == 0 {
                // AHT on (ahte is the second audfrm bit), then scramble the strategies after it
                let b = audfrm + 1;
                f[b / 8] |= 0x80 >> (b % 8);
                for _ in 0..4 {
                    let bit = audfrm + 2 + (next() as usize) % 48;
                    f[bit / 8] ^= 0x80 >> (bit % 8);
                }
            }
            for _ in 0..1 + next() % 3 {
                // mostly the first 48 bytes: bsi, audfrm and the first block's side info
                let span = if next() % 4 == 0 { f.len() } else { 48 };
                let bit = 16 + (next() as usize) % ((span - 2) * 8);
                f[bit / 8] ^= 0x80 >> (bit % 8);
            }
            let Some(i) = parse_frame(&f) else { continue };
            if i.frame_size != f.len() {
                continue;
            }
            let mut sub = Substream::new();
            if sub.decode_frame(&f, &i, &mut imdct, &opt).is_ok() {
                for c in &sub.pcm {
                    assert!(c.iter().all(|v| v.is_finite()));
                }
            }
            let s = sub.stats;
            reached.blksw += s.blksw;
            reached.aht += s.aht;
            reached.spx += s.spx;
            reached.delta_ba += s.delta_ba;
            reached.phase_flags += s.phase_flags;
        }
    }
    println!("tools reached in fuzzed frames: {reached:?}");
    assert!(reached.aht > 0 && reached.spx > 0 && reached.blksw > 0);
}

#[test]
#[ignore]
fn bench_parts() {
    use std::time::Instant;
    let mut t = Imdct::new();
    let mut coef = [0f32; 256];
    for (i, c) in coef.iter_mut().enumerate() {
        *c = ((i * 37 % 101) as f32 - 50.0) / 100.0;
    }
    let mut delay = [0f32; 256];
    let mut out = [0f32; 256];
    let n = 200_000;
    let s = Instant::now();
    for _ in 0..n {
        t.transform(&coef, false, &mut delay, &mut out);
    }
    println!(
        "imdct long: {:.2} us",
        s.elapsed().as_secs_f64() * 1e6 / n as f64
    );
    let s = Instant::now();
    for _ in 0..n {
        t.transform(&coef, true, &mut delay, &mut out);
    }
    println!(
        "imdct short: {:.2} us",
        s.elapsed().as_secs_f64() * 1e6 / n as f64
    );
    let mut exp = [0u8; 256];
    for (i, e) in exp.iter_mut().enumerate() {
        *e = (5 + (i * 7) % 13) as u8;
    }
    let p = crate::bitalloc::BaParams {
        sdcycod: 2,
        fdcycod: 1,
        sgaincod: 1,
        dbpbcod: 2,
        floorcod: 7,
        hth: &crate::tables::HTH[0],
    };
    let dba = crate::bitalloc::Dba::default();
    let c = crate::bitalloc::ChannelBa {
        start: 0,
        end: 253,
        fgain: 0x280,
        snroffset: 0,
        leak: None,
        dba: &dba,
        hebap: false,
    };
    let mut bap = [0u8; 256];
    let s = Instant::now();
    for _ in 0..n {
        crate::bitalloc::allocate(std::hint::black_box(&exp), &p, &c, &mut bap);
    }
    println!(
        "bit allocation: {:.2} us",
        s.elapsed().as_secs_f64() * 1e6 / n as f64
    );
    if let Ok(path) = std::env::var("MKFF_AC3_IN") {
        let data = std::fs::read(path).unwrap();
        let mut sub = Substream::new();
        let opt = Options {
            drc_scale: 0.0,
            dither: true,
        };
        let s = Instant::now();
        let mut pos = 0;
        let mut frames = 0;
        while pos + 8 < data.len() {
            let info = parse_frame(&data[pos..]).unwrap();
            sub.decode_frame(&data[pos..pos + info.frame_size], &info, &mut t, &opt)
                .unwrap();
            pos += info.frame_size;
            frames += 1;
        }
        println!(
            "frame decode: {:.2} us / frame",
            s.elapsed().as_secs_f64() * 1e6 / frames as f64
        );
    }
}
