//! Development helper: decode a raw AC-3 / E-AC-3 file, optionally write interleaved f32le,
//! and compare against a reference decode (interleaved f32le, e.g. from
//! `ffmpeg -drc_scale 0 -i in.ac3 -f f32le ref.f32`).
//!
//! `cargo run --release --example ac3_compare -- in.ac3 [--ref ref.f32] [--out out.f32]
//!  [--nodither] [--stereo|--mono]`

use std::time::Instant;

use mkff_ac3::{find_sync, parse_frame, Ac3Decoder, Downmix, StreamType};
use mkff_audio_core::Decoder;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let input = std::fs::read(&args[0]).expect("read input");
    let mut reference = None;
    let mut out_path = None;
    let mut dec = Ac3Decoder::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ref" => {
                i += 1;
                let b = std::fs::read(&args[i]).expect("read ref");
                reference = Some(
                    b.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes(*c))
                        .collect::<Vec<f32>>(),
                );
            }
            "--out" => {
                i += 1;
                out_path = Some(args[i].clone());
            }
            "--nodither" => dec.set_dither(false),
            "--stereo" => dec.set_downmix(Some(Downmix::Stereo)),
            "--mono" => dec.set_downmix(Some(Downmix::Mono)),
            a => panic!("unknown argument {a}"),
        }
        i += 1;
    }
    // split into packets: an independent frame plus its dependent frames
    let mut packets = Vec::new();
    let mut pos = find_sync(&input).expect("no sync");
    while pos < input.len() {
        let Some(info) = parse_frame(&input[pos..]) else {
            break;
        };
        let end = (pos + info.frame_size).min(input.len());
        let dependent =
            info.stream_type == StreamType::Dependent || (info.is_eac3() && info.substream_id != 0);
        if dependent && !packets.is_empty() {
            let last: &mut (usize, usize) = packets.last_mut().unwrap();
            last.1 = end;
        } else {
            packets.push((pos, end));
        }
        pos = end;
    }
    let mut all: Vec<Vec<f32>> = Vec::new();
    let mut frames = 0usize;
    let mut el = 0f64;
    for &(a, b) in &packets {
        let t = Instant::now();
        let buf = dec.decode(&input[a..b]).expect("decode");
        el += t.elapsed().as_secs_f64();
        if all.is_empty() {
            all = vec![Vec::new(); buf.channels.len()];
        }
        for (d, s) in all.iter_mut().zip(&buf.channels) {
            d.extend_from_slice(s);
        }
        frames += 1;
    }
    let n = all.first().map_or(0, Vec::len);
    let sr = dec.sample_rate();
    let info = dec.last_frame_info().cloned();
    println!(
        "{} packets, {} ch {:?}, {} Hz, {} samples, {:.1} ms, {:.0}x realtime, bsid {:?}",
        frames,
        all.len(),
        dec.layout().iter().map(|c| c.name()).collect::<Vec<_>>(),
        sr,
        n,
        el * 1e3,
        n as f64 / sr as f64 / el,
        info.map(|i| i.bsid)
    );
    let nch = all.len();
    if let Some(path) = out_path {
        let mut bytes = Vec::with_capacity(n * nch * 4);
        for i in 0..n {
            for c in &all {
                bytes.extend_from_slice(&c[i].to_le_bytes());
            }
        }
        std::fs::write(path, bytes).unwrap();
    }
    if let Some(r) = reference {
        let rn = r.len() / nch;
        let m = rn.min(n);
        println!("reference {rn} samples, comparing {m}");
        let mut ts = 0f64;
        let mut tn = 0f64;
        for (c, ch) in all.iter().enumerate() {
            let mut s = 0f64;
            let mut e = 0f64;
            let mut maxe = 0f32;
            for i in 0..m {
                let rv = r[i * nch + c];
                s += f64::from(rv) * f64::from(rv);
                let d = rv - ch[i];
                e += f64::from(d) * f64::from(d);
                maxe = maxe.max(d.abs());
            }
            ts += s;
            tn += e;
            println!(
                "  ch{c}: SNR {:.1} dB (signal rms {:.4}, max err {:.2e})",
                10.0 * (s / e.max(1e-30)).log10(),
                (s / m as f64).sqrt(),
                maxe
            );
        }
        println!("  total SNR {:.1} dB", 10.0 * (ts / tn.max(1e-30)).log10());
    }
}
