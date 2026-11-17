//! Test-only helpers: a minimal Layer I encoder used to make the Layer I fixtures (FFmpeg has
//! no Layer I encoder; its decoder provides the reference).

use crate::synth::Analysis;
use mkff_audio_core::bits::BitWriter;

fn sf_table() -> [f32; 63] {
    let mut t = [0f32; 63];
    for (i, v) in t.iter_mut().enumerate() {
        *v = 2f64.powf(1.0 - i as f64 / 3.0) as f32;
    }
    t
}

/// Encode planar input as Layer I at 32 or 48 kHz (no padding needed at these rates).
/// `joint_bound_ext` = Some(mode_extension) writes joint stereo.
pub(crate) fn encode_layer1(
    input: &[Vec<f32>],
    rate: u32,
    kbps: u32,
    joint: Option<u8>,
) -> Vec<u8> {
    let nch = input.len();
    let n = input[0].len();
    let mut fb: Vec<Analysis> = (0..nch).map(|_| Analysis::default()).collect();
    let sft = sf_table();
    let frame_bytes = (12 * kbps as usize * 1000 / rate as usize) * 4;
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < n + 481 + 384 {
        // subband samples [ch][s][sb]
        let mut sb = vec![[[0f32; 32]; 12]; nch];
        for (c, f) in fb.iter_mut().enumerate() {
            for (s, slot) in sb[c].iter_mut().enumerate() {
                let mut buf = [0f32; 32];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = input[c].get(pos + s * 32 + i).copied().unwrap_or(0.0);
                }
                f.process(&buf, slot);
            }
        }
        pos += 384;
        let bound = match joint {
            Some(e) if nch == 2 => (usize::from(e) + 1) * 4,
            _ => 32,
        };
        // scale factor index per [ch][sb]
        let mut sfi = vec![[0usize; 32]; nch];
        let mut energy = vec![[0f32; 32]; nch];
        for c in 0..nch {
            for k in 0..32 {
                let m = (0..12).map(|s| sb[c][s][k].abs()).fold(0f32, f32::max);
                sfi[c][k] = (0..63).rev().find(|&i| sft[i] >= m).unwrap_or(0);
                energy[c][k] = (0..12).map(|s| sb[c][s][k] * sb[c][s][k]).sum::<f32>();
            }
        }
        // greedy allocation by energy / noise
        let mut alloc = vec![[0u32; 32]; nch];
        let header_bits = 32 + 4 * (bound * nch + (32 - bound));
        let mut avail = (frame_bytes * 8) as i64 - header_bits as i64;
        loop {
            let mut best = None;
            let mut best_v = 0f32;
            for k in 0..32 {
                let chans: Vec<usize> = if k < bound {
                    (0..nch).collect()
                } else {
                    vec![0]
                };
                for &c in &chans {
                    let a = alloc[c][k];
                    if a >= 14 || energy[c][k] < 1e-9 {
                        continue;
                    }
                    let shared = if k >= bound { nch } else { 1 };
                    let cost = if a == 0 {
                        12 + 6 * shared as i64 + 12
                    } else {
                        12
                    } as i64;
                    if cost > avail {
                        continue;
                    }
                    let v = energy[c][k] / 4f32.powi(a as i32);
                    if v > best_v {
                        best_v = v;
                        best = Some((c, k, cost));
                    }
                }
            }
            let Some((c, k, cost)) = best else { break };
            alloc[c][k] += if alloc[c][k] == 0 { 2 } else { 1 };
            avail -= cost;
        }
        // alloc holds bits per sample (0 or 2..=15); code = bits - 1
        let mut w = BitWriter::with_capacity(frame_bytes);
        let br_idx = [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ]
        .iter()
        .position(|&b| b == kbps)
        .expect("bit rate") as u32;
        let sr_idx = match rate {
            44100 => 0,
            48000 => 1,
            _ => 2,
        };
        let mode = match (nch, joint) {
            (1, _) => 3,
            (_, Some(_)) => 1,
            _ => 0,
        };
        w.write(0xFFF, 12);
        w.write(0b1111, 4); // MPEG-1, layer I, no CRC
        w.write(br_idx, 4);
        w.write(sr_idx, 2);
        w.write(0, 2); // padding, private
        w.write(mode, 2);
        w.write(u32::from(joint.unwrap_or(0)), 2);
        w.write(0, 4);
        let code = |b: u32| if b == 0 { 0 } else { b - 1 };
        for k in 0..32 {
            if k < bound {
                for a in alloc.iter().take(nch) {
                    w.write(code(a[k]), 4);
                }
            } else {
                w.write(code(alloc[0][k]), 4);
            }
        }
        for k in 0..32 {
            for c in 0..nch {
                let a = if k < bound { alloc[c][k] } else { alloc[0][k] };
                if a != 0 {
                    w.write(sfi[c][k] as u32, 6);
                }
            }
        }
        for s in 0..12 {
            for k in 0..32 {
                let chans = if k < bound { nch } else { 1 };
                for c in 0..chans {
                    let a = if k < bound { alloc[c][k] } else { alloc[0][k] };
                    if a == 0 {
                        continue;
                    }
                    let levels = (1u32 << a) - 1;
                    let x = if k >= bound && nch == 2 {
                        (sb[0][s][k] / sft[sfi[0][k]] + sb[1][s][k] / sft[sfi[1][k]]) * 0.5
                    } else {
                        sb[c][s][k] / sft[sfi[c][k]]
                    };
                    let v = ((x * levels as f32 + levels as f32 - 1.0) * 0.5).round();
                    let v = v.clamp(0.0, (levels - 1) as f32) as u32;
                    w.write(v, a);
                }
            }
        }
        let mut bytes = w.finish();
        assert!(bytes.len() <= frame_bytes, "layer I frame overflow");
        bytes.resize(frame_bytes, 0);
        out.extend_from_slice(&bytes);
    }
    out
}

/// Read a WAV file (f32 or s16) as planar samples.
pub(crate) fn read_wav(path: &str) -> (u32, Vec<Vec<f32>>) {
    let b = std::fs::read(path).expect("read wav");
    let mut p = 12;
    let (mut rate, mut ch, mut fmt, mut bits) = (0u32, 0usize, 0u16, 0u16);
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let len = u32::from_le_bytes([b[p + 4], b[p + 5], b[p + 6], b[p + 7]]) as usize;
        let body = &b[p + 8..(p + 8 + len).min(b.len())];
        if id == b"fmt " {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            ch = usize::from(u16::from_le_bytes([body[2], body[3]]));
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
                _ => body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                    .collect(),
            };
            let frames = s.len() / ch;
            let planar = (0..ch)
                .map(|c| (0..frames).map(|i| s[i * ch + c]).collect())
                .collect();
            return (rate, planar);
        }
        p += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

/// `MKFF_MP3_L1_SRC=<dir with src_48000.wav and src_32000.wav> cargo test -p mkff-mp3 --release
/// make_layer1_fixtures -- --ignored` writes the Layer I fixtures.
#[test]
#[ignore]
fn make_layer1_fixtures() {
    let dir = std::env::var("MKFF_MP3_L1_SRC").expect("MKFF_MP3_L1_SRC");
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../testdata/audio/mp3");
    let (rate, x) = read_wav(&format!("{dir}/src_48000.wav"));
    assert_eq!(rate, 48000);
    std::fs::write(
        format!("{out}/l1_48k_js_384.mp1"),
        encode_layer1(&x, 48000, 384, Some(1)),
    )
    .unwrap();
    let (rate, x) = read_wav(&format!("{dir}/src_32000.wav"));
    assert_eq!(rate, 32000);
    let mono = vec![x[0].clone()];
    std::fs::write(
        format!("{out}/l1_32k_mono_192.mp1"),
        encode_layer1(&mono, 32000, 192, None),
    )
    .unwrap();
}
