//! Decoder tests against ffmpeg reference decodes (see testdata/audio/aac/README.md).

use mkff_aac::{
    adts_header, loas_frame_length, parse_adts, AacDecoder, AudioSpecificConfig, DecoderStats,
    SbrSignalling,
};
use mkff_audio_core::bits::BitWriter;
use mkff_audio_core::{AudioBuffer, Decoder, Error};

fn fixture(name: &str) -> Vec<u8> {
    let p = format!(
        "{}/../../../testdata/audio/aac/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

/// Split an ADTS / LOAS stream into frames.
fn frames(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let len = parse_adts(&data[pos..])
            .map(|h| h.frame_length)
            .or_else(|| loas_frame_length(&data[pos..]))
            .expect("sync");
        out.push(&data[pos..pos + len]);
        pos += len;
    }
    out
}

fn decode_all(dec: &mut AacDecoder, packets: &[&[u8]]) -> AudioBuffer {
    let mut out = AudioBuffer::default();
    for p in packets {
        out.append(&dec.decode(p).expect("decode"));
    }
    out
}

fn decode_file(name: &str) -> (AudioBuffer, DecoderStats) {
    let data = fixture(name);
    let mut dec = AacDecoder::new(&[]).unwrap();
    let out = decode_all(&mut dec, &frames(&data));
    assert_eq!(out.sample_rate, dec.sample_rate());
    (out, *dec.stats())
}

/// SNR (dB) of `ours` against an s16le interleaved reference, per channel.
fn snr_vs_s16(ours: &AudioBuffer, reference: &[u8]) -> Vec<f64> {
    let ch = ours.channels.len();
    let refs: Vec<f64> = reference
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f64::from(i16::from_le_bytes(*b)) / 32768.0)
        .collect();
    let n = refs.len() / ch;
    assert!(
        ours.frames() >= n,
        "decoded {} < reference {n}",
        ours.frames()
    );
    (0..ch)
        .map(|c| {
            let (mut s, mut e) = (0.0, 0.0);
            for i in 0..n {
                let r = refs[i * ch + c];
                let d = f64::from(ours.channels[c][i].clamp(-1.0, 32767.0 / 32768.0)) - r;
                s += r * r;
                e += d * d;
            }
            10.0 * (s / e.max(1e-30)).log10()
        })
        .collect()
}

fn check(name: &str, reference: &str, channels: usize, rate: u32, min_snr: f64) -> DecoderStats {
    let (out, stats) = decode_file(name);
    assert_eq!(out.channels.len(), channels, "{name}");
    assert_eq!(out.sample_rate, rate, "{name}");
    let snr = snr_vs_s16(&out, &fixture(reference));
    eprintln!("{name}: SNR vs s16 reference {snr:.1?} dB, {stats:?}");
    for s in snr {
        assert!(s > min_snr, "{name}: SNR {s:.1} dB");
    }
    stats
}

// The references are 16-bit, which bounds the measurable SNR to about 6 dB × 16 below the
// signal level; float references (see the README) give ~136 dB.

#[test]
fn mono_44k() {
    check("mono_44k.aac", "mono_44k.s16", 1, 44100, 85.0);
}

#[test]
fn mono_8k() {
    check("mono_8k.aac", "mono_8k.s16", 1, 8000, 85.0);
}

#[test]
fn stereo_48k() {
    let s = check("stereo_48k.aac", "stereo_48k.s16", 2, 48000, 85.0);
    assert!(s.ms_bands > 0 && s.intensity_bands > 0);
}

#[test]
fn stereo_22k() {
    check("stereo_22k.aac", "stereo_22k.s16", 2, 22050, 85.0);
}

#[test]
fn transient_short_windows() {
    let s = check("transient_44k.aac", "transient_44k.s16", 2, 44100, 70.0);
    assert!(s.window_sequences[2] > 0, "no short windows: {s:?}");
    assert!(s.window_sequences[1] > 0 && s.window_sequences[3] > 0);
}

#[test]
fn surround_5_1() {
    check("surround51_48k.aac", "surround51_48k.s16", 6, 48000, 85.0);
}

#[test]
fn program_config_element() {
    let s = check("pce51_48k.aac", "pce51_48k.s16", 6, 48000, 85.0);
    assert!(s.pce > 0);
}

#[test]
fn latm_loas() {
    let data = fixture("stereo_48k.latm");
    let mut dec = AacDecoder::new(&[]).unwrap();
    let out = decode_all(&mut dec, &frames(&data));
    assert_eq!(out.channels.len(), 2);
    let snr = snr_vs_s16(&out, &fixture("stereo_48k.s16"));
    assert!(snr.iter().all(|&s| s > 85.0), "{snr:?}");
}

#[test]
fn pns_energy_matches() {
    // PNS noise is random: compare frame energies instead of samples
    let (out, stats) = decode_file("pns_44k.aac");
    assert!(stats.pns_bands > 0);
    let r = fixture("pns_44k.s16");
    let refs: Vec<f32> = r
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
        .collect();
    for f in 1..refs.len() / 2048 {
        let mut e = [0f64; 2];
        for i in f * 1024..(f + 1) * 1024 {
            for c in 0..2 {
                e[0] += f64::from(out.channels[c][i]).powi(2);
                e[1] += f64::from(refs[i * 2 + c]).powi(2);
            }
        }
        let ratio = e[0] / e[1];
        assert!(
            (0.97..1.03).contains(&ratio),
            "frame {f}: energy ratio {ratio}"
        );
    }
}

#[test]
fn raw_access_units_with_asc() {
    // strip ADTS headers and decode with the equivalent AudioSpecificConfig
    let data = fixture("stereo_48k.aac");
    let fr = frames(&data);
    let h = parse_adts(fr[0]).unwrap();
    let asc = h.to_asc().to_bytes();
    let mut dec = AacDecoder::new(&asc).unwrap();
    let raw: Vec<&[u8]> = fr.iter().map(|f| &f[h.header_length..]).collect();
    let out = decode_all(&mut dec, &raw);
    let snr = snr_vs_s16(&out, &fixture("stereo_48k.s16"));
    assert!(snr.iter().all(|&s| s > 85.0), "{snr:?}");
    // re-wrapped with our ADTS writer
    let mut dec = AacDecoder::new(&[]).unwrap();
    let mut out2 = AudioBuffer::default();
    for r in &raw {
        let mut p = adts_header(&h.to_asc(), r.len()).to_vec();
        p.extend_from_slice(r);
        out2.append(&dec.decode(&p).unwrap());
    }
    assert_eq!(out, out2);
}

#[test]
fn explicit_sbr_signalling_upsamples_core() {
    let data = fixture("stereo_22k.aac");
    let fr = frames(&data);
    let h = parse_adts(fr[0]).unwrap();
    let mut asc = h.to_asc();
    asc.sbr_present = true;
    asc.extension_sample_rate = 44100;
    asc.extension_sampling_index = Some(4);
    asc.sbr_signalling = SbrSignalling::Hierarchical;
    let bytes = asc.to_bytes();
    assert_eq!(bytes[0] >> 3, 5, "AOT 5");
    let mut dec = AacDecoder::new(&bytes).unwrap();
    assert_eq!(dec.sample_rate(), 44100);
    assert!(dec.sbr_upsampling());
    let raw: Vec<&[u8]> = fr.iter().map(|f| &f[h.header_length..]).collect();
    let out = decode_all(&mut dec, &raw);
    assert_eq!(out.sample_rate, 44100);
    assert_eq!(out.frames(), 2048 * raw.len());
    // the plain core decode, compared at even output samples (delayed 16 core samples)
    let mut lc = AacDecoder::new(&[]).unwrap();
    let core = decode_all(&mut lc, &fr);
    for c in 0..2 {
        for i in 16..core.frames() {
            assert!((out.channels[c][2 * i] - core.channels[c][i - 16]).abs() < 1e-6);
        }
    }
}

#[test]
fn implicit_sbr_from_fill_element() {
    // a mono AU: SCE with an empty spectrum, FIL with an SBR extension payload, END
    let mut w = BitWriter::new();
    w.write(0, 3); // SCE
    w.write(0, 4);
    w.write(100, 8); // global gain
    w.write(0, 1); // reserved
    w.write(0, 2); // ONLY_LONG
    w.write(0, 1); // sine
    w.write(0, 6); // max_sfb
    w.write(0, 1); // predictor
    w.write(0, 3); // pulse, tns, gain control
    w.write(6, 3); // FIL
    w.write(2, 4); // 2 bytes
    w.write(13, 4); // EXT_SBR_DATA
    w.write(0, 12);
    w.write(7, 3); // END
    let au = w.finish();
    let asc = AudioSpecificConfig::lc(22050, 1);
    let mut dec = AacDecoder::new(&asc.to_bytes()).unwrap();
    assert_eq!(dec.sample_rate(), 22050);
    let out = dec.decode(&au).unwrap();
    assert_eq!(dec.sample_rate(), 44100);
    assert_eq!(out.frames(), 2048);
    assert_eq!(dec.stats().sbr_payloads, 1);
    // a 44.1 kHz core never switches
    let mut dec = AacDecoder::new(&AudioSpecificConfig::lc(44100, 1).to_bytes()).unwrap();
    assert_eq!(dec.decode(&au).unwrap().frames(), 1024);
    assert_eq!(dec.sample_rate(), 44100);
}

#[test]
fn parametric_stereo_signalling_outputs_stereo() {
    let data = fixture("mono_8k.aac");
    let fr = frames(&data);
    let h = parse_adts(fr[0]).unwrap();
    let mut asc = h.to_asc();
    asc.sbr_present = true;
    asc.ps_present = true;
    asc.extension_sample_rate = 16000;
    asc.extension_sampling_index = Some(8);
    let bytes = asc.to_bytes();
    assert_eq!(bytes[0] >> 3, 29, "AOT 29");
    let mut dec = AacDecoder::new(&bytes).unwrap();
    assert_eq!(dec.channels(), 2);
    let out = dec.decode(&fr[2][h.header_length..]).unwrap();
    assert_eq!(out.channels.len(), 2);
    assert_eq!(out.channels[0], out.channels[1]);
}

#[test]
fn unsupported_object_types() {
    for aot in [1u8, 3, 4] {
        let mut asc = AudioSpecificConfig::lc(44100, 2);
        asc.object_type = aot;
        assert!(
            matches!(AacDecoder::new(&asc.to_bytes()), Err(Error::Unsupported(_))),
            "AOT {aot}"
        );
    }
    let mut asc = AudioSpecificConfig::lc(44100, 2);
    asc.frame_length_flag = true;
    assert!(matches!(
        AacDecoder::new(&asc.to_bytes()),
        Err(Error::Unsupported(_))
    ));
    // an ADTS frame with the Main profile
    let data = fixture("mono_44k.aac");
    let mut f = frames(&data)[0].to_vec();
    f[2] &= 0x3F;
    let mut dec = AacDecoder::new(&[]).unwrap();
    assert!(matches!(dec.decode(&f), Err(Error::Unsupported(_))));
}

#[test]
fn malformed_input_never_panics() {
    let mut seed = 0x9E37_79B9u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for name in [
        "stereo_48k.aac",
        "transient_44k.aac",
        "surround51_48k.aac",
        "pns_44k.aac",
        "stereo_48k.latm",
    ] {
        let data = fixture(name);
        let fr = frames(&data);
        // truncated frames
        let mut dec = AacDecoder::new(&[]).unwrap();
        for f in &fr {
            for cut in [1, 3, 7, 9, f.len() / 3, f.len() / 2, f.len() - 1] {
                let _ = dec.decode(&f[..cut.min(f.len())]);
            }
        }
        // corrupted payloads (header kept so the payload parser runs)
        for f in &fr {
            for _ in 0..20 {
                let mut g = f.to_vec();
                let n = 1 + rnd() as usize % 8;
                for _ in 0..n {
                    let i = 7 + rnd() as usize % (g.len() - 7);
                    g[i] ^= 1 << (rnd() % 8);
                }
                let _ = dec.decode(&g);
            }
        }
        // truncated raw AUs
        let h = parse_adts(fr[0]);
        if let Some(h) = h {
            let mut dec = AacDecoder::new(&h.to_asc().to_bytes()).unwrap();
            for f in &fr {
                let raw = &f[h.header_length..];
                for cut in 0..raw.len().min(40) {
                    let _ = dec.decode(&raw[..cut]);
                }
                let _ = dec.decode(raw);
            }
        }
    }
    // pure random packets in every mode
    let configs: Vec<Vec<u8>> = vec![
        vec![],
        AudioSpecificConfig::lc(48000, 2).to_bytes(),
        AudioSpecificConfig::lc(44100, 6).to_bytes(),
        AudioSpecificConfig::lc(8000, 7).to_bytes(),
    ];
    for c in &configs {
        let mut dec = AacDecoder::new(c).unwrap();
        for i in 0..3000 {
            let len = rnd() as usize % 600;
            let mut p: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
            if c.is_empty() && i % 2 == 0 && len >= 7 {
                p[..7].copy_from_slice(&adts_header(&AudioSpecificConfig::lc(44100, 2), len - 7));
            }
            let _ = dec.decode(&p);
        }
    }
    // random configurations
    for _ in 0..5000 {
        let len = rnd() as usize % 12;
        let p: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
        let _ = AudioSpecificConfig::parse(&p);
        let _ = AacDecoder::new(&p);
        let _ = mkff_aac::LatmParser::new().parse_loas_frame(&p);
    }
}

#[test]
fn reset_restarts_cleanly() {
    let data = fixture("mono_44k.aac");
    let fr = frames(&data);
    let mut dec = AacDecoder::new(&[]).unwrap();
    let a = decode_all(&mut dec, &fr);
    dec.reset();
    let b = decode_all(&mut dec, &fr);
    assert_eq!(a, b);
}

#[test]
fn tns_long_windows() {
    let s = check("tns_44k.aac", "tns_44k.s16", 1, 44100, 65.0);
    assert!(s.tns_frames >= 5, "{s:?}");
}

#[test]
fn tns_short_windows() {
    let s = check("tns_short_48k.aac", "tns_short_48k.s16", 2, 48000, 70.0);
    assert!(s.tns_frames > 0 && s.window_sequences[2] > 20, "{s:?}");
}

/// Write an SCE (long window, 44.1 kHz band table) whose first band (4 lines) holds the pair
/// `(0, 0)` then `pair` coded with codebook 11, optionally with one pulse.
fn sce_with_band0(
    w: &mut BitWriter,
    pair_code: (u32, u32),
    sign: Option<bool>,
    pulse: Option<(u32, u32)>,
) {
    w.write(0, 3); // SCE
    w.write(0, 4);
    w.write(160, 8); // global gain = scalefactor of band 0
    w.write(0, 1);
    w.write(0, 2); // ONLY_LONG
    w.write(0, 1);
    w.write(1, 6); // max_sfb
    w.write(0, 1);
    w.write(11, 4); // section: codebook 11, length 1
    w.write(1, 5);
    w.write(0, 1); // scalefactor difference 0 (codeword "0")
    match pulse {
        Some((offset, amp)) => {
            w.write(1, 1);
            w.write(0, 2); // one pulse
            w.write(0, 6); // pulse_start_sfb
            w.write(offset, 5);
            w.write(amp, 4);
        }
        None => w.write(0, 1),
    }
    w.write(0, 2); // tns, gain control
    w.write(0, 4); // pair (0, 0): codebook 11 index 0 = 0000
    w.write(pair_code.0, pair_code.1);
    if let Some(s) = sign {
        w.write(u32::from(s), 1);
    }
}

#[test]
fn pulse_data_equals_direct_coding() {
    let asc = AudioSpecificConfig::lc(44100, 1).to_bytes();
    // A: lines 2,3 = (0, 0) plus a pulse of amplitude 5 at line 2 (0 → -5)
    let mut a = BitWriter::new();
    sce_with_band0(&mut a, (0, 4), None, Some((2, 5)));
    a.write(7, 3);
    // B: lines 2,3 = (5, 0) coded directly with a negative sign: codebook 11 index 85
    let mut b = BitWriter::new();
    sce_with_band0(&mut b, (0xbf, 8), Some(true), None);
    b.write(7, 3);
    let (a, b) = (a.finish(), b.finish());
    let mut da = AacDecoder::new(&asc).unwrap();
    let mut db = AacDecoder::new(&asc).unwrap();
    for _ in 0..2 {
        let oa = da.decode(&a).unwrap();
        let ob = db.decode(&b).unwrap();
        assert_eq!(oa, ob);
        assert!(oa.channels[0].iter().any(|v| v.abs() > 1e-6));
    }
    assert_eq!(da.stats().pulse_frames, 2);
}

#[test]
fn coupling_channel_element_is_parsed_and_skipped() {
    let asc = AudioSpecificConfig::lc(44100, 1).to_bytes();
    let mut with_cce = BitWriter::new();
    with_cce.write(2, 3); // CCE
    with_cce.write(0, 4); // tag
    with_cce.write(0, 1); // ind_sw_cce_flag
    with_cce.write(1, 3); // num_coupled_elements = 1 → two targets
    with_cce.write(1, 1); // target 0 is a CPE, both channels → two gain lists
    with_cce.write(0, 4);
    with_cce.write(1, 1);
    with_cce.write(1, 1);
    with_cce.write(0, 1); // target 1 is an SCE
    with_cce.write(0, 4);
    with_cce.write(0, 4); // cc_domain, gain_element_sign, gain_element_scale
                          // its ICS: one band of codebook 1 with four zero lines
    with_cce.write(100, 8);
    with_cce.write(0, 1);
    with_cce.write(0, 2);
    with_cce.write(0, 1);
    with_cce.write(1, 6);
    with_cce.write(0, 1);
    with_cce.write(1, 4);
    with_cce.write(1, 5);
    with_cce.write(0, 1); // scalefactor
    with_cce.write(0, 3); // pulse, tns, gain control
    with_cce.write(0, 1); // codebook 1, index 40 (0,0,0,0) = "0"
                          // gain element lists 1 (common_gain_element_present = 0: one codeword per coded band) and
                          // 2 (common: one codeword)
    with_cce.write(0, 1);
    with_cce.write(0, 1);
    with_cce.write(1, 1);
    with_cce.write(0, 1);
    let mut plain = BitWriter::new();
    sce_with_band0(&mut plain, (0xbf, 8), Some(false), None);
    plain.write(7, 3);
    sce_with_band0(&mut with_cce, (0xbf, 8), Some(false), None);
    with_cce.write(7, 3);
    let mut d1 = AacDecoder::new(&asc).unwrap();
    let mut d2 = AacDecoder::new(&asc).unwrap();
    let o1 = d1.decode(&with_cce.finish()).unwrap();
    let o2 = d2.decode(&plain.finish()).unwrap();
    assert_eq!(o1, o2);
    assert_eq!(d1.stats().cce, 1);
}

/// Long mutation fuzz over every fixture (`cargo test -p mkff-aac -- --ignored`).
#[test]
#[ignore]
fn fuzz_long() {
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let names = [
        "mono_44k.aac",
        "mono_8k.aac",
        "stereo_48k.aac",
        "stereo_22k.aac",
        "transient_44k.aac",
        "surround51_48k.aac",
        "pce51_48k.aac",
        "pns_44k.aac",
        "tns_44k.aac",
        "tns_short_48k.aac",
        "stereo_48k.latm",
    ];
    for name in names {
        let data = fixture(name);
        let fr = frames(&data);
        let mut dec = AacDecoder::new(&[]).unwrap();
        for round in 0..20_000 {
            let f = fr[round % fr.len()];
            let mut g = f.to_vec();
            match rnd() % 4 {
                0 => {
                    let i = 3 + rnd() as usize % (g.len() - 3);
                    g[i] ^= 1 << (rnd() % 8);
                }
                1 => {
                    for _ in 0..1 + rnd() % 30 {
                        let i = 7.min(g.len() - 1) + rnd() as usize % (g.len() - 7).max(1);
                        if i < g.len() {
                            g[i] = rnd() as u8;
                        }
                    }
                }
                2 => {
                    let cut = rnd() as usize % g.len();
                    g.truncate(cut.max(1));
                }
                _ => {
                    let i = 7 + rnd() as usize % (g.len() - 7).max(1);
                    for b in g.iter_mut().skip(i) {
                        *b = rnd() as u8;
                    }
                }
            }
            if let Ok(out) = dec.decode(&g) {
                assert!(out.channels.iter().all(|c| c.len() % 1024 == 0));
            }
        }
    }
}
