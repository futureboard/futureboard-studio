//! Development tool: encode raw PCM to `.flac`, decode `.flac` to raw PCM, and time both.
//!
//! ```text
//! flac_tool encode <in.raw> <out.flac> <rate> <channels> <bps> [level] [block_size]
//!     in.raw: interleaved s16le when bps <= 16, else s32le with samples left-justified
//!     (ffmpeg's `-f s32le` layout for 24-bit audio)
//! flac_tool decode <in.flac> <out.raw>
//!     writes the same layout (s16le / left-justified s32le)
//! ```

use std::time::Instant;

use mkff_audio_core::crc::crc16;
use mkff_audio_core::Encoder;
use mkff_flac::{find_frame_sync, FlacDecoder, FlacEncoder, FlacEncoderConfig, StreamInfo};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("encode") if args.len() >= 6 => encode(&args[1..]),
        Some("decode") if args.len() >= 3 => decode(&args[1], &args[2]),
        _ => Err("usage: flac_tool encode <in.raw> <out.flac> <rate> <channels> <bps> [level] [block] | decode <in.flac> <out.raw>".into()),
    };
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

type R = Result<(), Box<dyn std::error::Error>>;

fn encode(a: &[String]) -> R {
    let raw = std::fs::read(&a[0])?;
    let rate: u32 = a[2].parse()?;
    let ch: usize = a[3].parse()?;
    let bps: u32 = a[4].parse()?;
    let level: u8 = a.get(5).map_or(Ok(5), |s| s.parse())?;
    let block: usize = a.get(6).map_or(Ok(4096), |s| s.parse())?;
    let planar = read_raw(&raw, ch, bps);
    let frames = planar[0].len();
    let cfg = FlacEncoderConfig {
        sample_rate: rate,
        channels: ch,
        bits_per_sample: bps,
        block_size: block,
        level,
    };
    let mut enc = FlacEncoder::new(cfg)?;
    let t = Instant::now();
    let refs: Vec<&[i32]> = planar.iter().map(Vec::as_slice).collect();
    let mut packets = Vec::new();
    for start in (0..frames).step_by(1 << 16) {
        let end = (start + (1 << 16)).min(frames);
        let chunk: Vec<&[i32]> = refs.iter().map(|c| &c[start..end]).collect();
        packets.extend(enc.encode_i32(&chunk)?);
    }
    packets.extend(enc.flush()?);
    let secs = t.elapsed().as_secs_f64();
    let mut file = enc.stream_info().to_stream_header();
    for p in &packets {
        file.extend_from_slice(&p.data);
    }
    std::fs::write(&a[1], &file)?;
    let dur = frames as f64 / f64::from(rate);
    println!(
        "encoded {frames} samples x {ch} ch, level {level}: {} bytes ({:.2}% of raw {}), {:.3} s, {:.0}x real time",
        file.len(),
        100.0 * file.len() as f64 / (frames * ch * bps.div_ceil(8) as usize) as f64,
        frames * ch * bps.div_ceil(8) as usize,
        secs,
        dur / secs
    );
    Ok(())
}

fn decode(input: &str, output: &str) -> R {
    let data = std::fs::read(input)?;
    let (si, frames) = split(&data)?;
    let info = StreamInfo::parse(si)?;
    let mut dec = FlacDecoder::new(si)?;
    let t = Instant::now();
    let mut planar = vec![Vec::new(); usize::from(info.channels)];
    for f in &frames {
        let (pcm, _) = dec.decode_i32(f)?;
        for (o, c) in planar.iter_mut().zip(pcm) {
            o.extend_from_slice(&c);
        }
    }
    let secs = t.elapsed().as_secs_f64();
    let n = planar[0].len();
    let dur = n as f64 / f64::from(info.sample_rate.max(1));
    let bps = u32::from(info.bits_per_sample);
    let mut raw = Vec::new();
    for i in 0..n {
        for c in &planar {
            if bps <= 16 {
                raw.extend_from_slice(&((c[i] << (16 - bps)) as i16).to_le_bytes());
            } else {
                raw.extend_from_slice(&(c[i] << (32 - bps)).to_le_bytes());
            }
        }
    }
    std::fs::write(output, raw)?;
    println!(
        "decoded {} frames, {n} samples: {secs:.3} s, {:.0}x real time",
        frames.len(),
        dur / secs
    );
    Ok(())
}

fn read_raw(raw: &[u8], ch: usize, bps: u32) -> Vec<Vec<i32>> {
    let samples: Vec<i32> = if bps <= 16 {
        raw.as_chunks::<2>()
            .0
            .iter()
            .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])) >> (16 - bps))
            .collect()
    } else {
        raw.as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> (32 - bps))
            .collect()
    };
    (0..ch)
        .map(|c| samples.iter().skip(c).step_by(ch).copied().collect())
        .collect()
}

/// STREAMINFO block and frames of a native FLAC file.
type Split<'a> = (&'a [u8], Vec<&'a [u8]>);

/// Split a native FLAC file into STREAMINFO and frames.
fn split(data: &[u8]) -> Result<Split<'_>, Box<dyn std::error::Error>> {
    if !data.starts_with(b"fLaC") {
        return Err("not a FLAC file".into());
    }
    let mut pos = 4;
    let mut si: &[u8] = &[];
    loop {
        let h = data.get(pos..pos + 4).ok_or("truncated metadata")?;
        let len = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
        if h[0] & 0x7F == 0 {
            si = data.get(pos..pos + 4 + len).ok_or("truncated STREAMINFO")?;
        }
        pos += 4 + len;
        if h[0] & 0x80 != 0 {
            break;
        }
    }
    let mut frames = Vec::new();
    while pos < data.len() {
        let mut end = data.len();
        let mut search = pos + 2;
        while search < data.len() {
            let Some(off) = find_frame_sync(&data[search..]) else {
                break;
            };
            let cand = search + off;
            if crc16(0, &data[pos..cand]) == 0 {
                end = cand;
                break;
            }
            search = cand + 1;
        }
        frames.push(&data[pos..end]);
        pos = end;
    }
    Ok((si, frames))
}
