//! Picture geometry through every hardware encoder: frame sizes that aren't multiples of 16
//! (360, 1080, odd-macroblock widths) must come back unshifted, corners and centre alike.

use mkff_codecs::encode::{Container, MediaWriter};
use mkff_codecs::MediaFile;
use mkff_hwenc::{HwEncoderConfig, HwVideoEncoder, RateControl};

/// Quadrants: red, green / blue, white.
fn quadrants(w: usize, h: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let c: [u8; 4] = match (x < w / 2, y < h / 2) {
                (true, true) => [220, 20, 20, 255],
                (false, true) => [20, 200, 20, 255],
                (true, false) => [20, 20, 220, 255],
                (false, false) => [240, 240, 240, 255],
            };
            v[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&c);
        }
    }
    v
}

fn close(a: &[u8], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() < 60)
}

#[test]
fn geometry_survives_every_encoder_and_size() {
    let encoders = mkff_hwenc::h264_encoders();
    if encoders.is_empty() {
        eprintln!("no hardware H.264 encoder; skipped");
        return;
    }
    for info in &encoders {
        for (w, h, rate) in [
            (640usize, 360usize, RateControl::Cbr { kbps: 8000 }),
            (1280, 720, RateControl::Cbr { kbps: 8000 }),
            (1920, 1080, RateControl::Cbr { kbps: 8000 }),
            (854, 480, RateControl::Cbr { kbps: 8000 }),
            (640, 360, RateControl::Quality(65)),
            (1280, 720, RateControl::Vbr { kbps: 4000, max_kbps: 8000 }),
        ] {
            let mut cfg = HwEncoderConfig::cbr(w as u32, h as u32, 30, 1, 8000);
            cfg.rate = rate;
            let mut enc = HwVideoEncoder::new(Some(info.vendor), cfg).expect("open");
            let pic = quadrants(w, h);
            let mut packets = Vec::new();
            for i in 0..20 {
                packets.extend(enc.encode_rgba(&pic, i).expect("encode"));
            }
            packets.extend(enc.flush().expect("flush"));
            let path = std::env::temp_dir().join(format!("mkff-hwenc-geo-{:?}-{w}x{h}-{}.mp4", info.vendor, std::process::id()));
            let mut out = MediaWriter::create(&path, Container::Mp4, false).unwrap();
            let v = out.add_h264_track(enc.avcc().unwrap(), w as u16, h as u16, enc.timescale()).unwrap();
            for p in &packets {
                out.write(v, p).unwrap();
            }
            out.finish().unwrap();
            let clip = MediaFile::open(&path).unwrap();
            let rgba = clip.video_frame(10).unwrap().to_rgba8();
            let px = |x: usize, y: usize| &rgba[(y * w + x) * 4..(y * w + x) * 4 + 3];
            let probes = [
                ("top-left", px(4, 4), [220, 20, 20]),
                ("top-right", px(w - 5, 4), [20, 200, 20]),
                ("bottom-left", px(4, h - 5), [20, 20, 220]),
                ("bottom-right", px(w - 5, h - 5), [240, 240, 240]),
                ("centre-left-top", px(w / 4, h / 4), [220, 20, 20]),
            ];
            let bad: Vec<String> = probes.iter().filter(|(_, got, want)| !close(got, *want)).map(|(n, got, want)| format!("{n} {got:?} want {want:?}")).collect();
            let keys = packets.iter().filter(|p| p.keyframe).count();
            let reorder = packets.iter().any(|p| p.pts != p.dts);
            eprintln!("{} {w}x{h} {rate:?} ({} pkts, {keys} key, reorder {reorder}): {}", info.vendor.api_name(), packets.len(), if bad.is_empty() { "ok".to_string() } else { bad.join("; ") });
            let _ = std::fs::remove_file(&path);
            assert!(bad.is_empty(), "{} {w}x{h} {rate:?}: {}", info.name, bad.join("; "));
        }
    }
}
