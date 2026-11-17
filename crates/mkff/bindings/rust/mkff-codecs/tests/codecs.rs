//! Every container / codec in testdata/codecs (see scripts/gen_codec_fixtures.sh), decoded in
//! pure Rust.

use std::path::PathBuf;

use mkff_codecs::ivf::IvfReader;
use mkff_codecs::{
    AudioCodec, AudioDecoder, ChromaFormat, Frame, MediaFile, PixelLayout, VideoCodec, VideoDecoder,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata")
        .join(name)
}

/// The testsrc2 pattern has colour bars and a gradient: a real decode has plenty of luma spread,
/// a failed one (flat grey / black) does not.
fn assert_picture(f: &Frame, w: u32, h: u32) {
    assert_eq!((f.width(), f.height()), (w, h));
    let nv12 = f.to_nv12();
    let luma = &nv12[..(w * h) as usize];
    let (min, max) = luma
        .iter()
        .fold((255u8, 0u8), |(lo, hi), &y| (lo.min(y), hi.max(y)));
    assert!(max - min > 100, "flat picture: luma {min}..{max}");
    let rgba = f.to_rgba8();
    assert_eq!(rgba.len(), (w * h * 4) as usize);
}

/// A 440 Hz sine at -18 dBFS (ffmpeg's `sine` source): RMS ≈ 0.125 / √2.
fn assert_tone(samples: &[f32]) {
    assert!(!samples.is_empty());
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    assert!((0.06..0.12).contains(&rms), "rms {rms}");
}

fn check_movie(
    name: &str,
    codec_hint: &str,
    w: u32,
    h: u32,
    min_frames: u64,
    layout: Option<PixelLayout>,
    audio: Option<u32>,
) {
    let m = MediaFile::open(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let info = m.info();
    let v = info
        .video
        .as_ref()
        .unwrap_or_else(|| panic!("{name}: no video"));
    assert!(
        v.codec.to_ascii_lowercase().contains(codec_hint),
        "{name}: codec {}",
        v.codec
    );
    assert_eq!((v.width, v.height), (w, h), "{name}");
    let n = m.frame_count();
    assert!(n >= min_frames, "{name}: {n} frames");
    for i in 0..n {
        let f = m
            .video_frame(i)
            .unwrap_or_else(|e| panic!("{name} frame {i}: {e}"));
        assert_picture(&f, w, h);
        if let Some(l) = layout {
            assert_eq!(f.layout(), l, "{name}");
        }
    }
    // random access back to the start after reading forward
    assert_picture(&m.video_frame(0).unwrap(), w, h);
    match audio {
        Some(rate) => {
            let a = &info.audio[0];
            assert_eq!(a.sample_rate, rate, "{name}");
            let buf = m
                .audio(0, rate as usize / 10, 0)
                .unwrap_or_else(|e| panic!("{name} audio: {e}"));
            assert_eq!(buf.frames(), rate as usize / 10);
            assert_tone(&buf.channels[0][rate as usize / 50..]);
        }
        None => assert!(info.audio.is_empty() || name.ends_with(".mov"), "{name}"),
    }
}

const YUV420_8: PixelLayout = PixelLayout::Yuv {
    chroma: ChromaFormat::Yuv420,
    bit_depth: 8,
    alpha: false,
};

#[test]
fn vp9_opus_webm() {
    check_movie(
        "codecs/vp9_opus.webm",
        "vp9",
        64,
        48,
        6,
        Some(YUV420_8),
        Some(48000),
    );
}

#[test]
fn vp8_vorbis_webm() {
    check_movie(
        "codecs/vp8_vorbis.webm",
        "vp8",
        64,
        48,
        6,
        Some(YUV420_8),
        Some(48000),
    );
}

#[test]
fn av1_aac_mp4() {
    check_movie(
        "codecs/av1_aac.mp4",
        "av1",
        64,
        48,
        6,
        Some(YUV420_8),
        Some(48000),
    );
}

#[test]
fn hevc_main10_ac3_mkv() {
    check_movie(
        "codecs/hevc10_ac3.mkv",
        "hevc",
        64,
        48,
        6,
        Some(PixelLayout::Yuv {
            chroma: ChromaFormat::Yuv420,
            bit_depth: 10,
            alpha: false,
        }),
        Some(48000),
    );
}

#[test]
fn mpeg2_mp2_ts() {
    check_movie(
        "codecs/mpeg2_mp2.ts",
        "mpeg",
        64,
        48,
        5,
        Some(YUV420_8),
        Some(48000),
    );
}

#[test]
fn h264_aac_ts() {
    check_movie(
        "codecs/h264_aac.ts",
        "h.264",
        64,
        48,
        5,
        Some(YUV420_8),
        Some(48000),
    );
}

#[test]
fn prores_hq_mov() {
    check_movie(
        "codecs/prores_hq.mov",
        "prores",
        64,
        48,
        6,
        Some(PixelLayout::Yuv {
            chroma: ChromaFormat::Yuv422,
            bit_depth: 10,
            alpha: false,
        }),
        Some(48000),
    );
}

#[test]
fn prores_4444_with_alpha() {
    let m = MediaFile::open(fixture("codecs/prores_4444_alpha.mov")).unwrap();
    let f = m.video_frame(0).unwrap();
    assert_picture(&f, 64, 48);
    assert!(
        matches!(
            f.layout(),
            PixelLayout::Yuv {
                chroma: ChromaFormat::Yuv444,
                alpha: true,
                ..
            }
        ),
        "{:?}",
        f.layout()
    );
}

#[test]
fn dnxhr_mxf() {
    check_movie(
        "codecs/dnxhr.mxf",
        "dn",
        256,
        128,
        3,
        Some(PixelLayout::Yuv {
            chroma: ChromaFormat::Yuv422,
            bit_depth: 8,
            alpha: false,
        }),
        Some(48000),
    );
}

#[test]
fn mjpeg_pcm_avi() {
    check_movie(
        "codecs/mjpeg_pcm.avi",
        "jpeg",
        64,
        48,
        6,
        Some(PixelLayout::Rgba8),
        Some(48000),
    );
}

#[test]
fn existing_mkff_mp4_fixture() {
    check_movie(
        "tiny_main_256x144.mp4",
        "hevc",
        256,
        144,
        1,
        Some(YUV420_8),
        None,
    );
}

#[test]
fn audio_only_files() {
    for (name, rate) in [
        ("codecs/tone.flac", 44100),
        ("codecs/tone.mp3", 44100),
        ("codecs/tone.opus", 48000),
        ("codecs/tone_eac3.mka", 48000),
        ("codecs/tone_alac.m4a", 44100),
    ] {
        let m = MediaFile::open(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(m.info().video.is_none(), "{name}");
        // MKFF's own audio stack, except Matroska which FilmCraft still demuxes
        assert_eq!(m.audio_is_native(0), !name.ends_with(".mka"), "{name}");
        assert_eq!(m.source().is_none(), !name.ends_with(".mka"), "{name}: FilmCraft opened an audio-only file");
        assert_eq!(m.info().audio[0].sample_rate, rate, "{name}");
        let buf = m
            .audio(0, rate as usize / 10, 0)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_tone(&buf.channels[0][rate as usize / 50..]);
        // resampled read
        let r = m.audio(0, 4800, 48000).unwrap();
        assert_eq!((r.sample_rate, r.frames()), (48000, 4800), "{name}");
    }
}

/// Power of `freq` in `x` (Goertzel).
fn tone_power(x: &[f32], freq: f64, rate: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = f64::from(v) + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2) / (x.len() as f64).powi(2)
}

/// 5.1 AC-3 / E-AC-3 in MP4 with the spec's ChannelCount of 2 in the sample entry: all six
/// channels come out, each with its own tone, in WAV order — and at another rate, and after a
/// seek.
#[test]
fn surround_ac3_in_mp4_keeps_every_channel() {
    // FL FR FC LFE SL SR
    let tones = [300.0, 500.0, 700.0, 60.0, 900.0, 1100.0];
    for name in ["codecs/tone51_ac3.mp4", "codecs/tone51_eac3.mp4"] {
        let m = MediaFile::open(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(m.info().audio[0].channels, 6, "{name}");
        assert!(m.audio_is_native(0), "{name}");
        for (start, rate) in [(0i64, 48_000u32), (2400, 48_000), (0, 44_100)] {
            let buf = m.audio(start, 6000, rate).unwrap();
            assert_eq!(buf.channel_count(), 6, "{name}");
            for (c, chan) in buf.channels.iter().enumerate() {
                // skip the encoder's start-up
                let x = &chan[2000..];
                let own = tone_power(x, tones[c], f64::from(rate));
                for (k, &f) in tones.iter().enumerate() {
                    if k != c {
                        let other = tone_power(x, f, f64::from(rate));
                        assert!(
                            own > other * 50.0,
                            "{name} @{start}/{rate}: channel {c} has {f} Hz"
                        );
                    }
                }
            }
        }
        // random access reads the same samples as a sequential one
        let a = m.audio(3000, 2000, 48_000).unwrap();
        let _ = m.audio(9000, 100, 48_000).unwrap();
        assert_eq!(a, m.audio(3000, 2000, 48_000).unwrap(), "{name}");
    }
}

fn decode_ivf(name: &str, codec: VideoCodec) {
    let data = std::fs::read(fixture(name)).unwrap();
    let ivf = IvfReader::new(&data).unwrap();
    assert_eq!(ivf.header().codec(), Some(codec));
    let mut dec = VideoDecoder::new(codec).unwrap();
    let mut frames = Vec::new();
    for pkt in ivf {
        frames.extend(
            dec.decode(pkt.data, pkt.pts)
                .unwrap_or_else(|e| panic!("{name} pts {}: {e}", pkt.pts)),
        );
    }
    frames.extend(dec.flush());
    assert_eq!(frames.len(), 6, "{name}");
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.pts(), i as i64);
        assert_picture(f, 64, 48);
    }
}

#[test]
fn ivf_elementary_streams() {
    decode_ivf("codecs/vp8.ivf", VideoCodec::Vp8);
    decode_ivf("codecs/vp9.ivf", VideoCodec::Vp9);
    decode_ivf("codecs/av1.ivf", VideoCodec::Av1);
}

/// Split an Annex-B byte stream into access units at each access-unit delimiter / first slice.
fn h264_access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            let nal = stream[i + 3] & 0x1F;
            let first_slice =
                matches!(nal, 1 | 5) && stream.get(i + 4).is_some_and(|b| b & 0x80 != 0);
            let begin = if i > 0 && stream[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            if nal == 9
                || first_slice
                || (matches!(nal, 7 | 6) && starts.last().is_none_or(|&s: &usize| s < begin))
            {
                starts.push(begin);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    // keep only boundaries that begin a new AU: a parameter set / SEI starts one only if a slice
    // came after the previous start
    let mut aus = Vec::new();
    let mut prev = 0;
    let mut seen_slice = false;
    let mut j = 0;
    for &s in &starts {
        while j < s {
            if stream[j..].starts_with(&[0, 0, 1]) && matches!(stream[j + 3] & 0x1F, 1 | 5) {
                seen_slice = true;
            }
            j += 1;
        }
        if seen_slice && s > prev {
            aus.push(&stream[prev..s]);
            prev = s;
            seen_slice = false;
        }
    }
    aus.push(&stream[prev..]);
    aus
}

#[test]
fn h264_high_annexb_with_b_frames() {
    let data = std::fs::read(fixture("codecs/h264_high.h264")).unwrap();
    let mut dec = VideoDecoder::new(VideoCodec::H264).unwrap();
    let mut frames = Vec::new();
    for (i, au) in h264_access_units(&data).into_iter().enumerate() {
        frames.extend(dec.decode(au, i as i64).unwrap());
    }
    frames.extend(dec.flush());
    assert_eq!(frames.len(), 6);
    frames.iter().for_each(|f| assert_picture(f, 64, 48));
}

#[test]
fn existing_annexb_fixtures_decode_in_software() {
    // HEVC Main (I and I+P) and Main 10 (libhevc, libmkff's software fallback, is 8-bit only).
    // The whole Annex-B stream goes in as one packet: the decoder splits pictures itself.
    for (name, codec, w, h, depth) in [
        ("tiny_main_256x144.hevc", VideoCodec::Hevc, 256, 144, 8),
        ("tiny_main_p_256x144.hevc", VideoCodec::Hevc, 256, 144, 8),
        ("tiny_main10_64x64.hevc", VideoCodec::Hevc, 64, 64, 10),
    ] {
        let data = std::fs::read(fixture(name)).unwrap();
        let mut dec = VideoDecoder::new(codec).unwrap();
        let mut frames = dec
            .decode(&data, 0)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        frames.extend(dec.flush());
        assert!(!frames.is_empty(), "{name}");
        for f in &frames {
            assert_picture(f, w, h);
            assert!(
                matches!(f.layout(), PixelLayout::Yuv { bit_depth, .. } if bit_depth == depth),
                "{name}: {:?}",
                f.layout()
            );
            assert_eq!(f.to_nv12().len(), (w * h * 3 / 2) as usize);
            assert_eq!(f.to_p010().len(), (w * h * 3 / 2) as usize);
        }
    }
}

#[test]
fn adts_aac_elementary_stream() {
    let data = std::fs::read(fixture("codecs/tone.aac")).unwrap();
    let mut dec = AudioDecoder::new(AudioCodec::Aac, 0, &[]).unwrap();
    let mut pcm = Vec::new();
    let mut p = 0;
    while p + 7 <= data.len() {
        let len = (((data[p + 3] & 3) as usize) << 11)
            | (data[p + 4] as usize) << 3
            | (data[p + 5] as usize) >> 5;
        assert!(len >= 7 && p + len <= data.len());
        let b = dec.decode(&data[p..p + len], 0).unwrap();
        assert_eq!(b.sample_rate, 48000);
        pcm.extend_from_slice(&b.channels[0]);
        p += len;
    }
    assert!(pcm.len() >= 9000, "{}", pcm.len());
    assert_tone(&pcm[2048..]);
}

#[test]
fn codec_names_round_trip() {
    for c in VideoCodec::ALL {
        assert_eq!(VideoCodec::from_name(c.name()), Some(c));
        VideoDecoder::new(c).unwrap();
    }
    for c in AudioCodec::ALL {
        assert_eq!(AudioCodec::from_name(c.name()), Some(c));
    }
}

/// NAL units of an Annex-B buffer (start codes stripped).
fn nals(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(b.len(), |&n| n - 3);
            while e > s && b[e - 1] == 0 {
                e -= 1;
            }
            &b[s..e]
        })
        .collect()
}

#[test]
fn h264_avcc_and_out_of_band_parameter_sets() {
    let data = std::fs::read(fixture("codecs/h264_high.h264")).unwrap();
    let aus = h264_access_units(&data);
    let all = nals(&data);
    let sps = *all.iter().find(|n| n[0] & 0x1F == 7).unwrap();
    let pps = *all.iter().find(|n| n[0] & 0x1F == 8).unwrap();
    let slices = |au: &[u8]| -> Vec<Vec<u8>> {
        nals(au)
            .into_iter()
            .filter(|n| !matches!(n[0] & 0x1F, 7..=9))
            .map(<[u8]>::to_vec)
            .collect()
    };

    let reference: Vec<Vec<u8>> = {
        let mut dec = VideoDecoder::new(VideoCodec::H264).unwrap();
        let mut f: Vec<Frame> = aus
            .iter()
            .enumerate()
            .flat_map(|(i, au)| dec.decode(au, i as i64).unwrap())
            .collect();
        f.extend(dec.flush());
        f.iter().map(Frame::to_nv12).collect()
    };
    assert_eq!(reference.len(), 6);

    // MP4 / Matroska style: avcC record + 4-byte length-prefixed NAL units
    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);
    let mut dec = VideoDecoder::with_config(VideoCodec::H264, &avcc).unwrap();
    let mut out = Vec::new();
    for (i, au) in aus.iter().enumerate() {
        let sample: Vec<u8> = slices(au)
            .iter()
            .flat_map(|n| {
                (n.len() as u32)
                    .to_be_bytes()
                    .into_iter()
                    .chain(n.iter().copied())
            })
            .collect();
        out.extend(dec.decode(&sample, i as i64).unwrap());
    }
    out.extend(dec.flush());
    assert_eq!(
        out.iter().map(Frame::to_nv12).collect::<Vec<_>>(),
        reference,
        "avcC"
    );

    // Annex-B parameter sets given once as configuration, packets without them (also after reset)
    let mut cfg = vec![0, 0, 0, 1];
    cfg.extend_from_slice(sps);
    cfg.extend_from_slice(&[0, 0, 0, 1]);
    cfg.extend_from_slice(pps);
    let mut dec = VideoDecoder::with_config(VideoCodec::H264, &cfg).unwrap();
    for round in 0..2 {
        let mut out = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            let sample: Vec<u8> = slices(au)
                .iter()
                .flat_map(|n| [0, 0, 0, 1].into_iter().chain(n.iter().copied()))
                .collect();
            out.extend(dec.decode(&sample, i as i64).unwrap());
        }
        out.extend(dec.flush());
        assert_eq!(
            out.iter().map(Frame::to_nv12).collect::<Vec<_>>(),
            reference,
            "Annex-B config, round {round}"
        );
        dec.reset();
    }
}
