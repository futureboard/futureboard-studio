//! `cargo test -p mkff-audio-format --test dump -- --nocapture --ignored` prints what the
//! demuxers see in every fixture (a development aid; compare with ffprobe).

mod common;

use common::*;

#[test]
#[ignore]
fn dump_all() {
    let mut files: Vec<_> = std::fs::read_dir(root().join("audio/format"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    for n in [
        "tone.mp3",
        "tone.aac",
        "tone.flac",
        "tone.opus",
        "tone_alac.m4a",
        "tone51_ac3.mp4",
        "tone51_eac3.mp4",
        "av1_aac.mp4",
        "prores_hq.mov",
    ] {
        files.push(clip(n));
    }
    files.sort();
    for f in files {
        if f.extension().is_some_and(|e| e == "md") {
            continue;
        }
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        match mkff_audio_format::open_path(&f) {
            Err(e) => println!("{name}: ERROR {e}"),
            Ok(mut d) => {
                let tracks = d.tracks().to_vec();
                let pk = read_all(d.as_mut());
                for (i, t) in tracks.iter().enumerate() {
                    let (n, dur, first) = stats(&pk, i);
                    println!(
                        "{name} [{}] #{i} {} {}Hz ch{} tb{} dur={:?} prim={} pad={} cfg={} | pkts={n} sum={dur} first_pts={first}",
                        d.format_name(),
                        t.name,
                        t.sample_rate,
                        t.channels,
                        t.time_base,
                        t.duration,
                        t.priming,
                        t.padding,
                        t.config.len()
                    );
                }
            }
        }
    }
}

/// `MKFF_BENCH_FILES=a.mp3;b.m4a cargo test --release --test dump -- --ignored --nocapture bench`
/// times open / full read / 50 seeks on large local files.
#[test]
#[ignore]
fn bench_files_from_env() {
    let Ok(list) = std::env::var("MKFF_BENCH_FILES") else {
        return;
    };
    for f in list.split(';').filter(|s| !s.is_empty()) {
        let t0 = std::time::Instant::now();
        let mut d = mkff_audio_format::open_path(f).unwrap();
        let t_open = t0.elapsed();
        let tr = d.tracks()[0].clone();
        let t1 = std::time::Instant::now();
        let pk = read_all(d.as_mut());
        let t_read = t1.elapsed();
        let end = pk
            .iter()
            .filter(|p| p.track == 0)
            .map(|p| p.pts + i64::from(p.duration))
            .max()
            .unwrap_or(0);
        let t2 = std::time::Instant::now();
        let mut worst = 0i64;
        for k in 0..50 {
            let target = end * ((k * 37) % 50) / 50;
            let got = d.seek(0, target).unwrap();
            let p = d.next_packet().unwrap();
            if let Some(p) = p {
                assert_eq!(p.pts, got);
            }
            worst = worst.max(target - got);
        }
        println!(
            "{f}: {} {:?} dur={:?} pkts={} open={t_open:?} read={t_read:?} 50 seeks={:?} worst lag={}",
            tr.name,
            d.format_name(),
            tr.duration,
            pk.len(),
            t2.elapsed(),
            worst
        );
    }
}
