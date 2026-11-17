//! End-to-end tests for RAUF v1 and headerless RAW PCM through `mkff-audio`.

use std::io::Cursor;

use mkff_audio::format::{
    encode_pcm, PcmCoding, PcmFormat, RAUF_FLAG_FINALIZED, RAUF_FLAG_RECOVERED,
};
use mkff_audio::{
    read_rauf_info, supported_extensions, AudioBuffer, AudioFile, CodecId, Error, RaufInfo,
    RawPcmSpec, RAW_PCM_EXTENSIONS,
};

#[allow(clippy::too_many_arguments)]
fn build_rauf_header(
    version: u16,
    header_size: u16,
    sample_rate: u32,
    channels: u16,
    sample_format: u16,
    endianness: u16,
    interleaved: u16,
    flags: u16,
    frames_written: u64,
    project_start_sample: u64,
    take_id: [u8; 16],
    data_offset: u64,
) -> [u8; 256] {
    let mut h = [0u8; 256];
    h[0..4].copy_from_slice(b"RAUF");
    h[4..6].copy_from_slice(&version.to_le_bytes());
    h[6..8].copy_from_slice(&header_size.to_le_bytes());
    h[8..12].copy_from_slice(&sample_rate.to_le_bytes());
    h[12..14].copy_from_slice(&channels.to_le_bytes());
    h[14..16].copy_from_slice(&sample_format.to_le_bytes());
    h[16..18].copy_from_slice(&endianness.to_le_bytes());
    h[18..20].copy_from_slice(&interleaved.to_le_bytes());
    h[20..22].copy_from_slice(&flags.to_le_bytes());
    h[24..32].copy_from_slice(&frames_written.to_le_bytes());
    h[32..40].copy_from_slice(&project_start_sample.to_le_bytes());
    h[40..56].copy_from_slice(&take_id);
    h[56..64].copy_from_slice(&data_offset.to_le_bytes());
    h
}

#[test]
fn rauf_audio_file_s32_and_f32_decode_seek_and_recover() {
    assert!(supported_extensions().contains(&"rauf"));
    assert!(!supported_extensions().contains(&"raw"));
    assert!(!supported_extensions().contains(&"pcm"));
    assert_eq!(RAW_PCM_EXTENSIONS, &["raw", "pcm"]);

    // 1. Finalized s32le: verify exact x / 2^31 full-scale f32 values and sample-exact seek
    let s32_frames: [(i32, i32); 6] = [
        (i32::MIN, 0),
        (-(1 << 30), 1 << 30),
        (0, i32::MIN),
        (1 << 29, -(1 << 29)),
        (1 << 30, -(1 << 30)),
        (i32::MAX, -1),
    ];
    let take_id = [0x5A; 16];
    let mut s32_bytes = build_rauf_header(
        1,
        256,
        48_000,
        2,
        3,
        1,
        1,
        RAUF_FLAG_FINALIZED,
        s32_frames.len() as u64,
        144_000,
        take_id,
        256,
    )
    .to_vec();
    for (l, r) in s32_frames {
        s32_bytes.extend_from_slice(&l.to_le_bytes());
        s32_bytes.extend_from_slice(&r.to_le_bytes());
    }

    let info = read_rauf_info(&mut Cursor::new(s32_bytes.clone())).unwrap();
    assert_eq!(
        info,
        RaufInfo {
            project_start_sample: 144_000,
            take_id,
            flags: RAUF_FLAG_FINALIZED,
            finalized: true,
            recovered: false,
        }
    );

    let mut af = AudioFile::from_bytes(s32_bytes, "take.rauf").unwrap();
    assert_eq!(af.format_name(), "RAUF");
    assert_eq!(af.codec(), CodecId::Pcm);
    assert_eq!(af.sample_rate(), 48_000);
    assert_eq!(af.channels(), 2);
    assert_eq!(af.duration(), Some(s32_frames.len() as u64));
    let decoded = af.read_to_end().unwrap();
    assert_eq!(decoded.frames(), s32_frames.len());
    for (i, &(l, r)) in s32_frames.iter().enumerate() {
        let want_l = (f64::from(l) / 2_147_483_648.0) as f32;
        let want_r = (f64::from(r) / 2_147_483_648.0) as f32;
        assert_eq!(decoded.channels[0][i], want_l, "frame {i} left");
        assert_eq!(decoded.channels[1][i], want_r, "frame {i} right");
    }

    // Seek to an exact frame beyond the first packet in a multi-packet RAUF
    let big_frames = 6_000usize;
    let mut big_bytes = build_rauf_header(
        1,
        256,
        48_000,
        1,
        4,
        1,
        1,
        RAUF_FLAG_FINALIZED,
        big_frames as u64,
        0,
        [7; 16],
        256,
    )
    .to_vec();
    let want_f32: Vec<f32> = (0..big_frames)
        .map(|i| ((i as f32) * 0.001 - 3.0).sin())
        .collect();
    for &s in &want_f32 {
        big_bytes.extend_from_slice(&s.to_le_bytes());
    }
    let mut af_f32 = AudioFile::from_bytes(big_bytes, "float.rauf").unwrap();
    assert_eq!(af_f32.track().name, "PCM f32le");
    assert_eq!(af_f32.duration(), Some(big_frames as u64));
    for target in [0usize, 1, 4095, 4096, 4097, 5555] {
        af_f32.seek(target as u64).unwrap();
        assert_eq!(af_f32.position(), target as u64);
        let rest = af_f32.read_to_end().unwrap();
        assert_eq!(rest.channels[0], &want_f32[target..]);
    }

    // 2. Unfinalized file (frames_written = 0) with trailing partial frame
    let mut unfin_bytes =
        build_rauf_header(1, 256, 44_100, 2, 4, 1, 1, 0, 0, 10_000, [9; 16], 256).to_vec();
    let unfin_samples: [f32; 8] = [0.1, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4];
    for s in unfin_samples {
        unfin_bytes.extend_from_slice(&s.to_le_bytes());
    }
    unfin_bytes.extend_from_slice(&[1, 2, 3, 4, 5]); // 5 bytes < 8-byte stereo frame
    let unfin_info = read_rauf_info(&mut Cursor::new(unfin_bytes.clone())).unwrap();
    assert!(!unfin_info.finalized);
    assert!(unfin_info.recovered);
    let mut af_unfin = AudioFile::from_bytes(unfin_bytes, "crashed.rauf").unwrap();
    assert_eq!(af_unfin.duration(), Some(4));
    let dec_unfin = af_unfin.read_to_end().unwrap();
    assert_eq!(dec_unfin.channels[0], vec![0.1, 0.2, 0.3, 0.4]);
    assert_eq!(dec_unfin.channels[1], vec![-0.1, -0.2, -0.3, -0.4]);

    // 3. Invalid RAUF files return errors
    let bad_ver = build_rauf_header(2, 256, 48_000, 2, 3, 1, 1, 1, 0, 0, [0; 16], 256).to_vec();
    assert!(AudioFile::from_bytes(bad_ver, "bad.rauf").is_err());
    let bad_end = build_rauf_header(1, 256, 48_000, 2, 3, 2, 1, 1, 0, 0, [0; 16], 256).to_vec();
    assert!(AudioFile::from_bytes(bad_end, "bad.rauf").is_err());
    let bad_ch = build_rauf_header(1, 256, 48_000, 0, 3, 1, 1, 1, 0, 0, [0; 16], 256).to_vec();
    assert!(AudioFile::from_bytes(bad_ch, "bad.rauf").is_err());
    let _ = RAUF_FLAG_RECOVERED;
}

#[test]
fn raw_pcm_every_variant_round_trips_through_audio_file() {
    let dir = std::env::temp_dir().join(format!("mkff_audio_raw_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let src = AudioBuffer {
        sample_rate: 48_000,
        channels: vec![
            vec![0.0, 0.125, -0.125, 0.25, -0.25, 0.5, -0.5, 0.75, -0.75],
            vec![-0.5, 0.5, 0.0, -0.375, 0.375, -0.625, 0.625, -0.25, 0.25],
        ],
    };

    let variants: &[(PcmFormat, f32)] = &[
        (PcmFormat::u8(), 0.01),
        (PcmFormat::int(8, false), 0.01),
        (PcmFormat::int(16, false), 1e-4),
        (PcmFormat::int(16, true), 1e-4),
        (PcmFormat::int(24, false), 1e-6),
        (PcmFormat::int(24, true), 1e-6),
        (PcmFormat::int(32, false), 1e-7),
        (PcmFormat::int(32, true), 1e-7),
        (PcmFormat::float(32, false), 0.0),
        (PcmFormat::float(32, true), 0.0),
        (PcmFormat::float(64, false), 1e-7),
        (PcmFormat::float(64, true), 1e-7),
        (PcmFormat::alaw(), 0.03),
        (PcmFormat::mulaw(), 0.03),
    ];

    for &(fmt, tol) in variants {
        let encoded = encode_pcm(&src, fmt).unwrap();
        let data_offset = 37u64;
        let mut raw_bytes = vec![0xA5u8; data_offset as usize];
        raw_bytes.extend_from_slice(&encoded);
        // Append a partial trailing frame (1 byte short of a full stereo frame)
        let frame_bytes = fmt.bytes_per_sample() * 2;
        raw_bytes.extend(std::iter::repeat_n(0x33u8, frame_bytes - 1));

        let path = dir.join(format!("test_{}.raw", fmt.label()));
        std::fs::write(&path, &raw_bytes).unwrap();

        let spec = RawPcmSpec {
            sample_rate: 48_000,
            channels: 2,
            format: fmt,
            data_offset,
        };

        let mut af = AudioFile::open_raw_pcm(&path, spec).unwrap();
        assert_eq!(af.format_name(), "RAW PCM");
        assert_eq!(af.sample_rate(), 48_000);
        assert_eq!(af.channels(), 2);
        assert_eq!(af.duration(), Some(src.frames() as u64));

        let decoded = af.read_to_end().unwrap();
        assert_eq!(decoded.frames(), src.frames(), "{}", fmt.label());
        for (a, b) in src
            .channels
            .iter()
            .flatten()
            .zip(decoded.channels.iter().flatten())
        {
            assert!((a - b).abs() <= tol, "{}: {a} vs {b}", fmt.label());
        }

        // Also verify from_raw_pcm_reader and sample-exact seek
        let mut af_reader =
            AudioFile::from_raw_pcm_reader(Box::new(Cursor::new(raw_bytes)), spec).unwrap();
        af_reader.seek(4).unwrap();
        let tail = af_reader.read_to_end().unwrap();
        assert_eq!(tail.frames(), src.frames() - 4);
        assert!((tail.channels[0][0] - src.channels[0][4]).abs() <= tol);
        assert!((tail.channels[1][0] - src.channels[1][4]).abs() <= tol);
    }

    // Invalid spec rejected
    let invalid_spec = RawPcmSpec {
        sample_rate: 48_000,
        channels: 0,
        format: PcmFormat::s16le(),
        data_offset: 0,
    };
    assert!(matches!(
        AudioFile::from_raw_pcm_reader(Box::new(Cursor::new(vec![0u8; 32])), invalid_spec),
        Err(Error::InvalidArgument(_))
    ));
    let invalid_fmt_spec = RawPcmSpec {
        sample_rate: 48_000,
        channels: 2,
        format: PcmFormat {
            bits: 20,
            float: false,
            big_endian: false,
            signed: true,
            coding: PcmCoding::Linear,
        },
        data_offset: 0,
    };
    assert!(matches!(
        AudioFile::from_raw_pcm_reader(Box::new(Cursor::new(vec![0u8; 32])), invalid_fmt_spec),
        Err(Error::InvalidArgument(_))
    ));

    // AudioFile::open() on a .raw file of noise returns an error
    let noise_path = dir.join("noise.raw");
    let noise: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(&noise_path, &noise).unwrap();
    assert!(AudioFile::open(&noise_path).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
