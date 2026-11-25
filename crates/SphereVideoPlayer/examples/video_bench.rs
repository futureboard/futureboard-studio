//! Decode timing for one file through the public API, as the preview worker
//! drives it: sequential frames (playback), then a few far seeks (scrubbing).
//!
//! `cargo run --release -p sphere-video-player --example video_bench -- <file> [frames]`

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: video_bench <file> [frames]");
    let frames: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);

    let t = Instant::now();
    let mut decoder = sphere_video_player::VideoDecoder::open(&path).expect("open");
    let info = decoder.info();
    println!("open {:.1} ms  {info:?}", t.elapsed().as_secs_f64() * 1e3);

    let step = 1.0 / info.frame_rate.max(1.0);
    let mut times = Vec::with_capacity(frames);
    for i in 0..frames {
        let t = Instant::now();
        decoder.frame_at(i as f64 * step).expect("frame");
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    report("sequential", &mut times, step * 1e3);

    let mut seeks = Vec::new();
    for k in 1..=5 {
        let t = Instant::now();
        decoder
            .frame_at(info.duration_seconds * k as f64 / 6.0)
            .expect("seek");
        seeks.push(t.elapsed().as_secs_f64() * 1e3);
    }
    report("far seek", &mut seeks, f64::NAN);
    println!("decoder: {}", decoder.info().decoder);

    // Software path split into its two halves: MKFF decode vs RGBA convert.
    let media = mkff_codecs::MediaFile::open(&path).expect("open");
    let (mut decode, mut convert) = (Vec::new(), Vec::new());
    for i in 0..frames as u64 {
        let t = Instant::now();
        let frame = media.video_frame(i).expect("frame");
        decode.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        std::hint::black_box(frame.to_rgba8());
        convert.push(t.elapsed().as_secs_f64() * 1e3);
    }
    if std::env::var_os("VIDEO_BENCH_SERIES").is_some() {
        let series: Vec<String> = decode.iter().map(|ms| format!("{ms:.0}")).collect();
        println!("decode ms per frame: {}", series.join(" "));
    }
    report("sw decode", &mut decode, step * 1e3);
    report("to_rgba8", &mut convert, step * 1e3);

    streaming(&path, frames);
    if std::env::var_os("VIDEO_BENCH_COMPARE").is_some() {
        compare(&path, &media, info.duration_seconds);
    }
}

/// The decoder `VideoDecoder` picked (GPU when it can) against MKFF software
/// at the same times: mean absolute BGRA difference per frame.
fn compare(path: &str, media: &mkff_codecs::MediaFile, duration: f64) {
    let mut chosen = sphere_video_player::VideoDecoder::open(path).expect("open");
    let fps = chosen.info().frame_rate;
    // Forward steps, a jump across a keyframe, then backwards.
    let times = [0.0, 0.5, 1.0, 1.02, 4.5, 8.4, 3.0, 0.1, duration * 0.95];
    for t in times {
        let a = chosen.frame_at(t).expect("frame");
        let index = (t * fps + 1e-6).floor() as u64;
        let mut b = media.video_frame(index).expect("sw").to_rgba8();
        for px in b.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        let diff: u64 = a
            .bgra
            .iter()
            .zip(&b)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum();
        let luma = |px: &[u8; 4]| (u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2])) / 3;
        let (lo, hi) = a
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .map(luma)
            .fold((255, 0), |(l, h), v| (l.min(v), h.max(v)));
        let mid = (a.height as usize / 2 * a.width as usize + a.width as usize / 2) * 4;
        println!(
            "compare t={t:>6.3}s ({}x{} {} vs sw): mean abs diff {:.3}  range {lo}..{hi}  centre gpu {:?} sw {:?}  ptr_eq_len {}",
            a.width,
            a.height,
            chosen.info().decoder,
            diff as f64 / a.bgra.len() as f64,
            &a.bgra[mid..mid + 4],
            &b[mid..mid + 4],
            a.bgra.len() == b.len()
        );
    }
}

/// Throughput when samples are fed in order without waiting on each frame
/// (what a playback pipeline does): MP4 only.
fn streaming(path: &str, frames: usize) {
    use mkff_codecs::filmcraft::isobmff::{self, CodecConfig, TrackKind};
    use mkff_codecs::{VideoCodec, VideoDecoder};
    let file = std::fs::File::open(path).expect("open");
    let Ok(mp4) = isobmff::open(&file) else {
        println!("streaming: not MP4");
        return;
    };
    let track = mp4.track_of_kind(TrackKind::Video).expect("video");
    let t = &mp4.tracks[track];
    let (codec, config) = match t.codec() {
        Some(CodecConfig::Avc(c)) => (VideoCodec::H264, c.to_bytes()),
        Some(CodecConfig::Hevc(c)) => (VideoCodec::Hevc, c.to_bytes()),
        Some(CodecConfig::Av1(c)) => (VideoCodec::Av1, c.to_bytes()),
        Some(CodecConfig::Vp9(_)) => (VideoCodec::Vp9, Vec::new()),
        other => {
            println!("streaming: {:?} not covered", other.map(|c| c.name()));
            return;
        }
    };
    let mut dec = VideoDecoder::with_config(codec, &config).expect("decoder");
    let n = frames.min(t.samples.len());
    let start = Instant::now();
    let mut out = 0usize;
    for i in 0..n {
        let data = mp4.read_sample(&file, track, i).expect("sample");
        out += dec.decode(&data, i as i64).expect("decode").len();
    }
    out += dec.flush().len();
    let ms = start.elapsed().as_secs_f64() * 1e3;
    println!(
        " streaming: {n} samples -> {out} frames in {ms:.0} ms = {:.2} ms/frame ({})",
        ms / out.max(1) as f64,
        dec.name()
    );
}

fn report(label: &str, times: &mut [f64], budget_ms: f64) {
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    let max = times[times.len() - 1];
    println!(
        "{label:>10}: n={} mean {mean:.2} ms  p95 {p95:.2} ms  max {max:.2} ms{}",
        times.len(),
        if budget_ms.is_finite() {
            format!("  (frame budget {budget_ms:.2} ms)")
        } else {
            String::new()
        }
    );
}
