//! Encode with every hardware encoder on this machine, mux with mkff-codecs, decode back.
//! Machines without one pass trivially (the test prints what it skipped).

use std::time::Instant;

use mkff_codecs::encode::{Container, MediaWriter};
use mkff_codecs::MediaFile;
use mkff_hwenc::{HwEncoderConfig, HwVideoEncoder};

fn frame(w: usize, h: usize, i: u64) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    for (p, px) in rgba.chunks_exact_mut(4).enumerate() {
        let (x, y) = (p % w, p / w);
        // Left half red, right half a moving gradient.
        if x < w / 2 {
            px.copy_from_slice(&[220, 30, 30, 255]);
        } else {
            px.copy_from_slice(&[((x as u64 + i * 8) % 256) as u8, (y % 256) as u8, 128, 255]);
        }
    }
    rgba
}

#[test]
fn every_hardware_encoder_roundtrips() {
    let encoders = mkff_hwenc::h264_encoders();
    if encoders.is_empty() {
        eprintln!("no hardware H.264 encoder; skipped");
        return;
    }
    for info in encoders {
        let (w, h, fps) = (1920usize, 1080usize, 60u32);
        let mut cfg = HwEncoderConfig::cbr(w as u32, h as u32, fps, 1, 6000);
        cfg.keyint = 30;
        let mut enc = match HwVideoEncoder::new(Some(info.vendor), cfg) {
            Ok(e) => e,
            Err(e) => panic!("{} ({:?}): open failed: {e}", info.name, info.vendor),
        };
        let frames = 120u64;
        let pics: Vec<Vec<u8>> = (0..4).map(|i| frame(w, h, i)).collect();
        let mut packets = Vec::new();
        let t = Instant::now();
        for i in 0..frames {
            packets.extend(enc.encode_rgba(&pics[(i % 4) as usize], i).expect("encode"));
        }
        packets.extend(enc.flush().expect("flush"));
        let ms = t.elapsed().as_secs_f64() * 1000.0 / frames as f64;
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        let kbps = bytes as f64 * 8.0 / (frames as f64 / fps as f64) / 1000.0;
        eprintln!("{} [{}]: {} packets, {ms:.2} ms/frame (incl. RGBA→NV12), {kbps:.0} kb/s", info.name, info.vendor.api_name(), packets.len());
        assert!(packets.len() as u64 >= frames - 2, "{} packets for {frames} frames", packets.len());
        assert!(packets[0].keyframe, "first packet must be a keyframe");
        let keys = packets.iter().filter(|p| p.keyframe).count();
        assert!(keys >= 4, "a 30-frame GOP should give 4 keyframes in 120 frames, got {keys}");
        for pair in packets.windows(2) {
            assert!(pair[1].dts > pair[0].dts, "DTS must increase: {} then {}", pair[0].dts, pair[1].dts);
        }
        let avcc = enc.avcc().expect("avcC after encoding").to_vec();

        // Mux with MKFF and decode it back with MKFF.
        let path = std::env::temp_dir().join(format!("mkff-hwenc-{:?}-{}.mp4", info.vendor, std::process::id()));
        let mut out = MediaWriter::create(&path, Container::Mp4, false).expect("writer");
        let v = out.add_h264_track(&avcc, w as u16, h as u16, enc.timescale()).expect("track");
        for p in &packets {
            out.write(v, p).expect("write");
        }
        out.finish().expect("finish");
        let clip = MediaFile::open(&path).expect("open with MKFF");
        let vi = clip.info().video.clone().expect("video");
        assert_eq!((vi.width, vi.height), (w as u32, h as u32));
        assert!((clip.info().duration_seconds - 2.0).abs() < 0.1, "duration {}", clip.info().duration_seconds);
        let f = clip.video_frame(30).expect("decode frame 30");
        let rgba = f.to_rgba8();
        let px = &rgba[(h / 2 * w + w / 4) * 4..][..3];
        assert!(px[0] > 170 && px[1] < 80 && px[2] < 80, "{}: red half decoded as {px:?}", info.name);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn encoder_through_the_media_roster() {
    use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat, MediaRoster, PixelFormat, VideoFrame};
    let roster = MediaRoster::new();
    mkff_hwenc::addon::register(&roster);
    let input = MediaFormat::raw_video(320, 240, PixelFormat::Rgba8);
    let output = MediaFormat::encoded(CodecFamily::H264, Vec::new(), 0, 0);
    let Ok((flavor, mut enc)) = roster.video_encoder(&input, &output, AccelPolicy::HardwareOnly) else {
        eprintln!("skipping: no hardware H.264 encoder");
        return;
    };
    assert!(flavor.info.accel.is_hardware());
    let mut chunks = Vec::new();
    for i in 0..30u8 {
        let rgba: Vec<u8> = (0..320 * 240 * 4).map(|p| (p as u8).wrapping_add(i * 8)).collect();
        let f = VideoFrame { width: 320, height: 240, format: PixelFormat::Rgba8, planes: vec![rgba], strides: vec![1280], ..Default::default() };
        chunks.extend(enc.encode(&f).unwrap());
    }
    chunks.extend(enc.flush().unwrap());
    assert!(chunks.len() >= 28, "{} packets", chunks.len());
    assert!(chunks[0].keyframe);
    assert!(matches!(enc.output_format(), MediaFormat::EncodedVideo(e) if !e.config.is_empty()));
}
