//! Decoding tests against reference decodes made with FFmpeg (see
//! `testdata/audio/ac3/README.md` for how the fixtures and references were produced).

use mkff_ac3::{
    check_crc, find_sync, parse_dac3, parse_dec3, parse_frame, Ac3Decoder, Channel, Downmix,
    StreamType,
};
use mkff_audio_core::{AudioBuffer, Decoder, Error};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../../testdata/audio/ac3/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A reference decode: `channels` channels, every `step`-th sample frame.
struct Reference {
    channels: usize,
    step: usize,
    samples: Vec<Vec<f32>>,
}

fn reference(name: &str) -> Reference {
    let d = fixture(name);
    assert_eq!(&d[..8], b"MKFFREF1");
    let u32_at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]) as usize;
    let (channels, step, frames) = (u32_at(8), u32_at(12), u32_at(16));
    let mut samples = vec![Vec::with_capacity(frames); channels];
    let body = &d[20..];
    for f in 0..frames {
        for (c, s) in samples.iter_mut().enumerate() {
            let i = (f * channels + c) * 3;
            let v = i32::from_le_bytes([0, body[i], body[i + 1], body[i + 2]]) >> 8;
            s.push(v as f32 / (1 << 23) as f32);
        }
    }
    Reference {
        channels,
        step,
        samples,
    }
}

/// Split a raw stream into packets: an independent syncframe plus its dependent ones.
fn packets(data: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut pos = find_sync(data).expect("sync");
    while pos < data.len() {
        let info = parse_frame(&data[pos..]).expect("frame header");
        let end = pos + info.frame_size;
        assert!(end <= data.len());
        if info.stream_type == StreamType::Dependent {
            out.last_mut().expect("independent frame first").1 = end;
        } else {
            out.push((pos, end));
        }
        pos = end;
    }
    out.into_iter().map(|(a, b)| &data[a..b]).collect()
}

fn decode_all(dec: &mut Ac3Decoder, data: &[u8]) -> AudioBuffer {
    let mut all = AudioBuffer::default();
    for p in packets(data) {
        let buf = dec.decode(p).expect("decode");
        assert_eq!(buf.sample_rate, dec.sample_rate());
        all.append(&buf);
    }
    all
}

/// Per-channel SNR (dB) of `out` against the reference.
fn snr(out: &AudioBuffer, r: &Reference) -> Vec<f64> {
    assert_eq!(out.channels.len(), r.channels, "channel count");
    let n = r.samples[0].len();
    assert!(n * r.step >= out.frames() - r.step, "reference too short");
    out.channels
        .iter()
        .zip(&r.samples)
        .map(|(ch, rf)| {
            let (mut s, mut e) = (0f64, 0f64);
            for (i, &v) in rf.iter().enumerate() {
                let Some(&o) = ch.get(i * r.step) else { break };
                s += f64::from(v) * f64::from(v);
                e += f64::from(v - o) * f64::from(v - o);
            }
            10.0 * (s / e.max(1e-20)).log10()
        })
        .collect()
}

fn check_fixture(stream: &str, refname: &str, layout: &[Channel], rate: u32, min_snr: f64) {
    let data = fixture(stream);
    let r = reference(refname);
    let mut dec = Ac3Decoder::new();
    let t = std::time::Instant::now();
    let out = decode_all(&mut dec, &data);
    let el = t.elapsed().as_secs_f64();
    assert_eq!(dec.layout(), layout, "{stream} layout");
    assert_eq!(dec.channels(), layout.len());
    assert_eq!(dec.sample_rate(), rate);
    let s = snr(&out, &r);
    println!(
        "{stream}: {} samples x {} ch, SNR {:?} dB, {:.0}x real time",
        out.frames(),
        out.channels.len(),
        s.iter()
            .map(|v| (v * 10.0).round() / 10.0)
            .collect::<Vec<_>>(),
        out.frames() as f64 / f64::from(rate) / el
    );
    for (c, v) in s.iter().enumerate() {
        assert!(*v > min_snr, "{stream} channel {c}: SNR {v:.1} dB");
    }
}

use Channel::*;

#[test]
fn ac3_mono_48k() {
    check_fixture("ac3_mono_48k.ac3", "ac3_mono_48k.ref", &[C], 48_000, 90.0);
}

#[test]
fn ac3_mono_44k() {
    check_fixture("ac3_mono_44k.ac3", "ac3_mono_44k.ref", &[C], 44_100, 90.0);
}

#[test]
fn ac3_stereo_48k_coupling_rematrix() {
    check_fixture(
        "ac3_stereo_48k.ac3",
        "ac3_stereo_48k.ref",
        &[L, R],
        48_000,
        90.0,
    );
}

#[test]
fn ac3_stereo_44k() {
    check_fixture(
        "ac3_stereo_44k.ac3",
        "ac3_stereo_44k.ref",
        &[L, R],
        44_100,
        90.0,
    );
}

#[test]
fn ac3_51_48k() {
    check_fixture(
        "ac3_51_48k.ac3",
        "ac3_51_48k.ref",
        &[L, R, C, Lfe, Ls, Rs],
        48_000,
        90.0,
    );
}

#[test]
fn ac3_51_44k() {
    check_fixture(
        "ac3_51_44k.ac3",
        "ac3_51_44k.ref",
        &[L, R, C, Lfe, Ls, Rs],
        44_100,
        90.0,
    );
}

#[test]
fn ac3_block_switching() {
    check_fixture(
        "ac3_blksw_48k.ac3",
        "ac3_blksw_48k.ref",
        &[L, R],
        48_000,
        90.0,
    );
}

#[test]
fn eac3_stereo_48k() {
    check_fixture(
        "eac3_stereo_48k.eac3",
        "eac3_stereo_48k.ref",
        &[L, R],
        48_000,
        90.0,
    );
}

#[test]
fn eac3_51_48k() {
    check_fixture(
        "eac3_51_48k.eac3",
        "eac3_51_48k.ref",
        &[L, R, C, Lfe, Ls, Rs],
        48_000,
        90.0,
    );
}

#[test]
fn eac3_71_dependent_substream() {
    check_fixture(
        "eac3_71_48k.eac3",
        "eac3_71_48k.ref",
        &[L, R, C, Lfe, Lrs, Rrs, Ls, Rs],
        48_000,
        90.0,
    );
}

/// The same stream with the encoder's dither flags left on: decoding with dither stays close
/// to the dither-free reference (the dither replaces zero-bit mantissas with low-level noise),
/// and with dither disabled matches it.
#[test]
fn dither() {
    let data = fixture("ac3_stereo_48k_dither.ac3");
    let r = reference("ac3_stereo_48k.ref");
    let mut dec = Ac3Decoder::new();
    let s = snr(&decode_all(&mut dec, &data), &r);
    println!("dithered: SNR {s:?}");
    assert!(s.iter().all(|&v| v > 50.0 && v < 90.0), "{s:?}");
    let mut dec = Ac3Decoder::new();
    dec.set_dither(false);
    let s = snr(&decode_all(&mut dec, &data), &r);
    assert!(s.iter().all(|&v| v > 90.0), "{s:?}");
}

#[test]
fn whole_stream_as_one_packet_and_reset() {
    let data = fixture("ac3_51_48k.ac3");
    let mut dec = Ac3Decoder::new();
    let a = decode_all(&mut dec, &data);
    dec.reset();
    let b = dec.decode(&data).unwrap();
    assert_eq!(a, b);
    let data = fixture("eac3_71_48k.eac3");
    let mut dec = Ac3Decoder::new();
    let a = decode_all(&mut dec, &data);
    let mut dec = Ac3Decoder::new();
    let b = dec.decode(&data).unwrap();
    assert_eq!(a, b);
}

#[test]
fn downmix() {
    let data = fixture("ac3_51_48k.ac3");
    let mut full = Ac3Decoder::new();
    let f = decode_all(&mut full, &data);
    let mut dec = Ac3Decoder::new();
    dec.set_downmix(Some(Downmix::Stereo));
    let st = decode_all(&mut dec, &data);
    assert_eq!(dec.channels(), 2);
    assert_eq!(dec.layout(), &[L, R]);
    // 3/2 Lo/Ro with the stream's levels (ffmpeg writes cmixlev -3 dB, surmixlev -3 dB... read
    // them from the stream through the documented formula)
    let info = dec.last_frame_info().unwrap();
    let clev = [0.707_106_77f32, 0.594_603_55, 0.5, 0.594_603_55][info.cmixlev.unwrap() as usize];
    let slev = [0.707_106_77f32, 0.5, 0.0, 0.5][info.surmixlev.unwrap() as usize];
    let norm = 1.0 / (1.0 + clev + slev);
    for i in 0..f.frames() {
        let lo = (f.channels[0][i] + clev * f.channels[2][i] + slev * f.channels[4][i]) * norm;
        let ro = (f.channels[1][i] + clev * f.channels[2][i] + slev * f.channels[5][i]) * norm;
        assert!((st.channels[0][i] - lo).abs() < 1e-5);
        assert!((st.channels[1][i] - ro).abs() < 1e-5);
    }
    let mut dec = Ac3Decoder::new();
    dec.set_downmix(Some(Downmix::Mono));
    let mono = decode_all(&mut dec, &data);
    assert_eq!(mono.channels.len(), 1);
    assert_eq!(dec.channels(), 1);
    for i in 0..f.frames() {
        let m = 0.5 * (st.channels[0][i] + st.channels[1][i]);
        assert!((mono.channels[0][i] - m).abs() < 1e-5);
    }
    // a 7.1 programme downmixes its 5.1 core
    let mut dec = Ac3Decoder::new();
    dec.set_downmix(Some(Downmix::Stereo));
    let out = decode_all(&mut dec, &fixture("eac3_71_48k.eac3"));
    assert_eq!(out.channels.len(), 2);
}

#[test]
fn headers() {
    let d = fixture("ac3_51_48k.ac3");
    let i = parse_frame(&d).unwrap();
    assert_eq!(
        (i.bsid, i.sample_rate, i.acmod, i.lfe, i.channels),
        (8, 48_000, 7, true, 6)
    );
    assert_eq!(
        (i.frame_size, i.bit_rate, i.blocks_per_frame),
        (1536, 384_000, 6)
    );
    assert_eq!(i.stream_type, StreamType::Ac3);
    assert!(i.dialnorm >= 1 && i.dialnorm <= 31);
    assert!(check_crc(&d[..i.frame_size], &i));

    let d = fixture("ac3_stereo_44k.ac3");
    let i = parse_frame(&d).unwrap();
    assert_eq!(
        (i.sample_rate, i.acmod, i.channels, i.bit_rate),
        (44_100, 2, 2, 160_000)
    );
    // 160 kbit/s at 44.1 kHz: 348 or 349 words
    assert!(i.frame_size == 696 || i.frame_size == 698);

    let d = fixture("eac3_71_48k.eac3");
    let i = parse_frame(&d).unwrap();
    assert_eq!(i.stream_type, StreamType::Independent);
    assert_eq!(
        (i.bsid, i.acmod, i.lfe, i.blocks_per_frame, i.substream_id),
        (16, 7, true, 6, 0)
    );
    let dep = parse_frame(&d[i.frame_size..]).unwrap();
    assert_eq!(dep.stream_type, StreamType::Dependent);
    assert_eq!((dep.acmod, dep.channels, dep.chanmap), (2, 2, Some(0x0200)));
    assert_eq!(dep.coded_channels(), vec![Lrs, Rrs]);
    assert!(check_crc(&d[..i.frame_size], &i));

    // sync search skips garbage and false syncwords
    let mut junk = vec![0x0B, 0x77, 0x00, 0x11, 0x0B, 0x77, 0xFF, 0xFF, 0x12];
    let off = junk.len();
    junk.extend_from_slice(&fixture("ac3_mono_48k.ac3"));
    assert_eq!(find_sync(&junk), Some(off));
    assert_eq!(find_sync(&[0u8; 100]), None);
    assert!(parse_frame(&[0x0B, 0x77]).is_none());
}

#[test]
fn dac3_dec3() {
    // from testdata/codecs/tone51_ac3.mp4: 48 kHz, bsid 8, 3/2 + LFE, 192 kbit/s
    let a = parse_dac3(&[0x10, 0x3d, 0x40]).unwrap();
    assert_eq!(
        (a.sample_rate, a.bsid, a.acmod, a.lfe, a.channels),
        (48_000, 8, 7, true, 6)
    );
    assert_eq!(a.bit_rate, 192_000);
    assert_eq!(a.layout, vec![L, R, C, Lfe, Ls, Rs]);
    assert!(parse_dac3(&[0x10]).is_none());

    // from testdata/codecs/tone51_eac3.mp4: 192 kbit/s, one 5.1 substream
    let e = parse_dec3(&[0x06, 0x00, 0x20, 0x0f, 0x00, 0x00]).unwrap();
    assert_eq!(e.data_rate, 192);
    assert_eq!(e.substreams.len(), 1);
    assert_eq!((e.channels(), e.sample_rate()), (6, 48_000));
    assert_eq!(e.substreams[0].bsid, 16);

    // 7.1: 3/2 + LFE with one dependent substream carrying Lrs/Rrs (chan_loc bit 1)
    // data_rate 640, num_ind_sub 0 | fscod 0 bsid 16 rsv asvc 0 | bsmod 0 acmod 7 lfe 1 |
    // rsv 3 num_dep_sub 1 chan_loc 0b000000010
    let mut bits = String::new();
    bits += &format!("{:013b}{:03b}", 640, 0);
    bits += &format!(
        "{:02b}{:05b}{:01b}{:01b}{:03b}{:03b}{:01b}",
        0, 16, 0, 0, 0, 7, 1
    );
    bits += &format!("{:03b}{:04b}{:09b}", 0, 1, 0b10);
    while !bits.len().is_multiple_of(8) {
        bits.push('0');
    }
    let bytes: Vec<u8> = (0..bits.len() / 8)
        .map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap())
        .collect();
    let e = parse_dec3(&bytes).unwrap();
    assert_eq!(e.substreams[0].num_dep_sub, 1);
    assert_eq!(e.channels(), 8);
    assert_eq!(e.substreams[0].layout, vec![L, R, C, Lfe, Lrs, Rrs, Ls, Rs]);
    assert!(parse_dec3(&bytes[..3]).is_none());
}

/// Deterministic pseudo-random bytes.
fn noise(seed: u32, n: usize) -> Vec<u8> {
    let mut x = seed.max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[test]
fn malformed_input_never_panics() {
    let files = [
        "ac3_mono_48k.ac3",
        "ac3_51_48k.ac3",
        "ac3_blksw_48k.ac3",
        "eac3_stereo_48k.eac3",
        "eac3_71_48k.eac3",
    ];
    for name in files {
        let data = fixture(name);
        let first = packets(&data)[0].to_vec();
        // every truncation of the first packet: only whole frames decode
        let boundary = parse_frame(&first).unwrap().frame_size;
        let mut dec = Ac3Decoder::new();
        for n in 0..first.len() {
            match dec.decode(&first[..n]) {
                Ok(b) => assert!(n == 0 && b.is_empty() || n == boundary, "{name}: {n}"),
                Err(e) => assert!(matches!(e, Error::EndOfData | Error::InvalidData(_))),
            }
        }
        // random bit flips with the CRC check off: errors or audio, never a panic
        let mut dec = Ac3Decoder::new();
        dec.set_crc_check(false);
        let flips = noise(name.len() as u32 * 7919, 4096);
        // MKFF_AC3_FUZZ_ROUNDS raises the round count for longer local runs
        let rounds = std::env::var("MKFF_AC3_FUZZ_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok());
        for round in 0..rounds.unwrap_or(400usize) {
            let mut p = first.clone();
            for k in 0..1 + round % 4 {
                let j = round * 8 + k * 2 + round / 512;
                let r = u32::from_le_bytes([
                    flips[j % 4096],
                    flips[(j + 1) % 4096],
                    flips[(j + 2) % 4096],
                    flips[(j + 3) % 4096],
                ]) as usize
                    ^ round.wrapping_mul(2_654_435_761);
                let bit = 16 + r % ((p.len() - 2) * 8);
                p[bit / 8] ^= 0x80 >> (bit % 8);
            }
            if let Ok(b) = dec.decode(&p) {
                assert!(b.channels.iter().all(|c| c.iter().all(|v| v.is_finite())));
            }
        }
        // with the CRC check on, a flipped bit is rejected
        let mut dec = Ac3Decoder::new();
        let mut p = first.clone();
        p[first.len() / 2] ^= 0x10;
        assert!(dec.decode(&p).is_err());
    }
    // random data, with and without a syncword in front
    let mut dec = Ac3Decoder::new();
    dec.set_crc_check(false);
    for seed in 1..200u32 {
        let mut d = noise(seed, 64 + (seed as usize * 37) % 3000);
        assert!(dec.decode(&d).is_err() || d.iter().all(|&b| b == 0));
        d[0] = 0x0B;
        d[1] = 0x77;
        let _ = dec.decode(&d);
        let _ = find_sync(&d);
        let _ = parse_frame(&d);
        let _ = parse_dac3(&d[..3]);
        let _ = parse_dec3(&d[..8]);
    }
    assert!(dec.decode(&[]).unwrap().is_empty());
}
