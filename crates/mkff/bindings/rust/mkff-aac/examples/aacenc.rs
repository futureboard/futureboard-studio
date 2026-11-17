//! Encode raw interleaved f32le to ADTS: `aacenc in.f32 out.aac rate channels bitrate [tns pns]`.
//!
//! Development helper for checking the encoder output with other decoders.

use mkff_aac::{AacEncoder, AacEncoderConfig};
use mkff_audio_core::Encoder;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).expect("read input");
    let rate: u32 = a[3].parse().unwrap();
    let ch: usize = a[4].parse().unwrap();
    let br: u32 = a[5].parse().unwrap();
    let mut cfg = AacEncoderConfig::new(rate, ch, br);
    if let Some(v) = a.get(6) {
        cfg.tns = v == "1";
    }
    if let Some(v) = a.get(7) {
        cfg.pns = v == "1";
    }
    let mut enc = AacEncoder::new(cfg).expect("encoder");
    let samples: Vec<f32> = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let frames = samples.len() / ch;
    let planar: Vec<Vec<f32>> = (0..ch)
        .map(|c| (0..frames).map(|i| samples[i * ch + c]).collect())
        .collect();
    let t = std::time::Instant::now();
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for start in (0..frames).step_by(4096) {
        let end = (start + 4096).min(frames);
        let refs: Vec<&[f32]> = planar.iter().map(|c| &c[start..end]).collect();
        for p in enc.encode(&refs).unwrap() {
            bytes += p.data.len();
            out.extend(enc.adts_frame(&p.data));
        }
    }
    for p in enc.flush().unwrap() {
        bytes += p.data.len();
        out.extend(enc.adts_frame(&p.data));
    }
    let el = t.elapsed().as_secs_f64();
    let dur = frames as f64 / f64::from(rate);
    eprintln!(
        "{frames} samples, {:.1} kbps (target {}), {:.0}x real time",
        bytes as f64 * 8.0 / dur / 1000.0,
        br / 1000,
        dur / el
    );
    std::fs::write(&a[2], out).unwrap();
}
