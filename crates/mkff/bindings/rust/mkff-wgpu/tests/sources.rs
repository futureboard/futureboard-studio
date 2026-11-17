//! `VideoSource` opens every container: libmkff for MP4 / Annex-B, mkff-codecs for the rest and
//! whenever libmkff can't decode the clip here (CI runners have no video hardware). No GPU needed.

use std::path::PathBuf;

use mkff_wgpu::{decode_video_file, VideoSource};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../testdata")
        .join(name)
}

#[test]
fn opens_every_container() {
    for (name, w, h) in [
        ("codecs/vp9_opus.webm", 64, 48),
        ("codecs/vp8_vorbis.webm", 64, 48),
        ("codecs/av1_aac.mp4", 64, 48),
        ("codecs/hevc10_ac3.mkv", 64, 48),
        ("codecs/mpeg2_mp2.ts", 64, 48),
        ("codecs/h264_aac.ts", 64, 48),
        ("codecs/prores_hq.mov", 64, 48),
        ("codecs/dnxhr.mxf", 256, 128),
        ("codecs/mjpeg_pcm.avi", 64, 48),
        ("codecs/h264_high.h264", 64, 48),
        ("tiny_main_256x144.mp4", 256, 144),
        ("tiny_main10_64x64.hevc", 64, 64),
    ] {
        let frames = decode_video_file(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!frames.is_empty(), "{name}");
        for f in &frames {
            assert_eq!((f.width, f.height), (w, h), "{name}");
            assert_eq!(f.y.len(), (w * h) as usize, "{name}");
            assert_eq!(
                f.uv.len(),
                (w.div_ceil(2) * 2 * h.div_ceil(2)) as usize,
                "{name}"
            );
            let (lo, hi) =
                f.y.iter()
                    .fold((255u8, 0u8), |(a, b), &y| (a.min(y), b.max(y)));
            assert!(hi - lo > 100, "{name}: flat picture");
        }
    }
}

#[test]
fn random_access_in_software() {
    let mut src = VideoSource::open(&fixture("codecs/h264_aac.ts")).unwrap();
    let n = src.frame_count();
    assert!(n >= 6);
    let last = src.frame(n - 1).unwrap().y.clone();
    let first = src.frame(0).unwrap().y.clone();
    assert_ne!(first, last);
    assert_eq!(src.frame(0).unwrap().y, first);
}
