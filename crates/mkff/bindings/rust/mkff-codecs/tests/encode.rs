//! Encode → mux → demux → decode round trips for the `encode` module.

use mkff_codecs::encode::{AudioEncoder, Container, MediaWriter, ProResProfile, VideoEncoder, VideoEncoderConfig};
use mkff_codecs::MediaFile;

const W: u32 = 128;
const H: u32 = 72;
const FRAMES: u64 = 30;

/// A moving gradient; frame `i` has a bright bar at column `i * 4`.
fn picture(i: u64) -> Vec<u8> {
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let p = ((y * W + x) * 4) as usize;
            let bar = (x as u64 / 4) == i % (W as u64 / 4);
            rgba[p] = if bar { 250 } else { (x * 2) as u8 };
            rgba[p + 1] = (y * 3) as u8;
            rgba[p + 2] = 90;
            rgba[p + 3] = 255;
        }
    }
    rgba
}

fn roundtrip(cfg: VideoEncoderConfig, container: Container, fragmented: bool, name: &str) {
    let path = std::env::temp_dir().join(format!("mkff-encode-{name}-{}.{}", std::process::id(), if container == Container::Mov { "mov" } else { "mp4" }));
    let mut video = VideoEncoder::new(cfg).unwrap();
    let mut audio = AudioEncoder::aac(48_000, 2, 128_000).unwrap();
    let mut out = MediaWriter::create(&path, container, fragmented).unwrap();
    let v = out.add_video_track(&video).unwrap();
    let a = out.add_audio_track(&audio).unwrap();
    for i in 0..FRAMES {
        for p in video.encode_rgba(&picture(i), i).unwrap() {
            out.write(v, &p).unwrap();
        }
    }
    for p in video.flush().unwrap() {
        out.write(v, &p).unwrap();
    }
    let tone: Vec<f32> = (0..48_000).map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3).collect();
    for p in audio.encode(&[&tone, &tone]).unwrap() {
        out.write(a, &p).unwrap();
    }
    for p in audio.flush().unwrap() {
        out.write(a, &p).unwrap();
    }
    out.finish().unwrap();

    let media = MediaFile::open(&path).unwrap();
    let info = media.info();
    let vs = info.video.as_ref().expect("video stream");
    assert_eq!((vs.width, vs.height), (W, H), "{name}");
    assert_eq!(media.frame_count(), FRAMES, "{name}");
    let f = media.video_frame(10).unwrap();
    let rgba = f.to_rgba8();
    // The bar of frame 10 sits at x = 40..44: bright red, against a darker gradient.
    let at = |x: u32, y: u32| rgba[((y * W + x) * 4) as usize];
    assert!(at(41, 36) > 200 && at(20, 36) < 80, "{name}: bar {} vs {}", at(41, 36), at(20, 36));
    let pcm = media.audio(0, 4800, 48_000).unwrap();
    assert_eq!(pcm.channel_count(), 2);
    let peak = pcm.channels[0][1024..].iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!((0.2..0.4).contains(&peak), "{name}: audio peak {peak}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn h264_aac_mp4() {
    roundtrip(VideoEncoderConfig::h264(W, H, 30, 1), Container::Mp4, false, "h264");
}

#[test]
fn h264_aac_fragmented_mp4() {
    roundtrip(VideoEncoderConfig::h264(W, H, 30, 1), Container::Mp4, true, "h264-frag");
}

#[test]
fn prores_aac_mov() {
    roundtrip(VideoEncoderConfig::prores(W, H, 30, 1, ProResProfile::Hq), Container::Mov, false, "prores");
}


#[test]
fn h264_bframes_mp4() {
    let mut cfg = VideoEncoderConfig::h264(W, H, 30, 1);
    if let mkff_codecs::encode::VideoEncodeCodec::H264(s) = &mut cfg.codec {
        s.bframes = 2;
        s.preset = mkff_codecs::encode::H264Preset::Balanced;
    }
    roundtrip(cfg, Container::Mp4, false, "h264-b");
}
