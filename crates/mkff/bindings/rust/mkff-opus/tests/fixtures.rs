//! Decoding tests against libopus reference decodes (see `testdata/audio/opus/README.md`).

use std::path::PathBuf;

use mkff_audio_core::{AudioBuffer, Decoder};
use mkff_opus::{packet::Toc, Mode, OpusDecoder, OpusHead};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/audio/opus")
}

/// A minimal Ogg reader (RFC 3533): the packets of the first logical stream. CRCs are not checked.
fn ogg_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut cur = Vec::new();
    let mut pos = 0;
    let mut serial = None;
    while pos + 27 <= data.len() {
        assert_eq!(&data[pos..pos + 4], b"OggS", "bad Ogg capture at {pos}");
        let s = u32::from_le_bytes(data[pos + 14..pos + 18].try_into().unwrap());
        let nseg = usize::from(data[pos + 26]);
        let lacing = &data[pos + 27..pos + 27 + nseg];
        let mut body = pos + 27 + nseg;
        let mine = *serial.get_or_insert(s) == s;
        for &l in lacing {
            if mine {
                cur.extend_from_slice(&data[body..body + usize::from(l)]);
                if l < 255 {
                    packets.push(std::mem::take(&mut cur));
                }
            }
            body += usize::from(l);
        }
        pos = body;
    }
    packets
}

/// Decode an Ogg Opus file: (header, decoded audio with the pre-skip removed, packets).
fn decode_file(name: &str) -> (OpusHead, AudioBuffer, Vec<Vec<u8>>) {
    let data = std::fs::read(data_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let packets = ogg_packets(&data);
    let head = OpusHead::parse(&packets[0]).expect("OpusHead");
    assert!(packets[1].starts_with(b"OpusTags"));
    let mut dec = OpusDecoder::new(&head).unwrap();
    let mut out = AudioBuffer::empty(48000, usize::from(head.channels));
    for p in &packets[2..] {
        let b = dec.decode(p).expect("decode");
        assert_eq!(b.frames(), mkff_opus::packet_samples(p).unwrap());
        out.append(&b);
    }
    out.skip_front(usize::from(dec.pre_skip()));
    (head, out, packets[2..].to_vec())
}

/// Interleaved little-endian 16-bit reference.
fn read_s16(name: &str) -> Vec<f32> {
    let d = std::fs::read(data_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    d.as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
        .collect()
}

fn snr_db(reference: &[f32], got: &[f32]) -> f64 {
    let mut sig = 0f64;
    let mut err = 0f64;
    for (r, g) in reference.iter().zip(got) {
        sig += f64::from(*r).powi(2);
        // The reference is 16-bit: compare against our output rounded the same way.
        let g16 = (f64::from(*g) * 32768.0).round().clamp(-32768.0, 32767.0) / 32768.0;
        err += (f64::from(*r) - g16).powi(2);
    }
    if err == 0.0 {
        200.0
    } else {
        10.0 * (sig / err).log10()
    }
}

fn check(name: &str, min_snr: f64, expect_modes: &[Mode]) -> f64 {
    let (head, out, packets) = decode_file(&format!("{name}.opus"));
    let reference = read_s16(&format!("{name}.s16"));
    let got = out.interleaved();
    let ch = usize::from(head.channels);
    // libopus (via ffmpeg) also trims the end padding; compare the overlap.
    assert!(
        got.len() >= reference.len(),
        "{name}: decoded {} < reference {}",
        got.len() / ch,
        reference.len() / ch
    );
    let snr = snr_db(&reference, &got[..reference.len()]);
    let modes: Vec<Mode> = packets.iter().map(|p| Toc::parse(p[0]).mode).collect();
    for m in expect_modes {
        assert!(modes.contains(m), "{name}: no {m:?} packet");
    }
    println!(
        "{name}: {} ch, {} samples, SNR {snr:.2} dB",
        ch,
        reference.len() / ch
    );
    assert!(snr >= min_snr, "{name}: SNR {snr:.2} dB below {min_snr}");
    snr
}

#[test]
fn celt_stereo_20ms() {
    check("celt_stereo_20ms", 40.0, &[Mode::CeltOnly]);
}

#[test]
fn celt_stereo_60ms_code3() {
    check("celt_stereo_60ms", 40.0, &[Mode::CeltOnly]);
}

#[test]
fn celt_mono_short_frames() {
    for n in ["celt_mono_2_5ms", "celt_mono_5ms", "celt_mono_10ms"] {
        check(n, 40.0, &[Mode::CeltOnly]);
    }
}

#[test]
fn silk_nb_mono() {
    check("silk_nb_mono", 40.0, &[Mode::SilkOnly]);
}

#[test]
fn silk_mb_60ms() {
    check("silk_mb_60ms", 40.0, &[Mode::SilkOnly]);
}

#[test]
fn silk_wb_stereo_40ms() {
    check("silk_wb_stereo_40ms", 40.0, &[Mode::SilkOnly]);
}

#[test]
fn hybrid_fb_stereo() {
    check("hybrid_fb_stereo", 40.0, &[Mode::Hybrid]);
}

#[test]
fn hybrid_swb_10ms() {
    check("hybrid_swb_10ms", 40.0, &[Mode::Hybrid]);
}

#[test]
fn surround51_family1() {
    let (head, _, _) = decode_file("surround51.opus");
    assert_eq!(
        (
            head.channels,
            head.mapping_family,
            head.stream_count,
            head.coupled_count
        ),
        (6, 1, 4, 2)
    );
    check("surround51", 40.0, &[Mode::CeltOnly]);
}

#[test]
fn hybrid_to_celt_with_redundancy() {
    check("hybrid_to_celt", 40.0, &[Mode::Hybrid, Mode::CeltOnly]);
}

#[test]
fn spliced_mode_switches() {
    // SILK -> CELT -> hybrid -> SILK -> CELT without redundancy: the 2.5 ms around each switch
    // comes from packet loss concealment, which is not bit-exact with libopus.
    check(
        "mode_switch",
        20.0,
        &[Mode::SilkOnly, Mode::CeltOnly, Mode::Hybrid],
    );
}

/// Development helper: `OPUS_FILE=x.opus OPUS_REF=x.f32 cargo test ... -- --ignored dev_compare`
/// prints the packet modes and the SNR against a float reference.
#[test]
#[ignore]
fn dev_compare() {
    let Ok(file) = std::env::var("OPUS_FILE") else {
        return;
    };
    let data = std::fs::read(&file).unwrap();
    let packets = ogg_packets(&data);
    let head = OpusHead::parse(&packets[0]).unwrap();
    let mut dec = OpusDecoder::new(&head).unwrap();
    let mut out = AudioBuffer::empty(48000, usize::from(head.channels));
    let mut hist = std::collections::BTreeMap::new();
    let t = std::time::Instant::now();
    for p in &packets[2..] {
        let toc = Toc::parse(p[0]);
        *hist
            .entry(format!(
                "{:?}/{:?}/{}/{}/c{}",
                toc.mode,
                toc.bandwidth,
                toc.frame_samples,
                u8::from(toc.stereo) + 1,
                toc.code
            ))
            .or_insert(0) += 1;
        out.append(&dec.decode(p).unwrap());
    }
    let el = t.elapsed().as_secs_f64();
    out.skip_front(usize::from(head.pre_skip));
    println!("{head:?}");
    println!("modes {hist:?}");
    println!(
        "{} samples decoded in {:.3} ms ({:.0}x real time)",
        out.frames(),
        el * 1e3,
        out.frames() as f64 / 48000.0 / el
    );
    if let Ok(r) = std::env::var("OPUS_REF") {
        let d = std::fs::read(r).unwrap();
        let reference: Vec<f32> = d
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        let got = out.interleaved();
        let n = got.len().min(reference.len());
        let mut sig = 0f64;
        let mut err = 0f64;
        let ch = usize::from(head.channels);
        let mut worst = (0usize, 0f32);
        for i in 0..n {
            sig += f64::from(reference[i]).powi(2);
            let e = reference[i] - got[i];
            err += f64::from(e).powi(2);
            if e.abs() > worst.1 {
                worst = (i, e.abs());
            }
        }
        if let Ok(at) = std::env::var("OPUS_AT") {
            let at: usize = at.parse().unwrap();
            for i in (at..at + 40).step_by(4) {
                println!(
                    "  {i}: got {:?} want {:?}",
                    &got[i * ch..(i + 4) * ch],
                    &reference[i * ch..(i + 4) * ch]
                );
            }
        }
        let mut blocks = Vec::new();
        for b in 0..n / ch / 960 {
            let r = &reference[b * 960 * ch..(b + 1) * 960 * ch];
            let g = &got[b * 960 * ch..(b + 1) * 960 * ch];
            let s: f64 = r.iter().map(|v| f64::from(*v).powi(2)).sum();
            let e: f64 = r.iter().zip(g).map(|(a, b)| f64::from(a - b).powi(2)).sum();
            blocks.push(format!("{:.0}", 10.0 * (s / e.max(1e-30)).log10()));
        }
        if std::env::var("OPUS_BLOCKS").is_ok() {
            println!("block SNRs {}", blocks.join(" "));
        }
        println!(
            "lengths {} vs {}; SNR {:.2} dB; worst |err| {} at sample {} ch {}",
            got.len() / ch,
            reference.len() / ch,
            10.0 * (sig / err.max(1e-30)).log10(),
            worst.1,
            worst.0 / ch,
            worst.0 % ch
        );
    }
}

#[test]
fn existing_codec_testdata() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/codecs");
    // Ogg Opus written by libopus via ffmpeg.
    let data = std::fs::read(dir.join("tone.opus")).unwrap();
    let packets = ogg_packets(&data);
    let head = OpusHead::parse(&packets[0]).unwrap();
    assert_eq!((head.channels, head.pre_skip), (1, 312));
    let mut dec = OpusDecoder::new(&head).unwrap();
    let mut out = AudioBuffer::empty(48000, 1);
    for p in &packets[2..] {
        out.append(&dec.decode(p).unwrap());
    }
    out.skip_front(312);
    assert!(out.frames() >= 12000);
    let rms = (out.channels[0].iter().map(|v| v * v).sum::<f32>() / out.frames() as f32).sqrt();
    assert!(rms > 0.01 && rms < 1.0, "rms {rms}");
    // WebM: the Matroska CodecPrivate is the OpusHead packet.
    let webm = std::fs::read(dir.join("vp9_opus.webm")).unwrap();
    let pos = webm
        .windows(8)
        .position(|w| w == b"OpusHead")
        .expect("CodecPrivate");
    let head = OpusHead::parse(&webm[pos..pos + 19]).unwrap();
    assert_eq!(
        (head.channels, head.pre_skip, head.mapping_family),
        (1, 312, 0)
    );
    let dec = OpusDecoder::from_config(&webm[pos..pos + 19]).unwrap();
    assert_eq!(
        (dec.channels(), dec.sample_rate(), dec.pre_skip()),
        (1, 48000, 312)
    );
}

/// Development benchmark: `OPUS_FILE=long.opus cargo test --release -- --ignored dev_bench`.
#[test]
#[ignore]
fn dev_bench() {
    let Ok(file) = std::env::var("OPUS_FILE") else {
        return;
    };
    let data = std::fs::read(&file).unwrap();
    let packets = ogg_packets(&data);
    let head = OpusHead::parse(&packets[0]).unwrap();
    let mut dec = OpusDecoder::new(&head).unwrap();
    let mut samples = 0usize;
    let t = std::time::Instant::now();
    for _ in 0..3 {
        dec.reset();
        for p in &packets[2..] {
            samples += dec.decode(p).unwrap().frames();
        }
    }
    let el = t.elapsed().as_secs_f64();
    println!(
        "{samples} samples in {:.1} ms: {:.0}x real time",
        el * 1e3,
        samples as f64 / 48000.0 / el
    );
}
