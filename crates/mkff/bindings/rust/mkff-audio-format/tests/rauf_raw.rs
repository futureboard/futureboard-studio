//! In-memory tests for the RAUF v1 and headerless RAW PCM demuxers.

use std::io::Cursor;

use mkff_audio_format::{
    decode_pcm, encode_pcm, open, open_path, open_raw_pcm, open_raw_pcm_path, probe,
    read_rauf_info, AudioBuffer, CodecId, Error, PcmCoding, PcmFormat, RaufInfo, RawPcmSpec,
    AUDIO_EXTENSIONS, RAUF_FLAG_FINALIZED, RAUF_FLAG_HAS_PEAK, RAUF_FLAG_HAS_SIDECAR,
    RAUF_FLAG_RECOVERED, RAW_PCM_EXTENSIONS,
};

const RAUF_HEADER_SIZE: usize = 256;

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
) -> [u8; RAUF_HEADER_SIZE] {
    let mut h = [0u8; RAUF_HEADER_SIZE];
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
fn rauf_finalized_s32_and_f32_demux_and_seek() {
    assert!(AUDIO_EXTENSIONS.contains(&"rauf"));

    // Finalized stereo s32le across multiple 4096-frame packets
    let frames = 9_000usize;
    let channels = 2u16;
    let take_id = [0x11; 16];
    let header = build_rauf_header(
        1,
        256,
        48_000,
        channels,
        3,
        1,
        1,
        RAUF_FLAG_FINALIZED | RAUF_FLAG_HAS_SIDECAR,
        frames as u64,
        960_000,
        take_id,
        256,
    );
    let mut bytes = header.to_vec();
    let mut expected_l = Vec::with_capacity(frames);
    let mut expected_r = Vec::with_capacity(frames);
    for i in 0..frames {
        let l: i32 = match i {
            0 => i32::MIN,
            1 => -(1 << 30),
            2 => 0,
            3 => 1 << 30,
            4 => i32::MAX,
            _ => (i as i32 * 123_456).wrapping_sub(500_000_000),
        };
        let r: i32 = -l.saturating_add(1);
        bytes.extend_from_slice(&l.to_le_bytes());
        bytes.extend_from_slice(&r.to_le_bytes());
        expected_l.push((f64::from(l) / 2_147_483_648.0) as f32);
        expected_r.push((f64::from(r) / 2_147_483_648.0) as f32);
    }

    assert_eq!(probe(&bytes, ""), Some("RAUF"));
    assert_eq!(probe(&bytes, "take.rauf"), Some("RAUF"));

    let info = read_rauf_info(&mut Cursor::new(bytes.clone())).unwrap();
    assert_eq!(
        info,
        RaufInfo {
            project_start_sample: 960_000,
            take_id,
            flags: RAUF_FLAG_FINALIZED | RAUF_FLAG_HAS_SIDECAR,
            finalized: true,
            recovered: false,
        }
    );

    let mut demux = open(Box::new(Cursor::new(bytes)), "take.rauf").unwrap();
    assert_eq!(demux.format_name(), "RAUF");
    let track = demux.tracks()[0].clone();
    assert_eq!(track.codec, CodecId::Pcm);
    assert_eq!(track.pcm, Some(PcmFormat::int(32, false)));
    assert_eq!(track.bits_per_sample, Some(32));
    assert_eq!(track.name, "PCM s32le");
    assert_eq!(track.sample_rate, 48_000);
    assert_eq!(track.channels, 2);
    assert_eq!(track.duration, Some(frames as u64));

    let mut decoded = AudioBuffer::empty(48_000, 2);
    let mut pkt_count = 0usize;
    let mut next_pts = 0i64;
    while let Some(pkt) = demux.next_packet().unwrap() {
        assert_eq!(pkt.pts, next_pts);
        next_pts += i64::from(pkt.duration);
        pkt_count += 1;
        decoded.append(&decode_pcm(&pkt.data, track.pcm.unwrap(), 2));
    }
    assert_eq!(pkt_count, 3); // 4096 + 4096 + 808
    assert_eq!(decoded.channels[0], expected_l);
    assert_eq!(decoded.channels[1], expected_r);

    // Sample-exact seek
    for target in [0i64, 1, 3, 4095, 4096, 4097, 8999] {
        let landed = demux.seek(0, target).unwrap();
        assert_eq!(landed, target);
        let pkt = demux.next_packet().unwrap().unwrap();
        assert_eq!(pkt.pts, target);
        let b = decode_pcm(&pkt.data, track.pcm.unwrap(), 2);
        assert_eq!(b.channels[0][0], expected_l[target as usize]);
        assert_eq!(b.channels[1][0], expected_r[target as usize]);
    }
    assert!(demux.seek(1, 0).is_err());

    // Finalized mono f32le with custom data_offset > 256
    let f32_samples: [f32; 6] = [0.0, 1.0, -1.0, 0.25, -0.75, 0.123_456_7];
    let header_f32 = build_rauf_header(
        1,
        256,
        44_100,
        1,
        4,
        1,
        1,
        RAUF_FLAG_FINALIZED | RAUF_FLAG_HAS_PEAK,
        f32_samples.len() as u64,
        12_345,
        [0xAB; 16],
        320,
    );
    let mut f32_bytes = header_f32.to_vec();
    f32_bytes.resize(320, 0xFE); // padding between header and data_offset
    for s in f32_samples {
        f32_bytes.extend_from_slice(&s.to_le_bytes());
    }
    let mut demux_f32 = open(Box::new(Cursor::new(f32_bytes)), "float.rauf").unwrap();
    let t_f32 = demux_f32.tracks()[0].clone();
    assert_eq!(t_f32.pcm, Some(PcmFormat::float(32, false)));
    assert_eq!(t_f32.bits_per_sample, Some(32));
    assert_eq!(t_f32.name, "PCM f32le");
    assert_eq!(t_f32.duration, Some(f32_samples.len() as u64));
    let pkt = demux_f32.next_packet().unwrap().unwrap();
    let b = decode_pcm(&pkt.data, t_f32.pcm.unwrap(), 1);
    assert_eq!(b.channels[0], f32_samples);
}

#[test]
fn rauf_unfinalized_recovery_and_truncated_partial_frame() {
    // Unfinalized file: frames_written = 0, flags = 0, plus a 3-byte partial trailing frame
    let channels = 2u16;
    let take_id = [0x42; 16];
    let header = build_rauf_header(
        1, 256, 48_000, channels, 3, 1, 1, 0, 0, 48_000, take_id, 256,
    );
    let mut bytes = header.to_vec();
    let whole_frames = 5usize;
    for i in 0..(whole_frames * channels as usize) {
        let s = ((i as i32) + 1) * 10_000_000;
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    // Append 5 trailing bytes (less than one 8-byte stereo s32 frame)
    bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);

    let info = read_rauf_info(&mut Cursor::new(bytes.clone())).unwrap();
    assert!(!info.finalized);
    assert!(info.recovered);
    assert_eq!(info.project_start_sample, 48_000);
    assert_eq!(info.take_id, take_id);

    let mut demux = open(Box::new(Cursor::new(bytes)), "crashed.rauf").unwrap();
    assert_eq!(demux.tracks()[0].duration, Some(whole_frames as u64));
    let pkt = demux.next_packet().unwrap().unwrap();
    assert_eq!(pkt.duration, whole_frames as u32);
    assert_eq!(pkt.data.len(), whole_frames * 8);
    assert!(demux.next_packet().unwrap().is_none());

    // Finalized header with RAUF_FLAG_RECOVERED bit explicitly set, and frames_written larger
    // than the actual file size on disk: must clamp to the whole frames on disk.
    let header_clamped = build_rauf_header(
        1,
        256,
        48_000,
        1,
        4,
        1,
        1,
        RAUF_FLAG_FINALIZED | RAUF_FLAG_RECOVERED,
        999_999_999,
        0,
        [0; 16],
        256,
    );
    let mut clamped_bytes = header_clamped.to_vec();
    for s in [0.25f32, -0.25, 0.5] {
        clamped_bytes.extend_from_slice(&s.to_le_bytes());
    }
    clamped_bytes.push(0xFF); // partial 4th sample
    let info2 = read_rauf_info(&mut Cursor::new(clamped_bytes.clone())).unwrap();
    assert!(info2.finalized);
    assert!(info2.recovered);
    let mut demux2 = open(Box::new(Cursor::new(clamped_bytes)), "clamped.rauf").unwrap();
    assert_eq!(demux2.tracks()[0].duration, Some(3));
    let pkt2 = demux2.next_packet().unwrap().unwrap();
    assert_eq!(pkt2.duration, 3);
}

#[test]
fn rauf_invalid_headers_return_errors_without_panicking() {
    let valid = build_rauf_header(
        1,
        256,
        48_000,
        2,
        3,
        1,
        1,
        RAUF_FLAG_FINALIZED,
        0,
        0,
        [0; 16],
        256,
    );

    // Truncated header (< 256 bytes)
    assert!(read_rauf_info(&mut Cursor::new(valid[..100].to_vec())).is_err());
    assert!(open(Box::new(Cursor::new(valid[..100].to_vec())), "x.rauf").is_err());

    // Wrong magic
    let mut bad_magic = valid;
    bad_magic[0..4].copy_from_slice(b"NOPE");
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(bad_magic.to_vec())),
        Err(Error::InvalidData(_))
    ));
    assert!(open(Box::new(Cursor::new(bad_magic.to_vec())), "x.rauf").is_err());

    // Version 2
    let ver2 = build_rauf_header(2, 256, 48_000, 2, 3, 1, 1, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(ver2.to_vec())),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        open(Box::new(Cursor::new(ver2.to_vec())), "x.rauf"),
        Err(Error::Unsupported(_))
    ));

    // Bad header_size
    let bad_hsize = build_rauf_header(1, 128, 48_000, 2, 3, 1, 1, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(bad_hsize.to_vec())),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        open(Box::new(Cursor::new(bad_hsize.to_vec())), "x.rauf"),
        Err(Error::InvalidData(_))
    ));

    // Big-endian flag (endianness = 2)
    let big_end = build_rauf_header(1, 256, 48_000, 2, 3, 2, 1, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(big_end.to_vec())),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        open(Box::new(Cursor::new(big_end.to_vec())), "x.rauf"),
        Err(Error::Unsupported(_))
    ));

    // Non-interleaved (interleaved = 0)
    let planar = build_rauf_header(1, 256, 48_000, 2, 3, 1, 0, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(planar.to_vec())),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        open(Box::new(Cursor::new(planar.to_vec())), "x.rauf"),
        Err(Error::Unsupported(_))
    ));

    // Zero channels
    let zero_ch = build_rauf_header(1, 256, 48_000, 0, 3, 1, 1, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(zero_ch.to_vec())),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        open(Box::new(Cursor::new(zero_ch.to_vec())), "x.rauf"),
        Err(Error::InvalidData(_))
    ));

    // Zero sample_rate
    let zero_sr = build_rauf_header(1, 256, 0, 2, 3, 1, 1, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(zero_sr.to_vec())),
        Err(Error::InvalidData(_))
    ));

    // Unsupported sample_format
    let bad_fmt = build_rauf_header(1, 256, 48_000, 2, 99, 1, 1, 1, 0, 0, [0; 16], 256);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(bad_fmt.to_vec())),
        Err(Error::Unsupported(_))
    ));

    // data_offset before end of header or beyond end of file
    let off_small = build_rauf_header(1, 256, 48_000, 2, 3, 1, 1, 1, 0, 0, [0; 16], 128);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(off_small.to_vec())),
        Err(Error::InvalidData(_))
    ));
    let off_past = build_rauf_header(1, 256, 48_000, 2, 3, 1, 1, 1, 0, 0, [0; 16], 512);
    assert!(matches!(
        read_rauf_info(&mut Cursor::new(off_past.to_vec())),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        open(Box::new(Cursor::new(off_past.to_vec())), "x.rauf"),
        Err(Error::InvalidData(_))
    ));
}

#[test]
fn raw_pcm_demuxer_variants_offset_truncation_and_validation() {
    assert_eq!(RAW_PCM_EXTENSIONS, &["raw", "pcm"]);
    for ext in RAW_PCM_EXTENSIONS {
        assert!(!AUDIO_EXTENSIONS.contains(ext));
    }

    let variants = [
        (PcmFormat::u8(), 0.01f32),
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

    let src_buf = AudioBuffer {
        sample_rate: 48_000,
        channels: vec![
            vec![0.0, 0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 0.125],
            vec![-0.125, 0.5, -0.5, 0.0, 0.375, -0.375, 0.625, -0.625],
        ],
    };

    for (fmt, tol) in variants {
        let encoded = encode_pcm(&src_buf, fmt).unwrap();
        let data_offset = 19u64;
        let mut raw_bytes = vec![0xDE; data_offset as usize];
        raw_bytes.extend_from_slice(&encoded);
        // Add a partial trailing frame (1 byte less than a stereo frame)
        let frame_bytes = fmt.bytes_per_sample() * 2;
        raw_bytes.extend(std::iter::repeat_n(0x7F, frame_bytes - 1));

        let spec = RawPcmSpec {
            sample_rate: 48_000,
            channels: 2,
            format: fmt,
            data_offset,
        };
        let mut demux = open_raw_pcm(Box::new(Cursor::new(raw_bytes)), spec).unwrap();
        assert_eq!(demux.format_name(), "RAW PCM");
        let track = demux.tracks()[0].clone();
        assert_eq!(track.codec, CodecId::Pcm);
        assert_eq!(track.pcm, Some(fmt));
        assert_eq!(track.sample_rate, 48_000);
        assert_eq!(track.channels, 2);
        assert_eq!(track.duration, Some(src_buf.frames() as u64));

        let pkt = demux.next_packet().unwrap().unwrap();
        assert_eq!(pkt.duration, src_buf.frames() as u32);
        let decoded = decode_pcm(&pkt.data, fmt, 2);
        assert_eq!(decoded.frames(), src_buf.frames());
        for (a, b) in src_buf
            .channels
            .iter()
            .flatten()
            .zip(decoded.channels.iter().flatten())
        {
            assert!((a - b).abs() <= tol, "{}: {a} vs {b}", fmt.label());
        }

        // Sample-exact seek
        let landed = demux.seek(0, 3).unwrap();
        assert_eq!(landed, 3);
        let pkt_seek = demux.next_packet().unwrap().unwrap();
        assert_eq!(pkt_seek.pts, 3);
        let dec_seek = decode_pcm(&pkt_seek.data, fmt, 2);
        assert!((dec_seek.channels[0][0] - src_buf.channels[0][3]).abs() <= tol);
        assert!((dec_seek.channels[1][0] - src_buf.channels[1][3]).abs() <= tol);
    }

    // Invalid specs rejected with Error::InvalidArgument
    for bad_spec in [
        RawPcmSpec {
            sample_rate: 0,
            channels: 2,
            format: PcmFormat::s16le(),
            data_offset: 0,
        },
        RawPcmSpec {
            sample_rate: 48_000,
            channels: 0,
            format: PcmFormat::s16le(),
            data_offset: 0,
        },
        RawPcmSpec {
            sample_rate: 48_000,
            channels: 65,
            format: PcmFormat::s16le(),
            data_offset: 0,
        },
        RawPcmSpec {
            sample_rate: 48_000,
            channels: 2,
            format: PcmFormat {
                bits: 12,
                float: false,
                big_endian: false,
                signed: true,
                coding: PcmCoding::Linear,
            },
            data_offset: 0,
        },
    ] {
        assert!(matches!(
            open_raw_pcm(Box::new(Cursor::new(vec![0u8; 64])), bad_spec),
            Err(Error::InvalidArgument(_))
        ));
    }

    // open() and open_path() on a .raw/.pcm file of noise must fail
    let noise: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    assert_eq!(probe(&noise, "dump.raw"), None);
    assert_eq!(probe(&noise, "dump.pcm"), None);
    assert!(open(Box::new(Cursor::new(noise.clone())), "dump.raw").is_err());
    assert!(open(Box::new(Cursor::new(noise.clone())), "dump.pcm").is_err());

    let tmp_path = std::env::temp_dir().join(format!("mkff_raw_test_{}.raw", std::process::id()));
    std::fs::write(&tmp_path, &noise).unwrap();
    assert!(open_path(&tmp_path).is_err());
    let valid_spec = RawPcmSpec {
        sample_rate: 44_100,
        channels: 2,
        format: PcmFormat::s16le(),
        data_offset: 0,
    };
    let demux_path = open_raw_pcm_path(&tmp_path, valid_spec).unwrap();
    assert_eq!(demux_path.format_name(), "RAW PCM");
    assert_eq!(demux_path.tracks()[0].duration, Some(1024));
    let _ = std::fs::remove_file(&tmp_path);
}
