//! Large or unusual streams built in memory: they exercise the paths small fixtures cannot
//! (bisection, table-based MP3 seeking, ADTS CRCs and multi-block frames, VBRI, odd WAV chunks).

mod common;

use std::io::Cursor;

use common::read_all;
use mkff_audio_core::crc::{crc16, crc8};
use mkff_audio_format::{
    adts_header, open, FlacWriter, FlacWriterOptions, MpaGapless, MpaInfoTag, MpaWriter,
    OggOpusWriter,
};

fn reopen(bytes: Vec<u8>, name: &str) -> Box<dyn mkff_audio_format::Demuxer> {
    open(Box::new(Cursor::new(bytes)), name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// An MPEG-1 layer III 128 kb/s 44.1 kHz stereo frame whose payload encodes `n`.
fn mp3_frame(n: u32) -> Vec<u8> {
    let mut f = vec![0u8; 417];
    f[..4].copy_from_slice(&0xFFFB_9064u32.to_be_bytes());
    f[40..44].copy_from_slice(&n.to_be_bytes());
    f
}

#[test]
fn mp3_xing_toc_seek_on_a_large_file() {
    // 20 MiB: beyond the full-scan and scan-seek limits, so seeks use the Xing TOC
    let frames = 50_000u32;
    let mut w = MpaWriter::new(Cursor::new(Vec::new()), true).unwrap();
    for i in 0..frames {
        w.write_frame(&mp3_frame(i)).unwrap();
    }
    let bytes = w
        .finish(MpaInfoTag::Auto(Some(MpaGapless {
            encoder_delay: 576,
            padding: 1000,
        })))
        .unwrap()
        .into_inner();
    let mut d = reopen(bytes, "big.mp3");
    let t = d.tracks()[0].clone();
    assert_eq!(t.duration, Some(u64::from(frames) * 1152 - 1576));
    // near the start: exact (scanned)
    assert_eq!(d.seek(0, 1152 * 100 + 5).unwrap(), 1152 * 100);
    let p = d.next_packet().unwrap().unwrap();
    assert_eq!(u32::from_be_bytes(p.data[40..44].try_into().unwrap()), 100);
    // far away: through the TOC; identical frames make the estimate land on the right frame
    for target in [30_000u32, 45_123, 49_999] {
        let got = d.seek(0, i64::from(target) * 1152).unwrap();
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, got);
        let n = u32::from_be_bytes(p.data[40..44].try_into().unwrap());
        assert!(n.abs_diff(target) <= 1, "{target}: frame {n}");
    }
}

#[test]
fn mp3_vbri_header() {
    // a Fraunhofer VBRI frame followed by 100 audio frames
    let mut first = mp3_frame(0);
    let v = &mut first[36..];
    v[..4].copy_from_slice(b"VBRI");
    v[4..6].copy_from_slice(&1u16.to_be_bytes());
    v[10..14].copy_from_slice(&(101u32 * 417).to_be_bytes());
    v[14..18].copy_from_slice(&100u32.to_be_bytes());
    v[18..20].copy_from_slice(&10u16.to_be_bytes()); // entries
    v[20..22].copy_from_slice(&1u16.to_be_bytes()); // scale
    v[22..24].copy_from_slice(&2u16.to_be_bytes()); // entry size
    v[24..26].copy_from_slice(&10u16.to_be_bytes()); // frames per entry
    for e in 0..10 {
        v[26 + e * 2..28 + e * 2].copy_from_slice(&(10u16 * 417).to_be_bytes());
    }
    let mut bytes = first;
    for i in 1..=100 {
        bytes.extend_from_slice(&mp3_frame(i));
    }
    let mut d = reopen(bytes, "vbri.mp3");
    assert_eq!(d.tracks()[0].duration, Some(100 * 1152));
    let pk = read_all(d.as_mut());
    assert_eq!(pk.len(), 100);
    assert_eq!(
        u32::from_be_bytes(pk[0].data[40..44].try_into().unwrap()),
        1
    );
}

#[test]
fn adts_with_crc_and_several_blocks() {
    // protected single-block frames: header, CRC, payload
    let mut s = Vec::new();
    for i in 0..10u8 {
        let payload = vec![i; 50 + usize::from(i)];
        let mut h = adts_header(1, 4, 2, payload.len() + 2).unwrap();
        h[1] &= !1; // protection_absent = 0
        s.extend_from_slice(&h);
        s.extend_from_slice(&[0xAB, 0xCD]);
        s.extend_from_slice(&payload);
    }
    let mut d = reopen(s, "crc.aac");
    assert_eq!(d.tracks()[0].config, vec![0x12, 0x10]);
    let pk = read_all(d.as_mut());
    assert_eq!(pk.len(), 10);
    for (i, p) in pk.iter().enumerate() {
        assert_eq!(p.data, vec![i as u8; 50 + i]);
        assert_eq!(p.pts, i as i64 * 1024);
    }
    // protected frames with 3 raw data blocks each: positions, header CRC, blocks + CRCs
    let mut s = Vec::new();
    for f in 0..4u8 {
        let blocks: Vec<Vec<u8>> = (0..3u8)
            .map(|b| vec![f * 3 + b; 20 + usize::from(b)])
            .collect();
        let mut body = Vec::new();
        let mut pos = Vec::new();
        for b in &blocks {
            pos.push(body.len() as u16);
            body.extend_from_slice(b);
            body.extend_from_slice(&[0, 0]);
        }
        let len = 7 + 2 * 2 + 2 + body.len();
        let mut h = adts_header(1, 4, 2, len - 7).unwrap();
        h[1] &= !1;
        h[6] = (h[6] & !3) | 2;
        s.extend_from_slice(&h);
        s.extend_from_slice(&pos[1].to_be_bytes());
        s.extend_from_slice(&pos[2].to_be_bytes());
        s.extend_from_slice(&[0, 0]);
        s.extend_from_slice(&body);
    }
    let mut d = reopen(s, "blocks.aac");
    let pk = read_all(d.as_mut());
    assert_eq!(pk.len(), 12);
    for (i, p) in pk.iter().enumerate() {
        assert_eq!(p.data, vec![i as u8; 20 + i % 3]);
        assert_eq!(p.pts, i as i64 * 1024);
    }
    assert_eq!(d.seek(0, 5000).unwrap(), 3 * 1024);
    assert_eq!(d.next_packet().unwrap().unwrap().data, vec![3u8; 20]);
}

/// A syntactically valid FLAC frame (fixed 4096-sample blocks, 44.1 kHz stereo 16-bit) with a
/// payload of `len` pseudo-random bytes.
fn flac_frame(n: u32, len: usize, seed: &mut u32) -> Vec<u8> {
    let mut f = vec![0xFF, 0xF8, 0xC9, 0x18];
    // UTF-8 coded frame number
    if n < 0x80 {
        f.push(n as u8);
    } else if n < 0x800 {
        f.extend_from_slice(&[0xC0 | (n >> 6) as u8, 0x80 | (n & 0x3F) as u8]);
    } else {
        f.extend_from_slice(&[
            0xE0 | (n >> 12) as u8,
            0x80 | ((n >> 6) & 0x3F) as u8,
            0x80 | (n & 0x3F) as u8,
        ]);
    }
    f.push(crc8(&f));
    for _ in 0..len {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        f.push((*seed >> 16) as u8);
    }
    let c = crc16(0, &f);
    f.extend_from_slice(&c.to_be_bytes());
    f
}

fn streaminfo(total: u64) -> Vec<u8> {
    let mut s = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
    let x: u64 = (44100u64 << 44) | (1 << 41) | (15 << 36) | total;
    s.extend_from_slice(&x.to_be_bytes());
    s.extend_from_slice(&[0; 16]);
    s
}

#[test]
fn flac_bisection_seek_without_seektable() {
    let mut seed = 1u32;
    let frames = 600u32;
    let mut w = FlacWriter::new(
        Cursor::new(Vec::new()),
        &streaminfo(0),
        FlacWriterOptions {
            seek_points: 0,
            padding: 0,
        },
    )
    .unwrap();
    for n in 0..frames {
        w.write_frame(&flac_frame(n, 1500 + (n as usize * 37) % 3000, &mut seed))
            .unwrap();
    }
    let bytes = w.finish(None).unwrap().into_inner();
    assert!(bytes.len() > 1 << 20);
    let mut d = reopen(bytes, "big.flac");
    assert_eq!(d.tracks()[0].duration, Some(u64::from(frames) * 4096));
    for target in [
        0i64,
        4096 * 7 + 5,
        4096 * 300,
        4096 * 599 + 4095,
        4096 * 451 - 1,
    ] {
        let got = d.seek(0, target).unwrap();
        assert_eq!(got, target / 4096 * 4096, "{target}");
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, got);
        assert!(p.data.len() > 1500);
    }
    // every frame comes back whole
    d.seek(0, 0).unwrap();
    assert_eq!(read_all(d.as_mut()).len(), frames as usize);
}

#[test]
fn ogg_bisection_seek() {
    let head = OggOpusWriter::<Vec<u8>>::opus_head(2, 312, 48000);
    let mut w = OggOpusWriter::new(Vec::new(), 99, &head, "mkff").unwrap();
    let n = 5000u32;
    for i in 0..n {
        let mut p = vec![0xFCu8]; // CELT 20 ms, one frame
        p.extend_from_slice(&i.to_be_bytes());
        p.resize(120, (i % 251) as u8);
        w.write_packet(&p).unwrap();
    }
    let bytes = w.finish(Some(u64::from(n) * 960 - 500)).unwrap();
    assert!(bytes.len() > 500_000);
    let mut d = reopen(bytes, "big.opus");
    let t = d.tracks()[0].clone();
    assert_eq!(
        (t.duration, t.priming, t.padding),
        (Some(u64::from(n) * 960 - 500), 312, 188)
    );
    for target in [
        0i64,
        960 * 1000 + 17,
        960 * 2500,
        960 * 4999 + 959,
        960 * 4000 - 1,
    ] {
        let got = d.seek(0, target).unwrap();
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, got);
        assert!(
            got <= target && target - got < 960 * 40,
            "{target} -> {got}"
        );
        // the packet's pts matches its index
        let idx = u32::from_be_bytes(p.data[1..5].try_into().unwrap());
        assert_eq!(i64::from(idx) * 960, got);
    }
}

#[test]
fn wav_odd_chunks_and_streaming_size() {
    let fmt = [
        1u8, 0, 1, 0, 0x40, 0x1F, 0, 0, 0x80, 0x3E, 0, 0, 2, 0, 16, 0,
    ];
    let mut w = b"RIFF\0\0\0\0WAVE".to_vec();
    w.extend_from_slice(b"LIST\x03\x00\x00\x00abc\x00"); // odd chunk + pad byte
    w.extend_from_slice(b"fmt \x10\x00\x00\x00");
    w.extend_from_slice(&fmt);
    w.extend_from_slice(b"data\x00\x00\x00\x00"); // size never patched
    w.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5]);
    let mut d = reopen(w, "stream.wav");
    assert_eq!(d.tracks()[0].sample_rate, 8000);
    assert_eq!(d.tracks()[0].duration, Some(4));
    let p = d.next_packet().unwrap().unwrap();
    assert_eq!(p.data, vec![1, 0, 2, 0, 3, 0, 4, 0]);
    // a fmt chunk after the data chunk
    let mut w = b"RIFF\0\0\0\0WAVE".to_vec();
    w.extend_from_slice(b"data\x04\x00\x00\x00\x01\x00\x02\x00");
    w.extend_from_slice(b"fmt \x10\x00\x00\x00");
    w.extend_from_slice(&fmt);
    let mut d = reopen(w, "late.wav");
    assert_eq!(d.tracks()[0].duration, Some(2));
    assert_eq!(d.next_packet().unwrap().unwrap().data, vec![1, 0, 2, 0]);
}

#[test]
fn mp3_cbr_estimate_without_xing() {
    // 20 MiB of CBR frames and no Xing frame: far seeks use the bit rate
    let mut bytes = Vec::with_capacity(50_000 * 417);
    for i in 0..50_000u32 {
        bytes.extend_from_slice(&mp3_frame(i));
    }
    let mut d = reopen(bytes, "cbr.mp3");
    assert_eq!(d.tracks()[0].duration, Some(50_000 * 1152));
    let got = d.seek(0, 40_000 * 1152).unwrap();
    let p = d.next_packet().unwrap().unwrap();
    assert_eq!(p.pts, got);
    let n = u32::from_be_bytes(p.data[40..44].try_into().unwrap());
    assert!(n.abs_diff(40_000) <= 1, "frame {n}");
    // and back near the start, exactly
    assert_eq!(d.seek(0, 1152 * 3).unwrap(), 1152 * 3);
    let p = d.next_packet().unwrap().unwrap();
    assert_eq!(u32::from_be_bytes(p.data[40..44].try_into().unwrap()), 3);
}
