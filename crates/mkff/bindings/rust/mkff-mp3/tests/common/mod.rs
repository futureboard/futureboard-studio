//! Shared test helpers.
#![allow(dead_code)]

use mkff_audio_core::{AudioBuffer, Decoder};
use mkff_mp3::{parse_header, Mp3Decoder};

pub fn fixture_path(name: &str) -> String {
    format!(
        "{}/../../../testdata/audio/mp3/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Interleaved f32le reference window.
pub fn reference(name: &str) -> Vec<f32> {
    fixture(name)
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Split a stream into frames by walking the headers from the first sync.
pub fn split_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut p = mkff_mp3::find_sync(data).unwrap_or(0);
    while p + 4 <= data.len() {
        let Some(h) = parse_header(&data[p..]) else {
            break;
        };
        let len = h.frame_len_bytes;
        if len == 0 || p + len > data.len() {
            break;
        }
        out.push(&data[p..p + len]);
        p += len;
    }
    out
}

/// Decode a whole stream fed in chunks of `chunk` bytes (0 = one frame per packet).
pub fn decode(data: &[u8], chunk: usize) -> AudioBuffer {
    let mut d = Mp3Decoder::new();
    let mut out = AudioBuffer::default();
    if chunk == 0 {
        for f in split_frames(data) {
            out.append(&d.decode(f).expect("decode"));
        }
    } else {
        for c in data.chunks(chunk) {
            out.append(&d.decode(c).expect("decode"));
        }
    }
    out.sample_rate = d.sample_rate();
    out
}

/// SNR in dB of interleaved `got` against `want`.
pub fn snr_db(got: &[f32], want: &[f32]) -> f64 {
    let n = got.len().min(want.len());
    assert!(n > 0, "nothing to compare");
    let (mut s, mut e) = (0f64, 0f64);
    for i in 0..n {
        let (g, w) = (f64::from(got[i]), f64::from(want[i]));
        s += w * w;
        e += (g - w) * (g - w);
    }
    10.0 * (s / e.max(1e-30)).log10()
}

/// Interleaved frames `start..start + len` of a buffer.
pub fn window(buf: &AudioBuffer, start: usize, len: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(len * buf.channels.len());
    for i in start..start + len {
        for c in &buf.channels {
            out.push(c.get(i).copied().unwrap_or(0.0));
        }
    }
    out
}

/// Small deterministic PRNG (xorshift64*).
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [-1, 1).
    pub fn uniform(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// A music-like test signal: a vibrato chord, a slow sweep, decaying noise bursts every 0.5 s.
pub fn test_signal(rate: u32, channels: usize, secs: f32) -> Vec<Vec<f32>> {
    let n = (rate as f32 * secs) as usize;
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut out = vec![vec![0f32; n]; channels];
    let tau = std::f32::consts::TAU;
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let burst = (-40.0 * (t % 0.5)).exp() * 0.25;
        for (c, ch) in out.iter_mut().enumerate() {
            let detune = 1.0 + 0.003 * c as f32;
            let v = 0.18 * (tau * 261.6 * detune * t + 0.3 * (tau * 5.0 * t).sin()).sin()
                + 0.12 * (tau * 329.6 * t + c as f32).sin()
                + 0.08 * (tau * (392.0 + 100.0 * c as f32) * t).sin()
                + 0.06 * (tau * (150.0 * t + 300.0 * t * t)).sin()
                + burst * rng.uniform();
            ch[i] = v;
        }
    }
    out
}
