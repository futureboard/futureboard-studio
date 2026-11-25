//! Real decodes of MKFF's checked-in test clips through the public API.

use std::path::PathBuf;
use std::sync::Arc;

use sphere_video_player::{VideoDecoder, VideoError, VideoFrame};

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../mkff/testdata")
        .join(rel)
}

/// Not a flat or empty picture: the luma-ish spread is real image content.
fn assert_real_picture(frame: &VideoFrame) {
    let expected = frame.width as usize * frame.height as usize * 4;
    assert!(frame.width > 0 && frame.height > 0);
    assert_eq!(frame.bgra.len(), expected);
    let (min, max) = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| (u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3)
        .fold((255u32, 0u32), |(lo, hi), v| (lo.min(v), hi.max(v)));
    assert!(max > min + 8, "flat picture {min}..{max}");
    assert!(frame.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
}

fn exercise(rel: &str) -> VideoDecoder {
    let mut decoder = VideoDecoder::open(fixture(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let info = decoder.info();
    assert!(info.width > 0 && info.height > 0, "{rel}: {info:?}");
    assert!(info.duration_seconds > 0.0, "{rel}: {info:?}");
    assert!(info.frame_rate > 0.0, "{rel}: {info:?}");

    let first = decoder.frame_at(0.0).unwrap();
    assert_real_picture(&first);
    // The same position inside the same frame hands back the same image.
    let again = decoder.frame_at(0.0).unwrap();
    assert!(
        Arc::ptr_eq(&first, &again),
        "{rel}: re-decoded an unchanged frame"
    );

    // Forward stepping, then a backwards seek.
    let step = 1.0 / info.frame_rate;
    let mut last = first.timestamp_seconds;
    let mut t = step;
    while t < info.duration_seconds.min(1.0) {
        let frame = decoder.frame_at(t).unwrap();
        assert_real_picture(&frame);
        assert!(
            frame.timestamp_seconds >= last,
            "{rel}: went backwards at {t}"
        );
        last = frame.timestamp_seconds;
        t += step;
    }
    let back = decoder.frame_at(0.0).unwrap();
    assert_real_picture(&back);
    assert!(
        back.timestamp_seconds <= step,
        "{rel}: seek back landed at {back:?}"
    );

    assert_eq!(
        decoder.frame_at(info.duration_seconds + 1.0).err(),
        Some(VideoError::EndOfStream)
    );
    decoder
}

#[test]
fn h264_mp4_decodes_on_the_gpu_or_in_software() {
    let decoder = exercise("tiny_main_256x144.mp4");
    let info = decoder.info();
    assert_eq!((info.width, info.height), (256, 144));
    assert!(matches!(info.decoder, "GPU" | "software"));
}

#[test]
fn vp9_webm_decodes_in_software() {
    let decoder = exercise("codecs/vp9_opus.webm");
    assert_eq!(decoder.info().decoder, "software");
}

#[test]
fn prores_mov_falls_through_to_software() {
    let decoder = exercise("codecs/prores_hq.mov");
    assert_eq!(decoder.info().decoder, "software");
}

#[test]
fn audio_only_file_has_no_video_stream() {
    let result = VideoDecoder::open(fixture("audio/format/flac.mp4"));
    assert!(result.is_err(), "audio-only MP4 opened as video");
}
