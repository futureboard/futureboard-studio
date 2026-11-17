//! Print every GPU accelerant and its hooks, the roster's flavors, and optionally decode an
//! Annex-B H.264 / HEVC file on the GPU.
//!
//! `cargo run -p mkff-mediakit --example accel_info [file.h264|file.hevc]`

use mkff_mediakit::{accelerant, AccelPolicy, CodecFamily, MediaFormat, MediaRoster};

fn main() {
    let roster = MediaRoster::roster();
    print!("{}", accelerant::describe(roster.accelerants()));
    println!("flavors:");
    for f in roster.flavors() {
        println!("  [{}] {} — {}", f.addon_name, f.info.name, f.info.accel);
    }
    let Some(path) = std::env::args().nth(1) else {
        return;
    };
    let data = std::fs::read(&path).expect("read input");
    let codec = if path.ends_with(".hevc") || path.ends_with(".h265") {
        CodecFamily::Hevc
    } else {
        CodecFamily::H264
    };
    let fmt = MediaFormat::encoded(codec, Vec::new(), 0, 0);
    match roster.video_decoder(&fmt, AccelPolicy::HardwareOnly) {
        Ok((flavor, mut dec)) => {
            println!(
                "decoding {path} with {} ({})",
                flavor.info.name,
                dec.acceleration()
            );
            let t = std::time::Instant::now();
            let mut frames = dec.decode(&data, 0).expect("decode");
            frames.extend(dec.flush().expect("flush"));
            let el = t.elapsed();
            for f in frames.iter().take(3) {
                println!(
                    "  {}×{} {:?} pts {} gpu {:?} planes {}",
                    f.width,
                    f.height,
                    f.format,
                    f.pts,
                    f.gpu,
                    f.planes.len()
                );
            }
            println!("{} frames in {el:?}", frames.len());
        }
        Err(e) => println!("no hardware decoder: {e}"),
    }
}
