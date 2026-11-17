//! Test helpers: a minimal Ogg page reader and fixture access.

#![allow(dead_code)]

use std::path::PathBuf;

/// The packets of the first logical stream of an Ogg file, with each packet's granule position
/// (the granule of the page on which it ends, -1 if another packet ends later on that page).
pub struct OggStream {
    pub packets: Vec<Vec<u8>>,
    pub granules: Vec<i64>,
    /// Granule position of the last page.
    pub last_granule: i64,
}

pub fn read_ogg(data: &[u8]) -> OggStream {
    let mut packets = Vec::new();
    let mut granules = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut serial = None;
    let mut last_granule = -1;
    let mut pos = 0;
    while pos + 27 <= data.len() {
        assert_eq!(&data[pos..pos + 4], b"OggS", "lost Ogg sync at {pos}");
        let granule = i64::from_le_bytes(data[pos + 6..pos + 14].try_into().unwrap());
        let this_serial = u32::from_le_bytes(data[pos + 14..pos + 18].try_into().unwrap());
        let nseg = usize::from(data[pos + 26]);
        let table = &data[pos + 27..pos + 27 + nseg];
        let mut body = pos + 27 + nseg;
        let page_end = body + table.iter().map(|&b| usize::from(b)).sum::<usize>();
        let mine = *serial.get_or_insert(this_serial) == this_serial;
        if mine {
            let first_on_page = packets.len();
            for &lace in table {
                partial.extend_from_slice(&data[body..body + usize::from(lace)]);
                body += usize::from(lace);
                if lace < 255 {
                    packets.push(std::mem::take(&mut partial));
                    granules.push(-1);
                }
            }
            if packets.len() > first_on_page {
                *granules.last_mut().unwrap() = granule;
            }
            last_granule = granule;
        }
        pos = page_end;
    }
    OggStream {
        packets,
        granules,
        last_granule,
    }
}

pub fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/audio/vorbis")
}

pub fn fixture(name: &str) -> Vec<u8> {
    let p = fixture_dir().join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub fn f32_file(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// SNR in dB of `got` against `want`.
pub fn snr_db(want: &[f32], got: &[f32]) -> f64 {
    let (mut s, mut e) = (0f64, 0f64);
    for (a, b) in want.iter().zip(got) {
        s += f64::from(*a) * f64::from(*a);
        e += (f64::from(*a) - f64::from(*b)).powi(2);
    }
    if e == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (s / e).log10()
}

/// LSB-first bit writer for crafting headers.
#[derive(Default)]
pub struct LsbWriter {
    pub bytes: Vec<u8>,
    bits: usize,
}

impl LsbWriter {
    pub fn put(&mut self, v: u64, n: u32) -> &mut Self {
        for i in 0..n {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (v >> i) & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 1 << (self.bits % 8);
            }
            self.bits += 1;
        }
        self
    }
}

/// The audio track's `CodecPrivate` and frames of a (simple, unlaced) Matroska / WebM file.
pub fn read_mkv_audio(data: &[u8], codec_id: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
    fn vint(d: &[u8], pos: &mut usize, keep_marker: bool) -> u64 {
        let first = d[*pos];
        let len = first.leading_zeros() as usize + 1;
        let mut v = if keep_marker {
            u64::from(first)
        } else {
            u64::from(first) & (0xff >> len)
        };
        for i in 1..len {
            v = (v << 8) | u64::from(d[*pos + i]);
        }
        *pos += len;
        if !keep_marker && v == (1u64 << (7 * len)) - 1 {
            return u64::MAX; // unknown size
        }
        v
    }
    struct Walk {
        track: Option<u64>,
        cur_track: u64,
        cur_codec: String,
        cur_private: Vec<u8>,
        private: Vec<u8>,
        frames: Vec<(u64, Vec<u8>)>,
    }
    fn walk(d: &[u8], mut pos: usize, end: usize, codec_id: &str, w: &mut Walk) {
        while pos < end && pos < d.len() {
            let id = vint(d, &mut pos, true);
            let size = vint(d, &mut pos, false);
            let body_end = if size == u64::MAX {
                end
            } else {
                (pos + size as usize).min(end)
            };
            match id {
                0x1853_8067 | 0x1654_AE6B | 0x1F43_B675 | 0xA0 => {
                    walk(d, pos, body_end, codec_id, w)
                }
                0xAE => {
                    w.cur_codec.clear();
                    w.cur_private.clear();
                    walk(d, pos, body_end, codec_id, w);
                    if w.cur_codec == codec_id {
                        w.track = Some(w.cur_track);
                        w.private = w.cur_private.clone();
                    }
                }
                0xD7 => {
                    w.cur_track = d[pos..body_end]
                        .iter()
                        .fold(0, |a, &b| (a << 8) | u64::from(b))
                }
                0x86 => w.cur_codec = String::from_utf8_lossy(&d[pos..body_end]).into_owned(),
                0x63A2 => w.cur_private = d[pos..body_end].to_vec(),
                0xA3 | 0xA1 => {
                    let mut p = pos;
                    let track = vint(d, &mut p, false);
                    let flags = d[p + 2];
                    assert_eq!(
                        flags & 0x06,
                        0,
                        "laced blocks not supported by the test reader"
                    );
                    w.frames.push((track, d[p + 3..body_end].to_vec()));
                }
                _ => {}
            }
            pos = body_end;
        }
    }
    let mut w = Walk {
        track: None,
        cur_track: 0,
        cur_codec: String::new(),
        cur_private: Vec::new(),
        private: Vec::new(),
        frames: Vec::new(),
    };
    walk(data, 0, data.len(), codec_id, &mut w);
    let track = w.track.expect("no audio track");
    let frames = w
        .frames
        .into_iter()
        .filter(|(t, _)| *t == track)
        .map(|(_, f)| f)
        .collect();
    (w.private, frames)
}
