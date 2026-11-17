//! Decode the checked-in fixtures and compare with reference decodes (see
//! `testdata/audio/vorbis/README.md`).

mod common;

use common::*;
use mkff_audio_core::{AudioBuffer, Decoder};
use mkff_vorbis::{split_config, Comments, IdentHeader, VorbisDecoder};

/// Reference files hold every 11th frame of the reference decode.
const REF_STRIDE: usize = 11;

struct Fixture {
    name: &'static str,
    channels: u8,
    rate: u32,
    blocksizes: (usize, usize),
    /// Minimum SNR against the reference, in dB.
    min_snr: f64,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "mono_44k_q3",
        channels: 1,
        rate: 44100,
        blocksizes: (256, 2048),
        min_snr: 110.0,
    },
    Fixture {
        name: "mono_8k_qm1",
        channels: 1,
        rate: 8000,
        blocksizes: (512, 512),
        min_snr: 110.0,
    },
    Fixture {
        name: "stereo_22k_q0",
        channels: 2,
        rate: 22050,
        blocksizes: (1024, 1024),
        min_snr: 110.0,
    },
    Fixture {
        name: "stereo_48k_q8",
        channels: 2,
        rate: 48000,
        blocksizes: (256, 2048),
        min_snr: 110.0,
    },
    Fixture {
        name: "surround51_48k_q2",
        channels: 6,
        rate: 48000,
        blocksizes: (256, 2048),
        min_snr: 110.0,
    },
    // reference from libvorbis, whose ffmpeg wrapper outputs 16-bit samples: ~81 dB is the ceiling
    Fixture {
        name: "stereo_44k_ffenc",
        channels: 2,
        rate: 44100,
        blocksizes: (2048, 2048),
        min_snr: 78.0,
    },
];

/// Decode every packet; returns the output and the decoder.
fn decode_all(dec: &mut VorbisDecoder, packets: &[Vec<u8>]) -> AudioBuffer {
    let mut out = AudioBuffer::default();
    for p in packets {
        let b = dec.decode(p).unwrap();
        assert_eq!(b.channel_count(), dec.channels());
        out.append(&b);
    }
    out
}

/// SNR of `got` against a strided reference, overall and per channel.
fn compare(name: &str, got: &AudioBuffer) -> (f64, Vec<f64>) {
    let reference = f32_file(&fixture(&format!("{name}.ref.f32")));
    let ch = got.channel_count();
    let ref_frames = reference.len() / ch;
    let n = ref_frames.min(got.frames().div_ceil(REF_STRIDE));
    assert!(
        n * REF_STRIDE + REF_STRIDE > got.frames().min(ref_frames * REF_STRIDE),
        "{name}: length mismatch"
    );
    let mut want_all = Vec::new();
    let mut got_all = Vec::new();
    let mut per = Vec::new();
    for c in 0..ch {
        let w: Vec<f32> = (0..n).map(|i| reference[i * ch + c]).collect();
        let g: Vec<f32> = (0..n).map(|i| got.channels[c][i * REF_STRIDE]).collect();
        per.push(snr_db(&w, &g));
        want_all.extend(w);
        got_all.extend(g);
    }
    (snr_db(&want_all, &got_all), per)
}

#[test]
fn fixtures_match_reference() {
    for f in FIXTURES {
        let ogg = read_ogg(&fixture(&format!("{}.ogg", f.name)));
        let mut dec = VorbisDecoder::new(&[]).unwrap();
        assert!(!dec.is_ready());
        let mut out = decode_all(&mut dec, &ogg.packets);
        assert!(dec.is_ready());

        let id = dec.ident().unwrap();
        assert_eq!(id.channels, f.channels, "{}", f.name);
        assert_eq!(id.sample_rate, f.rate, "{}", f.name);
        assert_eq!((id.blocksize_0, id.blocksize_1), f.blocksizes, "{}", f.name);
        assert_eq!(dec.sample_rate(), f.rate);
        let comments = dec.comments().unwrap();
        assert!(!comments.vendor.is_empty());
        assert!(
            comments.get("encoder").unwrap().starts_with("Lavc"),
            "{}",
            f.name
        );

        // the decoded length reaches the final granule position, by less than one long block
        let granule = ogg.last_granule as usize;
        assert!(
            out.frames() >= granule && out.frames() < granule + f.blocksizes.1,
            "{}",
            f.name
        );
        out.truncate(granule);

        let (snr, per) = compare(f.name, &out);
        println!(
            "{}: {} frames, SNR {snr:.1} dB, per channel {per:.1?}",
            f.name,
            out.frames()
        );
        assert!(snr > f.min_snr, "{}: SNR {snr:.1} dB", f.name);
        for (c, s) in per.iter().enumerate() {
            assert!(
                *s > f.min_snr - 3.0,
                "{} channel {c}: SNR {s:.1} dB",
                f.name
            );
        }
        assert!(out
            .channels
            .iter()
            .flatten()
            .all(|s| s.is_finite() && s.abs() < 2.0));
    }
}

/// Granule bookkeeping through `packet_block_size` / `packet_samples` agrees with the decoder's
/// output and the Ogg granule positions; libvorbis streams mix both block sizes.
#[test]
fn packet_timing_helpers() {
    for f in FIXTURES {
        let ogg = read_ogg(&fixture(&format!("{}.ogg", f.name)));
        let mut dec = VorbisDecoder::new(&[]).unwrap();
        let mut prev: Option<&[u8]> = None;
        let mut total = 0usize;
        let mut sizes = std::collections::BTreeSet::new();
        for (i, p) in ogg.packets.iter().enumerate() {
            let out = dec.decode(p).unwrap();
            if i < 3 {
                assert_eq!(
                    dec.packet_block_size(p),
                    None,
                    "header packets have no block size"
                );
                continue;
            }
            let bs = dec.packet_block_size(p).unwrap();
            sizes.insert(bs);
            match prev {
                None => assert_eq!(out.frames(), 0),
                Some(pp) => {
                    let n = dec.packet_samples(pp, p).unwrap();
                    assert_eq!(out.frames(), n, "{} packet {i}", f.name);
                    total += n;
                }
            }
            let g = ogg.granules[i];
            if g >= 0 && i + 1 < ogg.packets.len() {
                assert_eq!(total as i64, g, "{} granule at packet {i}", f.name);
            }
            prev = Some(p);
        }
        if f.blocksizes.0 != f.blocksizes.1 {
            assert_eq!(sizes.len(), 2, "{}: expected short and long blocks", f.name);
        }
    }
}

/// Every configuration layout gives the same decoder, and `from_headers` too.
#[test]
fn config_layouts_decode_identically() {
    let ogg = read_ogg(&fixture("stereo_22k_q0.ogg"));
    let [id, com, setup] = [&ogg.packets[0], &ogg.packets[1], &ogg.packets[2]];
    let audio = &ogg.packets[3..];
    let reference = decode_all(&mut VorbisDecoder::new(&[]).unwrap(), &ogg.packets);

    let lace = |n: usize| {
        let mut v = vec![255u8; n / 255];
        v.push((n % 255) as u8);
        v
    };
    let mut xiph = vec![2u8];
    xiph.extend(lace(id.len()));
    xiph.extend(lace(com.len()));
    xiph.extend_from_slice(id);
    xiph.extend_from_slice(com);
    xiph.extend_from_slice(setup);
    let mut be16 = Vec::new();
    for p in [id, com, setup] {
        be16.extend_from_slice(&(p.len() as u16).to_be_bytes());
        be16.extend_from_slice(p);
    }
    let plain = [id.as_slice(), com, setup].concat();

    for cfg in [&xiph, &be16, &plain] {
        let parts = split_config(cfg).unwrap().unwrap();
        assert_eq!(parts, [id.as_slice(), com.as_slice(), setup.as_slice()]);
        let mut dec = VorbisDecoder::new(cfg).unwrap();
        assert!(dec.is_ready());
        assert_eq!(dec.comments(), Some(&Comments::parse(com).unwrap()));
        // headers repeated in-band are ignored
        assert!(dec.decode(id).unwrap().is_empty());
        assert_eq!(decode_all(&mut dec, audio), reference);
    }
    let mut dec = VorbisDecoder::from_headers(id, setup).unwrap();
    assert!(dec.comments().is_none());
    assert_eq!(dec.ident(), Some(&IdentHeader::parse(id).unwrap()));
    assert_eq!(decode_all(&mut dec, audio), reference);
}

/// After `reset`, the next packet only primes the overlap; output from the one after matches an
/// uninterrupted decode.
#[test]
fn reset_restarts_overlap() {
    let ogg = read_ogg(&fixture("mono_44k_q3.ogg"));
    let mut dec = VorbisDecoder::new(&[]).unwrap();
    let outs: Vec<AudioBuffer> = ogg.packets.iter().map(|p| dec.decode(p).unwrap()).collect();
    let k = 20;
    dec.reset();
    assert!(dec.decode(&ogg.packets[k]).unwrap().is_empty());
    for (i, p) in ogg.packets.iter().enumerate().skip(k + 1).take(10) {
        assert_eq!(dec.decode(p).unwrap(), outs[i], "packet {i}");
    }
    // empty packets are tolerated and change nothing
    assert!(dec.decode(&[]).unwrap().is_empty());
}

/// The WebM fixture: Xiph-laced `CodecPrivate`, one Vorbis packet per block, 440 Hz tone.
#[test]
fn webm_codec_private() {
    let data = std::fs::read(fixture_dir().join("../../codecs/vp8_vorbis.webm")).unwrap();
    let (private, frames) = read_mkv_audio(&data, "A_VORBIS");
    assert_eq!(private[0], 2);
    let mut dec = VorbisDecoder::new(&private).unwrap();
    assert_eq!((dec.channels(), dec.sample_rate()), (1, 48000));
    let out = decode_all(&mut dec, &frames);
    assert!(out.frames() > 10_000);
    let (snr, _) = compare("vp8_vorbis_webm", &out);
    println!("vp8_vorbis.webm: {} frames, SNR {snr:.1} dB", out.frames());
    assert!(snr > 110.0, "SNR {snr:.1} dB");
    // zero crossings of the steady part give the tone frequency
    let s = &out.channels[0][4096..out.frames() - 2048];
    let crossings = s.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    let hz = crossings as f64 * 48000.0 / s.len() as f64;
    assert!((hz - 440.0).abs() < 10.0, "{hz} Hz");
}

/// Release-mode speed check: prints the real-time factor and asserts a floor well below what a
/// desktop machine reaches (several hundred times real time for 5.1), to stay robust on slow CI.
#[test]
fn decode_speed() {
    let mut report = Vec::new();
    for name in ["stereo_48k_q8", "surround51_48k_q2", "mono_44k_q3"] {
        let ogg = read_ogg(&fixture(&format!("{name}.ogg")));
        let mut dec = VorbisDecoder::new(&[]).unwrap();
        for p in &ogg.packets[..3] {
            dec.decode(p).unwrap();
        }
        let audio = &ogg.packets[3..];
        let rounds = 20;
        let mut frames = 0usize;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            dec.reset();
            for p in audio {
                frames += dec.decode(p).unwrap().frames();
            }
        }
        let secs = t.elapsed().as_secs_f64();
        let factor = frames as f64 / f64::from(dec.sample_rate()) / secs;
        report.push(format!("{name}: {factor:.0}x real time"));
        if !cfg!(debug_assertions) {
            assert!(factor > 50.0, "{name}: only {factor:.0}x real time");
        }
    }
    println!("{}", report.join("; "));
}
