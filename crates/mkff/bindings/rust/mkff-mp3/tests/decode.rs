//! Decoder tests against FFmpeg's float decode of the fixtures (see
//! testdata/audio/mp3/README.md).

mod common;

use common::*;
use mkff_audio_core::Decoder;
use mkff_mp3::{parse_header, parse_xing, ChannelMode, Mp3Decoder, MpegVersion, XingKind};

/// (fixture, sample rate, channels, total samples per channel FFmpeg decodes, layer, version)
const FIXTURES: &[(&str, u32, usize, usize, u8, MpegVersion)] = &[
    ("l1_48k_js_384.mp1", 48000, 2, 15360, 1, MpegVersion::Mpeg1),
    (
        "l1_32k_mono_192.mp1",
        32000,
        1,
        13824,
        1,
        MpegVersion::Mpeg1,
    ),
    (
        "l2_44k_js_192_crc.mp2",
        44100,
        2,
        23040,
        2,
        MpegVersion::Mpeg1,
    ),
    (
        "l2_48k_stereo_256.mp2",
        48000,
        2,
        14976,
        2,
        MpegVersion::Mpeg1,
    ),
    (
        "l2_48k_stereo_64.mp2",
        48000,
        2,
        24192,
        2,
        MpegVersion::Mpeg1,
    ),
    ("l2_32k_mono_48.mp2", 32000, 1, 16128, 2, MpegVersion::Mpeg1),
    ("l2_22k_mono_64.mp2", 22050, 1, 11520, 2, MpegVersion::Mpeg2),
    ("l3_44k_js_128.mp3", 44100, 2, 27648, 3, MpegVersion::Mpeg1),
    (
        "l3_44k_stereo_320.mp3",
        44100,
        2,
        14976,
        3,
        MpegVersion::Mpeg1,
    ),
    (
        "l3_44k_js_64_ms.mp3",
        44100,
        2,
        24192,
        3,
        MpegVersion::Mpeg1,
    ),
    (
        "l3_44k_js_64_is.mp3",
        44100,
        2,
        24192,
        3,
        MpegVersion::Mpeg1,
    ),
    (
        "l3_48k_stereo_160.mp3",
        48000,
        2,
        20736,
        3,
        MpegVersion::Mpeg1,
    ),
    ("l3_32k_mono_56.mp3", 32000, 1, 17280, 3, MpegVersion::Mpeg1),
    ("l3_24k_mono_32.mp3", 24000, 1, 13248, 3, MpegVersion::Mpeg2),
    ("l3_22k_js_48.mp3", 22050, 2, 12672, 3, MpegVersion::Mpeg2),
    (
        "l3_22k_js_32_ms.mp3",
        22050,
        2,
        12672,
        3,
        MpegVersion::Mpeg2,
    ),
    (
        "l3_16k_stereo_32.mp3",
        16000,
        2,
        9216,
        3,
        MpegVersion::Mpeg2,
    ),
    ("l3_12k_js_24.mp3", 12000, 2, 7488, 3, MpegVersion::Mpeg25),
    ("l3_11k_mono_16.mp3", 11025, 1, 6912, 3, MpegVersion::Mpeg25),
    ("l3_8k_mono_8.mp3", 8000, 1, 5184, 3, MpegVersion::Mpeg25),
];

/// Reference windows: 1536 frames from 0.16 s of FFmpeg's decode.
fn ref_window(name: &str, rate: u32) -> (usize, Vec<f32>) {
    let stem = name.rsplit_once('.').unwrap().0;
    (rate as usize * 16 / 100, reference(&format!("{stem}.ref")))
}

#[test]
fn fixtures_match_reference_decode() {
    for &(name, rate, ch, total, layer, version) in FIXTURES {
        let data = fixture(name);
        let h = parse_header(&data).unwrap();
        assert_eq!(
            (h.layer, h.version, h.sample_rate, h.channels),
            (layer, version, rate, ch),
            "{name}"
        );
        // odd chunk sizes exercise the partial-frame buffering
        let out = decode(&data, 997);
        assert_eq!(out.sample_rate, rate, "{name}");
        assert_eq!(out.channels.len(), ch, "{name}");
        assert_eq!(out.frames(), total, "{name}: decoded length");
        let (start, want) = ref_window(name, rate);
        let got = window(&out, start, 1536);
        let snr = snr_db(&got, &want);
        eprintln!("{name:24} SNR vs FFmpeg {snr:.1} dB");
        assert!(snr > 90.0, "{name}: SNR {snr:.1} dB");
        // one frame per packet decodes identically (reservoir carried across packets)
        let per_frame = decode(&data, 0);
        assert_eq!(
            per_frame.channels, out.channels,
            "{name}: per-frame packets"
        );
    }
}

#[test]
fn intensity_stereo_mpeg1() {
    // the IS fixture differs from its M/S source only in the mode_extension bits
    let is = decode(&fixture("l3_44k_js_64_is.mp3"), 4096);
    let ms = decode(&fixture("l3_44k_js_64_ms.mp3"), 4096);
    let d = snr_db(&is.channels[0], &ms.channels[0]);
    assert!(
        d < 40.0,
        "intensity stereo did not change the output ({d:.1} dB)"
    );
}

#[test]
fn intensity_stereo_lsf() {
    // MPEG-2 LSF intensity stereo (with the illegal-position rule of ISO/IEC 13818-3, which
    // FFmpeg does not apply, so there is no FFmpeg reference): decodes, stays bounded and
    // differs from the M/S decode
    let is = decode(&fixture("l3_22k_js_32_is.mp3"), 4096);
    let ms = decode(&fixture("l3_22k_js_32_ms.mp3"), 4096);
    assert_eq!(is.frames(), 12672);
    assert!(is
        .channels
        .iter()
        .flatten()
        .all(|v| v.is_finite() && v.abs() < 4.0));
    assert!(snr_db(&is.channels[1], &ms.channels[1]) < 40.0);
}

#[test]
fn xing_and_lame_tag() {
    let data = fixture("l3_44k_vbr_xing.mp3");
    let start = mkff_mp3::find_sync(&data).unwrap();
    assert!(start > 0, "the fixture starts with an ID3v2 tag");
    let first = &data[start..start + parse_header(&data[start..]).unwrap().frame_len_bytes];
    let x = parse_xing(first).expect("Xing header");
    assert_eq!(x.kind, XingKind::Xing);
    let frames = split_frames(&data).len() as u32 - 1;
    assert_eq!(x.frames, Some(frames));
    assert_eq!(x.bytes, Some((data.len() - start) as u32));
    assert!(x.toc.is_some());
    // FFmpeg's muxer writes its own version string into the LAME extension
    assert!(x.encoder.as_deref().unwrap_or("").starts_with("Lavc"));
    assert_eq!(x.encoder_delay, Some(576));
    assert_eq!(x.tag_crc_ok, Some(true));
    // FFmpeg trims encoder_delay + 529 samples at the start and padding - 529 at the end
    assert_eq!(x.gapless_samples(1152), Some(17640));
    let out = decode(&data, 1500);
    assert_eq!(
        out.frames(),
        frames as usize * 1152,
        "the tag frame decodes to nothing"
    );
    let (start, want) = ref_window("l3_44k_vbr_xing.mp3", 44100);
    let got = window(&out, start + 1105, 1536);
    let snr = snr_db(&got, &want);
    assert!(snr > 90.0, "SNR {snr:.1}");
}

#[test]
fn crc_check() {
    let data = fixture("l2_44k_js_192_crc.mp2");
    assert!(parse_header(&data).unwrap().protection);
    let mut d = Mp3Decoder::new();
    d.set_crc_check(true);
    d.decode(&data).unwrap();
    assert_eq!(d.frames_concealed(), 0, "all CRCs match");
    // damage a bit allocation field of the third frame
    let mut bad = data.clone();
    let f = split_frames(&data);
    let off = f[0].len() + f[1].len() + 7;
    bad[off] ^= 0x10;
    let mut d = Mp3Decoder::new();
    d.set_crc_check(true);
    let out = d.decode(&bad).unwrap();
    assert_eq!(d.frames_concealed(), 1);
    assert_eq!(
        out.frames(),
        23040,
        "a failed frame still produces its samples"
    );
}

#[test]
fn reservoir_after_reset() {
    let data = fixture("l3_44k_js_128.mp3");
    let frames = split_frames(&data);
    let full = decode(&data, 0);
    // start in the middle: main_data_begin points before the first packet
    let k = 10;
    let mdb = (u32::from(frames[k][4]) << 1) | u32::from(frames[k][5] >> 7);
    assert!(mdb > 0, "fixture frame uses the reservoir");
    let mut d = Mp3Decoder::new();
    d.decode(frames[0]).unwrap();
    d.reset();
    let mut out = mkff_audio_core::AudioBuffer::default();
    for f in &frames[k..] {
        out.append(&d.decode(f).unwrap());
    }
    assert!(d.frames_concealed() >= 1);
    assert_eq!(out.frames(), (frames.len() - k) * 1152);
    assert!(
        out.channels[0][..1152].iter().all(|&v| v == 0.0),
        "unavailable main data decodes to silence"
    );
    // after the reservoir refills and the overlap / filterbank settle the output is exact
    let off = 3 * 1152;
    let got = &out.channels[0][off..];
    let want = &full.channels[0][k * 1152 + off..];
    assert!(snr_db(got, want) > 100.0);
}

#[test]
fn skips_id3_and_junk() {
    let data = fixture("l3_44k_js_128.mp3");
    let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x10".to_vec();
    tagged.extend_from_slice(&[0x55; 16]);
    tagged.extend_from_slice(&[0xFF, 0xFB, 0x12]); // a broken sync
    tagged.extend_from_slice(&data);
    let a = decode(&data, 512);
    let b = decode(&tagged, 512);
    assert_eq!(a.channels, b.channels);
}

#[test]
fn free_format() {
    // turn the 128 kbit/s CBR stream into free format (bit-rate index 0, same frame sizes)
    let data = fixture("l3_44k_js_128.mp3");
    let mut ff = Vec::new();
    for f in split_frames(&data) {
        let mut f = f.to_vec();
        f[2] &= 0x0F;
        ff.extend_from_slice(&f);
    }
    assert_eq!(parse_header(&ff).unwrap().bitrate_kbps, 0);
    assert_eq!(mkff_mp3::find_sync(&ff), Some(0));
    let a = decode(&data, 2048);
    let b = decode(&ff, 2048);
    // the last frame stays buffered: its length is only known from a following header
    assert_eq!(&a.channels[0][..b.frames()], &b.channels[0][..]);
    assert!(b.frames() >= a.frames() - 1152);
}

#[test]
fn header_fields() {
    let data = fixture("l3_44k_js_64_ms.mp3");
    let h = parse_header(&data).unwrap();
    assert_eq!(h.channel_mode, ChannelMode::JointStereo);
    assert_eq!(h.bitrate_kbps, 64);
    assert_eq!(h.samples_per_frame, 1152);
    let data = fixture("l3_8k_mono_8.mp3");
    let h = parse_header(&data).unwrap();
    assert_eq!(
        (h.version, h.sample_rate, h.channels, h.samples_per_frame),
        (MpegVersion::Mpeg25, 8000, 1, 576)
    );
}

#[test]
fn existing_codec_fixture() {
    // testdata/codecs/tone.mp3 (ID3v2 + Info frame + mono 64 kbit/s)
    let data = std::fs::read(format!(
        "{}/../../../testdata/codecs/tone.mp3",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let out = decode(&data, 333);
    assert_eq!(out.sample_rate, 44100);
    assert_eq!(out.channels.len(), 1);
    assert!(out.frames() > 0);
    let peak = out.channels[0].iter().fold(0f32, |m, v| m.max(v.abs()));
    assert!(peak > 0.05 && peak < 1.5);
}
