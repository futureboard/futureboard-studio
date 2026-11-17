//! Truncated, corrupted and hostile inputs: the readers must return errors or stop, never panic,
//! hang or allocate without bound.

mod common;

use std::io::Cursor;

use common::*;

fn all_inputs() -> Vec<(String, Vec<u8>)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(root().join("audio/format")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|e| e == "md") {
            continue;
        }
        v.push((
            p.file_name().unwrap().to_string_lossy().to_string(),
            std::fs::read(&p).unwrap(),
        ));
    }
    for n in [
        "tone.mp3",
        "tone.aac",
        "tone.flac",
        "tone.opus",
        "tone_alac.m4a",
        "tone51_eac3.mp4",
        "av1_aac.mp4",
    ] {
        v.push((n.to_string(), std::fs::read(clip(n)).unwrap()));
    }
    v
}

/// Open, read everything and seek around; any error is fine, a panic is not.
fn exercise(name: &str, data: Vec<u8>) {
    let len = data.len();
    let Ok(mut d) = mkff_audio_format::open(Box::new(Cursor::new(data)), name) else {
        return;
    };
    let tracks = d.tracks().to_vec();
    assert!(!tracks.is_empty(), "{name}");
    let mut n = 0;
    let mut bytes = 0usize;
    while let Ok(Some(p)) = d.next_packet() {
        assert!(p.track < tracks.len());
        n += 1;
        bytes += p.data.len();
        assert!(n < 200_000, "{name}: runaway packet loop");
    }
    // packets come from the file (PCM packets are bounded by the data too)
    assert!(
        bytes <= len * 2,
        "{name}: {bytes} bytes of packets from a {len}-byte file"
    );
    for t in [0, 1000, 5000, 1 << 40, -5] {
        if d.seek(0, t).is_ok() {
            for _ in 0..3 {
                if !matches!(d.next_packet(), Ok(Some(_))) {
                    break;
                }
            }
        }
    }
}

#[test]
fn truncated_files() {
    for (name, data) in all_inputs() {
        let len = data.len();
        let mut cuts: Vec<usize> = (0..64.min(len)).collect();
        cuts.extend((0..len).step_by((len / 97).max(1)));
        for cut in cuts {
            exercise(&name, data[..cut].to_vec());
        }
    }
}

#[test]
fn corrupted_files() {
    let mut seed = std::env::var("MKFF_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x1234_5678_9ABC_DEF0u64);
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for (name, data) in all_inputs() {
        let rounds: usize = std::env::var("MKFF_FUZZ_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24);
        for round in 0..rounds {
            let mut d = data.clone();
            let flips = 1 + round % 8;
            for _ in 0..flips {
                let at = (rnd() % d.len() as u64) as usize;
                // headers live at the start: hit them more often
                let at = if round % 3 == 0 {
                    at % d.len().min(512)
                } else {
                    at
                };
                d[at] = rnd() as u8;
            }
            exercise(&name, d);
        }
    }
}

#[test]
fn hostile_headers() {
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    // WAV with absurd chunk sizes and a fmt that claims 0 channels
    let mut w = b"RIFF\xff\xff\xff\xffWAVEfmt \xff\xff\xff\x7f".to_vec();
    w.extend_from_slice(&[1, 0, 0, 0, 0x44, 0xAC, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0]);
    cases.push(("a.wav", w));
    let mut w = b"RIFF\x00\x00\x00\x00WAVEfmt \x10\x00\x00\x00".to_vec();
    w.extend_from_slice(&[1, 0, 2, 0, 0x44, 0xAC, 0, 0, 0, 0, 0, 0, 4, 0, 16, 0]);
    w.extend_from_slice(b"data\xff\xff\xff\xff\x01\x02");
    cases.push(("b.wav", w));
    // RF64 whose ds64 claims a huge data chunk
    let mut w = b"RF64\xff\xff\xff\xffWAVEds64\x1c\x00\x00\x00".to_vec();
    w.extend_from_slice(&[0xFF; 28]);
    w.extend_from_slice(b"fmt \x10\x00\x00\x00");
    w.extend_from_slice(&[1, 0, 1, 0, 0x44, 0xAC, 0, 0, 0, 0, 0, 0, 2, 0, 16, 0]);
    w.extend_from_slice(b"data\xff\xff\xff\xff\x00\x00\x00\x00");
    cases.push(("c.wav", w));
    // FLAC with a giant metadata block and a giant seek table
    cases.push(("a.flac", b"fLaC\x03\xff\xff\xff\x00\x00".to_vec()));
    cases.push(("b.flac", b"fLaC\x80\x00\x00\x22\x10\x00\x10\x00\x00\x00\x00\x00\x00\x00\x0a\xc4\x42\xf0\x00\x00\x00\x00".to_vec()));
    // MP4 with a moov claiming 4 GiB, and a stsz claiming 2^32 samples
    cases.push((
        "a.mp4",
        b"\x00\x00\x00\x08ftyp\xff\xff\xff\xffmoov\x00\x00".to_vec(),
    ));
    let stsz = [
        &[0u8, 0, 0, 20][..],
        b"stsz",
        &[0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF],
    ]
    .concat();
    let stbl = [&((stsz.len() + 8) as u32).to_be_bytes()[..], b"stbl", &stsz].concat();
    let minf = [&((stbl.len() + 8) as u32).to_be_bytes()[..], b"minf", &stbl].concat();
    let hdlr = [
        &[0u8, 0, 0, 20][..],
        b"hdlr",
        &[0, 0, 0, 0, 0, 0, 0, 0],
        b"soun",
    ]
    .concat();
    let mdhd = [
        &[0u8, 0, 0, 32][..],
        b"mdhd",
        &[0; 12],
        &[0, 0, 0xAC, 0x44],
        &[0; 8],
    ]
    .concat();
    let mdia = [
        &((minf.len() + hdlr.len() + mdhd.len() + 8) as u32).to_be_bytes()[..],
        b"mdia",
        &mdhd,
        &hdlr,
        &minf,
    ]
    .concat();
    let trak = [&((mdia.len() + 8) as u32).to_be_bytes()[..], b"trak", &mdia].concat();
    let moov = [&((trak.len() + 8) as u32).to_be_bytes()[..], b"moov", &trak].concat();
    cases.push(("b.mp4", moov));
    // Ogg page with a bad CRC, and an endless run of continued pages
    let mut page = b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x13".to_vec();
    page.extend_from_slice(b"OpusHead\x01\x01\x38\x01\x80\xbb\x00\x00\x00\x00\x00");
    cases.push(("a.opus", page));
    // MP3 / ADTS: lone sync words
    cases.push(("a.mp3", [0xFF, 0xFB, 0x90, 0x64].repeat(1000)));
    cases.push((
        "a.aac",
        [0xFF, 0xF1, 0x50, 0x80, 0x00, 0x1F, 0xFC].repeat(1000),
    ));
    cases.push(("b.aac", vec![0xFF; 10_000]));
    for (name, data) in cases {
        exercise(name, data);
    }
}
