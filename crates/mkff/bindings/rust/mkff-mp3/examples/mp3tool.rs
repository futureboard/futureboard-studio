//! Development tool: decode, compare against a reference decode, and encode.
//!
//! ```text
//! mp3tool decode  <in.mp3> <out.f32>              raw interleaved f32le
//! mp3tool compare <in.mp3> <ref.f32> [skip]       SNR against a reference f32le decode
//! mp3tool bench   <in.mp3> [repeats]              decode speed
//! mp3tool encode  <in.wav> <out.mp3> <kbps> [js|stereo|mono] [quality] [xing]
//! ```

use mkff_audio_core::{Decoder, Encoder};
use mkff_mp3::{Mp3Decoder, Mp3Encoder, Mp3EncoderConfig, StereoMode};
use std::time::Instant;

fn decode_all(data: &[u8]) -> (u32, usize, Vec<f32>) {
    let mut d = Mp3Decoder::new();
    let mut out = Vec::new();
    // feed in odd-sized chunks to exercise partial-frame buffering
    for chunk in data.chunks(1000) {
        let b = d.decode(chunk).expect("decode");
        out.extend(b.interleaved());
    }
    (d.sample_rate(), d.channels(), out)
}

fn read_f32(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).expect("read");
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Minimal WAV reader (PCM 16-bit or float 32-bit).
fn read_wav(path: &str) -> (u32, usize, Vec<f32>) {
    let b = std::fs::read(path).expect("read wav");
    let mut p = 12;
    let (mut rate, mut ch, mut fmt, mut bits) = (0u32, 0usize, 0u16, 0u16);
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let len = u32::from_le_bytes([b[p + 4], b[p + 5], b[p + 6], b[p + 7]]) as usize;
        let body = &b[p + 8..(p + 8 + len).min(b.len())];
        if id == b"fmt " {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            ch = u16::from_le_bytes([body[2], body[3]]) as usize;
            rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
            if fmt == 0xFFFE {
                fmt = u16::from_le_bytes([body[24], body[25]]);
            }
        } else if id == b"data" {
            let s: Vec<f32> = match (fmt, bits) {
                (3, 32) => body
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
                (1, 16) => body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                    .collect(),
                _ => panic!("unsupported wav format {fmt}/{bits}"),
            };
            return (rate, ch, s);
        }
        p += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("decode") => {
            let data = std::fs::read(&args[2]).expect("read");
            let (rate, ch, pcm) = decode_all(&data);
            let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
            std::fs::write(&args[3], bytes).expect("write");
            eprintln!("{rate} Hz, {ch} ch, {} frames", pcm.len() / ch.max(1));
        }
        Some("compare") => {
            let data = std::fs::read(&args[2]).expect("read");
            let (rate, ch, pcm) = decode_all(&data);
            let reference = read_f32(&args[3]);
            let skip: usize = args.get(4).map_or(0, |s| s.parse().unwrap());
            let pcm = &pcm[(skip * ch).min(pcm.len())..];
            let n = pcm.len().min(reference.len());
            let (mut sig, mut err, mut maxe) = (0f64, 0f64, 0f64);
            for i in 0..n {
                let r = f64::from(reference[i]);
                let e = f64::from(pcm[i]) - r;
                sig += r * r;
                err += e * e;
                maxe = maxe.max(e.abs());
            }
            println!(
                "{rate} Hz {ch} ch: ours {} ref {} samples, SNR {:.1} dB, max err {:.2e}",
                pcm.len() / ch.max(1),
                reference.len() / ch.max(1),
                10.0 * (sig / err.max(1e-30)).log10(),
                maxe
            );
        }
        Some("snr") => {
            // snr <decoded.raw> <original.raw> <channels> <skip>: SNR of decoded[skip..] vs original
            let a = read_f32(&args[2]);
            let b = read_f32(&args[3]);
            let ch: usize = args[4].parse().unwrap();
            let skip: usize = args[5].parse().unwrap();
            let a = &a[(skip * ch).min(a.len())..];
            let n = a.len().min(b.len());
            let (mut sig, mut err) = (0f64, 0f64);
            for i in 0..n {
                let (x, y) = (f64::from(a[i]), f64::from(b[i]));
                sig += y * y;
                err += (x - y) * (x - y);
            }
            println!(
                "SNR {:.2} dB over {} frames",
                10.0 * (sig / err.max(1e-30)).log10(),
                n / ch
            );
        }
        Some("spectrum") => {
            // spectrum <decoded.raw> <original.raw> <channels> <skip> <rate>: averaged power of
            // the original and of the coding error per third-octave band (channel 0)
            use mkff_audio_core::fft::{Complex, Fft};
            let a = read_f32(&args[2]);
            let b = read_f32(&args[3]);
            let ch: usize = args[4].parse().unwrap();
            let skip: usize = args[5].parse().unwrap();
            let rate: f64 = args[6].parse().unwrap();
            let n = 2048;
            let frames = (b.len() / ch).min(a.len() / ch - skip);
            let mut fft = Fft::new(n);
            let mut ps = vec![0f64; n / 2];
            let mut pe = vec![0f64; n / 2];
            let mut buf = vec![Complex::ZERO; n];
            let mut ebuf = vec![Complex::ZERO; n];
            let mut cnt = 0;
            for start in (0..frames.saturating_sub(n)).step_by(n / 2) {
                for i in 0..n {
                    let w = (std::f64::consts::PI * (i as f64 + 0.5) / n as f64)
                        .sin()
                        .powi(2) as f32;
                    let x = b[(start + i) * ch];
                    let y = a[(start + i + skip) * ch];
                    buf[i] = Complex::new(x * w, 0.0);
                    ebuf[i] = Complex::new((y - x) * w, 0.0);
                }
                fft.forward(&mut buf);
                fft.forward(&mut ebuf);
                for k in 0..n / 2 {
                    ps[k] += f64::from(buf[k].norm_sqr());
                    pe[k] += f64::from(ebuf[k].norm_sqr());
                }
                cnt += 1;
            }
            let mut f = 50.0f64;
            println!("   band Hz     signal dB   error dB   SNR dB");
            while f < rate / 2.0 {
                let f2 = f * 2f64.powf(1.0 / 3.0);
                let k1 = (f / rate * n as f64) as usize;
                let k2 = ((f2 / rate * n as f64) as usize).min(n / 2).max(k1 + 1);
                let s: f64 = ps[k1..k2].iter().sum::<f64>() / cnt as f64;
                let e: f64 = pe[k1..k2].iter().sum::<f64>() / cnt as f64;
                println!(
                    "{:7.0}-{:7.0} {:9.1} {:9.1} {:8.1}",
                    f,
                    f2,
                    10.0 * (s + 1e-20).log10(),
                    10.0 * (e + 1e-20).log10(),
                    10.0 * ((s + 1e-20) / (e + 1e-20)).log10()
                );
                f = f2;
            }
        }
        Some("lag") => {
            // lag <decoded.raw> <original.raw> <channels> <maxlag>: best alignment (channel 0)
            let a = read_f32(&args[2]);
            let b = read_f32(&args[3]);
            let ch: usize = args[4].parse().unwrap();
            let maxlag: usize = args[5].parse().unwrap();
            let n = (b.len() / ch).min(a.len() / ch - maxlag).min(44100);
            let mut best = (0f64, 0usize);
            for lag in 0..maxlag {
                let mut c = 0f64;
                for i in 0..n {
                    c += f64::from(a[(i + lag) * ch]) * f64::from(b[i * ch]);
                }
                if c.abs() > best.0.abs() {
                    best = (c, lag);
                }
            }
            println!("best lag {} (correlation {:.3e})", best.1, best.0);
        }
        Some("diff") => {
            // per-block error between two raw f32 files: diff <a> <b> <channels> [block] [skip_a] [from] [to]
            let a = read_f32(&args[2]);
            let b = read_f32(&args[3]);
            let ch: usize = args[4].parse().unwrap();
            let blk: usize = args.get(5).map_or(576, |s| s.parse().unwrap());
            let skip: usize = args.get(6).map_or(0, |s| s.parse().unwrap());
            let a = a[(skip * ch).min(a.len())..].to_vec();
            let from: usize = args.get(7).map_or(0, |s| s.parse().unwrap());
            let n = (a.len().min(b.len()) / ch)
                .min(args.get(8).map_or(usize::MAX, |s| s.parse().unwrap()));
            for s in (from..n).step_by(blk) {
                let mut line = format!("{s:7}");
                for c in 0..ch {
                    let (mut sig, mut err) = (0f64, 0f64);
                    for i in s..(s + blk).min(n) {
                        let (x, y) = (f64::from(a[i * ch + c]), f64::from(b[i * ch + c]));
                        sig += y * y;
                        err += (x - y) * (x - y);
                    }
                    line += &format!(
                        "  ch{c} sig {:9.3e} snr {:6.1}",
                        sig,
                        10.0 * (sig.max(1e-30) / err.max(1e-30)).log10()
                    );
                }
                println!("{line}");
            }
        }
        Some("setis") => {
            // turn on the intensity stereo flag of every joint stereo frame (fixture making)
            let mut data = std::fs::read(&args[2]).expect("read");
            let mut p = mkff_mp3::find_sync(&data).unwrap_or(0);
            let mut n = 0;
            while p + 4 <= data.len() {
                let Some(h) = mkff_mp3::parse_header(&data[p..]) else {
                    break;
                };
                if h.channel_mode == mkff_mp3::ChannelMode::JointStereo {
                    data[p + 3] |= 0x10;
                    n += 1;
                }
                p += h.frame_len_bytes.max(1);
            }
            std::fs::write(&args[3], &data).expect("write");
            eprintln!("{n} frames flagged");
        }
        Some("bench") => {
            let data = std::fs::read(&args[2]).expect("read");
            let reps: usize = args.get(3).map_or(20, |s| s.parse().unwrap());
            let (rate, ch, pcm) = decode_all(&data);
            let secs = (pcm.len() / ch) as f64 / f64::from(rate);
            let t = Instant::now();
            for _ in 0..reps {
                let mut d = Mp3Decoder::new();
                let b = d.decode(&data).unwrap();
                std::hint::black_box(&b);
            }
            let el = t.elapsed().as_secs_f64();
            println!(
                "{:.1}x real time ({:.3} s audio decoded in {:.4} s per pass)",
                secs * reps as f64 / el,
                secs,
                el / reps as f64
            );
        }
        Some("encode") => {
            let (rate, ch, pcm) = read_wav(&args[2]);
            let kbps: u32 = args[4].parse().unwrap();
            let mode = match args.get(5).map(String::as_str) {
                Some("stereo") => StereoMode::Stereo,
                Some("mono") => StereoMode::Mono,
                _ if ch == 1 => StereoMode::Mono,
                _ => StereoMode::JointStereo,
            };
            let quality: u8 = args.get(6).map_or(3, |s| s.parse().unwrap());
            let cfg = Mp3EncoderConfig {
                sample_rate: rate,
                channels: ch,
                bitrate_kbps: kbps,
                mode,
                quality,
            };
            let mut e = Mp3Encoder::new(cfg).expect("encoder");
            let frames = pcm.len() / ch;
            let planar: Vec<Vec<f32>> = (0..ch)
                .map(|c| (0..frames).map(|i| pcm[i * ch + c]).collect())
                .collect();
            let t = Instant::now();
            let mut out = Vec::new();
            for start in (0..frames).step_by(4096) {
                let end = (start + 4096).min(frames);
                let slices: Vec<&[f32]> = planar.iter().map(|c| &c[start..end]).collect();
                for p in e.encode(&slices).unwrap() {
                    out.extend_from_slice(&p.data);
                }
            }
            for p in e.flush().unwrap() {
                out.extend_from_slice(&p.data);
            }
            let el = t.elapsed().as_secs_f64();
            if args.get(7).map(String::as_str) == Some("xing") {
                let mut f = e.xing_frame();
                f.extend_from_slice(&out);
                out = f;
            }
            std::fs::write(&args[3], &out).expect("write");
            let secs = frames as f64 / f64::from(rate);
            println!(
                "{} bytes, {:.2} kbit/s, priming {}, {:.1}x real time, {} frames ({} M/S), {} short granule-channels",
                out.len(),
                out.len() as f64 * 8.0 / secs / 1000.0,
                e.priming(),
                secs / el,
                e.stats().frames,
                e.stats().ms_frames,
                e.stats().short_blocks
            );
        }
        _ => eprintln!("usage: see the source"),
    }
}
