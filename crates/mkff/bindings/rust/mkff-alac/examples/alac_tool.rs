//! Development tool: encode raw PCM to ALAC in a CAF file (for checking with other decoders),
//! decode an ALAC `.m4a` to raw PCM, and time both.
//!
//! ```text
//! alac_tool encode <in.raw> <out.caf> <rate> <channels> <bits>
//!     in.raw: interleaved s16le for 16-bit, else s32le with samples left-justified
//! alac_tool decode <in.m4a> <out.raw>
//!     writes the same layout
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::Instant;

use mkff_alac::{AlacDecoder, AlacEncoder, AlacEncoderConfig};
use mkff_audio_core::Encoder;

type R = Result<(), Box<dyn std::error::Error>>;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let r = match a.first().map(String::as_str) {
        Some("encode") if a.len() >= 6 => encode(&a[1..]),
        Some("decode") if a.len() >= 3 => decode(&a[1], &a[2]),
        _ => Err("usage: alac_tool encode <in.raw> <out.caf> <rate> <channels> <bits> | decode <in.m4a> <out.raw>".into()),
    };
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn encode(a: &[String]) -> R {
    let raw = std::fs::read(&a[0])?;
    let rate: u32 = a[2].parse()?;
    let ch: usize = a[3].parse()?;
    let bits: u32 = a[4].parse()?;
    let samples: Vec<i32> = if bits == 16 {
        raw.as_chunks::<2>()
            .0
            .iter()
            .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
            .collect()
    } else {
        raw.as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> (32 - bits))
            .collect()
    };
    let planar: Vec<Vec<i32>> = (0..ch)
        .map(|c| samples.iter().skip(c).step_by(ch).copied().collect())
        .collect();
    let frames = planar[0].len();
    let mut enc = AlacEncoder::new(AlacEncoderConfig {
        sample_rate: rate,
        channels: ch,
        bit_depth: bits,
        frame_length: 4096,
    })?;
    let t = Instant::now();
    let refs: Vec<&[i32]> = planar.iter().map(Vec::as_slice).collect();
    let mut packets = enc.encode_i32(&refs)?;
    packets.extend(enc.flush()?);
    let enc_secs = t.elapsed().as_secs_f64();
    let dur = frames as f64 / f64::from(rate);
    let bytes: usize = packets.iter().map(|p| p.data.len()).sum();

    // decode back in memory: timing and a self-check
    let mut dec = AlacDecoder::new(&enc.config())?;
    let t = Instant::now();
    let mut back = vec![Vec::new(); ch];
    for p in &packets {
        let (pcm, _) = dec.decode_i32(&p.data)?;
        for (b, c) in back.iter_mut().zip(pcm) {
            b.extend_from_slice(&c);
        }
    }
    let dec_secs = t.elapsed().as_secs_f64();
    assert_eq!(back, planar, "round trip mismatch");

    // CAF: header, desc, kuki, pakt, data
    let mut caf = Vec::new();
    caf.extend_from_slice(b"caff\x00\x01\x00\x00");
    let chunk = |caf: &mut Vec<u8>, ty: &[u8; 4], body: &[u8]| {
        caf.extend_from_slice(ty);
        caf.extend_from_slice(&(body.len() as i64).to_be_bytes());
        caf.extend_from_slice(body);
    };
    let mut desc = Vec::new();
    desc.extend_from_slice(&f64::from(rate).to_be_bytes());
    desc.extend_from_slice(b"alac");
    let flags: u32 = match bits {
        16 => 1,
        20 => 2,
        24 => 3,
        _ => 4,
    };
    for v in [flags, 0, 4096, ch as u32, 0] {
        desc.extend_from_slice(&v.to_be_bytes());
    }
    chunk(&mut caf, b"desc", &desc);
    chunk(&mut caf, b"kuki", &enc.config());
    let mut pakt = Vec::new();
    pakt.extend_from_slice(&(packets.len() as i64).to_be_bytes());
    pakt.extend_from_slice(&(frames as i64).to_be_bytes());
    pakt.extend_from_slice(&0i32.to_be_bytes());
    pakt.extend_from_slice(&((packets.len() * 4096 - frames) as i32).to_be_bytes());
    for p in &packets {
        let mut v = p.data.len();
        let mut groups = vec![(v & 0x7F) as u8];
        v >>= 7;
        while v > 0 {
            groups.push(0x80 | (v & 0x7F) as u8);
            v >>= 7;
        }
        pakt.extend(groups.iter().rev());
    }
    chunk(&mut caf, b"pakt", &pakt);
    let mut data = vec![0u8; 4];
    for p in &packets {
        data.extend_from_slice(&p.data);
    }
    chunk(&mut caf, b"data", &data);
    std::fs::write(&a[1], caf)?;
    let raw_bytes = frames * ch * bits.div_ceil(8) as usize;
    println!(
        "encoded {frames} samples x {ch} ch, {bits}-bit: {bytes} bytes ({:.2}% of {raw_bytes}); encode {enc_secs:.3} s ({:.0}x real time), decode {dec_secs:.3} s ({:.0}x real time)",
        100.0 * bytes as f64 / raw_bytes as f64,
        dur / enc_secs,
        dur / dec_secs
    );
    Ok(())
}

fn decode(input: &str, output: &str) -> R {
    let track = common::read_mp4(&std::fs::read(input)?);
    let mut dec = AlacDecoder::new(&track.cookie)?;
    let cfg = *dec.config();
    let t = Instant::now();
    let mut planar = vec![Vec::new(); usize::from(cfg.num_channels)];
    for p in &track.samples {
        let (pcm, _) = dec.decode_i32(p)?;
        for (o, c) in planar.iter_mut().zip(pcm) {
            o.extend_from_slice(&c);
        }
    }
    let secs = t.elapsed().as_secs_f64();
    let n = planar[0].len();
    let bits = u32::from(cfg.bit_depth);
    let mut raw = Vec::new();
    for i in 0..n {
        for c in &planar {
            if bits == 16 {
                raw.extend_from_slice(&(c[i] as i16).to_le_bytes());
            } else {
                raw.extend_from_slice(&(c[i] << (32 - bits)).to_le_bytes());
            }
        }
    }
    std::fs::write(output, raw)?;
    println!(
        "decoded {} packets, {n} samples: {secs:.3} s, {:.0}x real time",
        track.samples.len(),
        n as f64 / f64::from(cfg.sample_rate) / secs
    );
    Ok(())
}
