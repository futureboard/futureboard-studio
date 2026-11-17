//! Test helpers: a minimal MP4 sample extractor and MD5 (RFC 1321) for comparing decodes with
//! ffmpeg's. Test-only; not a general MP4 parser.

#![allow(dead_code)]

/// The audio track of an MP4 / M4A file: the sample entry's codec boxes and the samples.
pub struct Track {
    /// Children of the sample entry (for ALAC: the `alac` box, maybe a `chan` box).
    pub cookie: Vec<u8>,
    pub samples: Vec<Vec<u8>>,
}

fn be32(d: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

/// Child boxes of `data`: (type, body).
fn boxes(mut data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let mut size = be32(data, 0) as usize;
        let mut header = 8;
        if size == 1 {
            size = u64::from_be_bytes(data[8..16].try_into().unwrap()) as usize;
            header = 16;
        } else if size == 0 {
            size = data.len();
        }
        let size = size.min(data.len());
        out.push(([data[4], data[5], data[6], data[7]], &data[header..size]));
        data = &data[size..];
    }
    out
}

fn child<'a>(data: &'a [u8], ty: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data)
        .into_iter()
        .find(|(t, _)| t == ty)
        .map(|(_, b)| b)
}

/// Extract the first track's codec config boxes and samples.
pub fn read_mp4(file: &[u8]) -> Track {
    let moov = child(file, b"moov").expect("moov");
    let trak = child(moov, b"trak").expect("trak");
    let stbl = [b"mdia", b"minf", b"stbl"]
        .iter()
        .fold(trak, |d, t| child(d, t).expect("stbl path"));
    let stsd = child(stbl, b"stsd").expect("stsd");
    // version/flags, entry count, then the first sample entry box
    let entry = boxes(&stsd[8..]).remove(0).1;
    // audio sample entry: 28 bytes of fields before the child boxes
    let cookie = entry[28..].to_vec();

    let stsz = child(stbl, b"stsz").expect("stsz");
    let fixed = be32(stsz, 4) as usize;
    let count = be32(stsz, 8) as usize;
    let sizes: Vec<usize> = (0..count)
        .map(|i| {
            if fixed != 0 {
                fixed
            } else {
                be32(stsz, 12 + 4 * i) as usize
            }
        })
        .collect();
    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..be32(stco, 4) as usize)
            .map(|i| u64::from(be32(stco, 8 + 4 * i)))
            .collect()
    } else {
        let co64 = child(stbl, b"co64").expect("stco / co64");
        (0..be32(co64, 4) as usize)
            .map(|i| u64::from_be_bytes(co64[8 + 8 * i..16 + 8 * i].try_into().unwrap()))
            .collect()
    };
    let stsc = child(stbl, b"stsc").expect("stsc");
    let runs: Vec<(usize, usize)> = (0..be32(stsc, 4) as usize)
        .map(|i| {
            (
                be32(stsc, 8 + 12 * i) as usize,
                be32(stsc, 12 + 12 * i) as usize,
            )
        })
        .collect();
    let mut samples = Vec::with_capacity(count);
    let mut s = 0;
    for (ci, &off) in offsets.iter().enumerate() {
        let chunk = ci + 1;
        let per = runs
            .iter()
            .rev()
            .find(|(first, _)| *first <= chunk)
            .map_or(0, |r| r.1);
        let mut pos = off as usize;
        for _ in 0..per {
            if s >= count {
                break;
            }
            samples.push(file[pos..pos + sizes[s]].to_vec());
            pos += sizes[s];
            s += 1;
        }
    }
    assert_eq!(samples.len(), count);
    Track { cookie, samples }
}

/// MD5 (RFC 1321) of `data`, lower-case hex.
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let t: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    let mut h = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in msg.as_chunks::<64>().0 {
        let x: Vec<u32> = block
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((b & d) | (c & !d), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rot = S[(i / 16) * 4 + i % 4];
            let sum = a.wrapping_add(f).wrapping_add(t[i]).wrapping_add(x[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(sum.rotate_left(rot));
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    h.iter()
        .flat_map(|v| v.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn md5_known_answers() {
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5_hex(
            b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
        ),
        "57edf4a22be3c955ac49da2e2107b67a"
    );
}
