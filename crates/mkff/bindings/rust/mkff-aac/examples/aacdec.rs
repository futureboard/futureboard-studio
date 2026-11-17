//! Decode an ADTS / LOAS file to raw interleaved f32le: `aacdec in.aac out.f32 [asc-hex raw-au-file]`.
//!
//! Development helper: compares against reference decodes and measures speed.

use mkff_aac::{loas_frame_length, parse_adts, AacDecoder};
use mkff_audio_core::Decoder;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read input");
    let mut dec = AacDecoder::new(&[]).expect("decoder");
    let mut out = Vec::new();
    let mut pos = 0;
    let t = std::time::Instant::now();
    let mut frames = 0usize;
    let mut samples = 0usize;
    while pos < data.len() {
        let len = parse_adts(&data[pos..])
            .map(|h| h.frame_length)
            .or_else(|| loas_frame_length(&data[pos..]))
            .unwrap_or(data.len() - pos);
        let buf = dec
            .decode(&data[pos..pos + len.min(data.len() - pos)])
            .expect("decode");
        samples += buf.frames();
        out.extend(buf.interleaved());
        pos += len;
        frames += 1;
    }
    let el = t.elapsed().as_secs_f64();
    eprintln!("{:?}", dec.stats());
    let rate = dec.sample_rate();
    eprintln!(
        "{frames} frames, {} ch, {rate} Hz, {samples} samples, {:.3} ms, {:.0}x real time",
        dec.channels(),
        el * 1e3,
        samples as f64 / rate.max(1) as f64 / el
    );
    let mut f = std::fs::File::create(&args[2]).expect("create output");
    let bytes: Vec<u8> = out.iter().flat_map(|v| v.to_le_bytes()).collect();
    f.write_all(&bytes).unwrap();
}
