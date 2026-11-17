//! Print what mkff-codecs sees in a media file, and optionally dump decoded video as raw NV12.
//!
//! ```sh
//! cargo run -p mkff-codecs --release --example probe -- clip.mkv
//! cargo run -p mkff-codecs --release --example probe -- clip.mkv --nv12 out.yuv --frames 10
//! cargo run -p mkff-codecs --release --example probe -- clip.mov --planar out.yuv
//! ```
//!
//! `--planar` writes the decoder's native planes (Y, Cb, Cr; 16-bit little-endian above 8 bits),
//! i.e. ffmpeg's `yuv420p` / `yuv422p10le` / … layouts.

use std::io::Write;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: probe <file> [--nv12 <out.yuv> | --planar <out.yuv>] [--frames N]");
        std::process::exit(2);
    };
    let (mut out, mut planar, mut limit) = (None, false, u64::MAX);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--nv12" => out = args.next(),
            "--planar" => {
                out = args.next();
                planar = true;
            }
            "--frames" => limit = args.next().and_then(|n| n.parse().ok()).unwrap_or(u64::MAX),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let m = match mkff_codecs::MediaFile::open(&path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let i = m.info();
    println!(
        "{}: {} ({:?}), {:.3} s",
        i.name, i.container, i.kind, i.duration_seconds
    );
    if let Some(v) = &i.video {
        println!(
            "  video: {} {}x{} @ {}/{} fps, {}{}",
            v.codec,
            v.width,
            v.height,
            v.frame_rate.0,
            v.frame_rate.1,
            v.pixel_format,
            if v.has_alpha { ", alpha" } else { "" }
        );
    }
    for (k, a) in i.audio.iter().enumerate() {
        println!(
            "  audio #{k}: {} {} Hz, {} ch",
            a.codec, a.sample_rate, a.channels
        );
    }

    if i.video.is_none() {
        return;
    }
    let mut file =
        out.map(|p| std::io::BufWriter::new(std::fs::File::create(p).expect("create output")));
    let n = m.frame_count().min(limit);
    let t0 = Instant::now();
    for k in 0..n {
        let f = match m.video_frame(k) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("frame {k}: {e}");
                std::process::exit(1);
            }
        };
        if let Some(w) = file.as_mut() {
            let bytes = match (planar, f.planes()) {
                (false, _) => f.to_nv12(),
                (true, mkff_codecs::Planes::U8 { y, u, v, .. }) => [y, u, v].concat(),
                (true, mkff_codecs::Planes::U16 { y, u, v, .. }) => [y, u, v]
                    .concat()
                    .iter()
                    .flat_map(|s| s.to_le_bytes())
                    .collect(),
                (true, _) => f.to_rgba8(),
            };
            w.write_all(&bytes).expect("write");
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "  decoded {n} frames in {dt:.3} s ({:.1} fps)",
        n as f64 / dt.max(1e-9)
    );
}
