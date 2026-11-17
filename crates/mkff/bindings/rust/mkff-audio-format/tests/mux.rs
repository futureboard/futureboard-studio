//! Muxers round-tripped through the demuxers. Set `MKFF_MUX_OUT=<dir>` to also write the files
//! (they were checked with ffprobe / ffmpeg during development; see the README of the fixtures).

mod common;

use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use common::*;
use mkff_audio_format::{
    decode_pcm, encode_pcm, ogg_flac_headers, xiph_unlace, AdtsWriter, AudioBuffer, CodecId,
    DemuxPacket, Demuxer, FlacWriter, FlacWriterOptions, M4aTrack, M4aWriter, MpaGapless,
    MpaInfoTag, MpaWriter, OggOpusWriter, OggWriter, PcmFormat, Track, WavSampleFormat, WavWriter,
};

fn dump(name: &str, bytes: &[u8]) {
    if let Ok(dir) = std::env::var("MKFF_MUX_OUT") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(std::path::Path::new(&dir).join(name), bytes).unwrap();
    }
}

fn reopen(bytes: Vec<u8>, name: &str) -> Box<dyn Demuxer> {
    mkff_audio_format::open(Box::new(Cursor::new(bytes)), name)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn source(p: std::path::PathBuf) -> (Track, Vec<DemuxPacket>) {
    let mut d = open(&p);
    let t = d.tracks()[0].clone();
    let pk = read_all(d.as_mut())
        .into_iter()
        .filter(|p| p.track == 0)
        .collect();
    (t, pk)
}

fn sine(rate: u32, channels: usize, frames: usize) -> AudioBuffer {
    let mut b = AudioBuffer::new(rate, channels, frames);
    for (c, ch) in b.channels.iter_mut().enumerate() {
        for (i, s) in ch.iter_mut().enumerate() {
            *s = 0.5 * (i as f32 * 0.03 * (c + 1) as f32).sin();
        }
    }
    b
}

#[test]
fn wav_writer_round_trip() {
    for (fmt, ch, mask, tol) in [
        (WavSampleFormat::Pcm16, 2, None, 1e-4),
        (WavSampleFormat::Pcm24, 6, Some(0x3F), 1e-6),
        (WavSampleFormat::Pcm32, 1, Some(0x4), 1e-7),
        (WavSampleFormat::Float32, 2, Some(0x3), 0.0),
    ] {
        let buf = sine(48000, ch, 10_001);
        let mut w = WavWriter::new(Cursor::new(Vec::new()), 48000, ch as u32, fmt).unwrap();
        w.write(&buf).unwrap();
        assert_eq!(w.frames_written(), 10_001);
        let bytes = w.finish().unwrap().into_inner();
        let name = format!("out_{fmt:?}_{ch}ch.wav");
        dump(&name, &bytes);
        assert_eq!(bytes.len() % 2, 0);
        let mut d = reopen(bytes, &name);
        let t = d.tracks()[0].clone();
        assert_eq!(
            (t.sample_rate, t.channels, t.duration, t.channel_mask),
            (48000, ch as u32, Some(10_001), mask)
        );
        let mut got = AudioBuffer::default();
        while let Some(p) = d.next_packet().unwrap() {
            got.append(&decode_pcm(&p.data, t.pcm.unwrap(), ch));
        }
        assert_eq!(got.frames(), 10_001);
        for (a, b) in got
            .channels
            .iter()
            .flatten()
            .zip(buf.channels.iter().flatten())
        {
            assert!((a - b).abs() <= tol, "{name}: {a} {b}");
        }
    }
}

/// A `Write + Read + Seek` that keeps only the first MiB and reads zeros after it: enough to
/// write and parse a WAV file larger than 4 GiB without storing it.
struct Sparse {
    head: Vec<u8>,
    len: u64,
    pos: u64,
}

impl Write for Sparse {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let keep = self.head.len() as u64;
        if self.pos < keep {
            let n = ((keep - self.pos) as usize).min(b.len());
            self.head[self.pos as usize..self.pos as usize + n].copy_from_slice(&b[..n]);
        }
        self.pos += b.len() as u64;
        self.len = self.len.max(self.pos);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for Sparse {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.len.saturating_sub(self.pos) as usize).min(b.len());
        for (i, x) in b[..n].iter_mut().enumerate() {
            *x = self
                .head
                .get((self.pos + i as u64) as usize)
                .copied()
                .unwrap_or(0);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Sparse {
    fn seek(&mut self, s: SeekFrom) -> std::io::Result<u64> {
        self.pos = match s {
            SeekFrom::Start(p) => p,
            SeekFrom::End(o) => self.len.checked_add_signed(o).unwrap(),
            SeekFrom::Current(o) => self.pos.checked_add_signed(o).unwrap(),
        };
        Ok(self.pos)
    }
}

#[test]
fn wav_writer_upgrades_to_rf64() {
    let sink = Sparse {
        head: vec![0; 1 << 20],
        len: 0,
        pos: 0,
    };
    let mut w = WavWriter::new(sink, 48000, 2, WavSampleFormat::Pcm24).unwrap();
    let block = vec![0u8; 6 << 20];
    let mut written = 0u64;
    while written <= 0x1_0000_0000 {
        w.write_bytes(&block).unwrap();
        written += block.len() as u64;
    }
    let mut sink = w.finish().unwrap();
    assert_eq!(&sink.head[..4], b"RF64");
    assert_eq!(&sink.head[12..16], b"ds64");
    let data = u64::from_le_bytes(sink.head[28..36].try_into().unwrap());
    assert_eq!(data, written);
    sink.pos = 0;
    let mut d = mkff_audio_format::open(Box::new(sink), "big.wav").unwrap();
    assert_eq!(d.tracks()[0].duration, Some(written / 6));
    assert_eq!(
        d.seek(0, (written / 6) as i64 - 10).unwrap(),
        (written / 6) as i64 - 10
    );
    assert_eq!(d.next_packet().unwrap().unwrap().duration, 10);
}

#[test]
fn wav_writer_pads_odd_data() {
    let mut w = WavWriter::new(Cursor::new(Vec::new()), 8000, 1, WavSampleFormat::Pcm24).unwrap();
    w.write_interleaved(&[0.1, 0.2, 0.3]).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    assert_eq!(bytes.len() % 2, 0);
    let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    assert_eq!(riff + 8, bytes.len());
    let mut d = reopen(bytes, "odd.wav");
    assert_eq!(d.tracks()[0].duration, Some(3));
    assert_eq!(read_all(d.as_mut()).len(), 1);
}

#[test]
fn mpa_writer_gapless_tag() {
    for (src, name, vbr) in [
        (fixture("vbr.mp3"), "out_vbr.mp3", true),
        (clip("tone.mp3"), "out_cbr.mp3", false),
    ] {
        let (t, pk) = source(src);
        let mut w = MpaWriter::new(Cursor::new(Vec::new()), true).unwrap();
        for p in &pk {
            w.write_frame(&p.data).unwrap();
        }
        let total = pk.len() as u64 * 1152;
        let length = t.duration.unwrap();
        let delay = 576u32;
        let padding = (total - length - u64::from(delay)) as u32;
        let bytes = w
            .finish(MpaInfoTag::Auto(Some(MpaGapless {
                encoder_delay: delay,
                padding,
            })))
            .unwrap()
            .into_inner();
        dump(name, &bytes);
        let tag = &bytes[4 + if t.channels == 1 { 17 } else { 32 }..][..4];
        assert_eq!(tag, if vbr { b"Xing" } else { b"Info" });
        let mut d = reopen(bytes, name);
        let t2 = d.tracks()[0].clone();
        assert_eq!(
            (t2.duration, t2.priming, t2.padding),
            (
                Some(length),
                u64::from(delay) + 529,
                u64::from(padding).saturating_sub(529)
            )
        );
        let back = read_all(d.as_mut());
        assert_eq!(back.len(), pk.len());
        assert!(back.iter().zip(&pk).all(|(a, b)| a.data == b.data));
    }
    // without reservation the stream is just the frames
    let (_, pk) = source(fixture("tone.mp2"));
    let mut w = MpaWriter::new(Cursor::new(Vec::new()), true).unwrap();
    for p in &pk {
        w.write_frame(&p.data).unwrap();
    }
    assert_eq!(w.reserved_len(), 0, "layer II gets no Xing frame");
    let bytes = w.finish(MpaInfoTag::Auto(None)).unwrap().into_inner();
    assert_eq!(bytes.len(), pk.iter().map(|p| p.data.len()).sum::<usize>());
    assert!(MpaWriter::new(Cursor::new(Vec::new()), false)
        .unwrap()
        .write_frame(&[0xFF, 0xFB, 0x90, 0x64])
        .is_err());
}

#[test]
fn adts_writer_round_trip() {
    let (t, pk) = source(fixture("aac.m4a"));
    let mut w = AdtsWriter::new(Vec::new(), &t.config).unwrap();
    for p in &pk {
        w.write_packet(&p.data).unwrap();
    }
    let bytes = w.finish().unwrap();
    dump("out.aac", &bytes);
    let mut d = reopen(bytes, "out.aac");
    let t2 = d.tracks()[0].clone();
    assert_eq!(
        (t2.codec, t2.sample_rate, t2.channels),
        (CodecId::Aac, 44100, 1)
    );
    assert_eq!(t2.config[..2], t.config[..2]);
    let back = read_all(d.as_mut());
    assert_eq!(back.len(), pk.len());
    assert!(back
        .iter()
        .zip(&pk)
        .all(|(a, b)| a.data == b.data && a.pts == b.pts));
}

#[test]
fn flac_writer_round_trip() {
    let (t, pk) = source(fixture("seek.flac"));
    // zero the total in the STREAMINFO we start from: finish must fill it in
    let mut info = t.config.clone();
    info[13] &= 0xF0;
    info[14..18].fill(0);
    let mut w = FlacWriter::new(
        Cursor::new(Vec::new()),
        &info,
        FlacWriterOptions {
            seek_points: 10,
            padding: 100,
        },
    )
    .unwrap();
    for p in &pk {
        w.write_frame(&p.data).unwrap();
    }
    let bytes = w.finish(None).unwrap().into_inner();
    dump("out.flac", &bytes);
    // fLaC, STREAMINFO, SEEKTABLE with real points, PADDING
    assert_eq!(&bytes[..4], b"fLaC");
    assert_eq!(bytes[4 + 4 + 34] & 0x7F, 3);
    let first_point = u64::from_be_bytes(bytes[4 + 4 + 34 + 4 + 18..][..8].try_into().unwrap());
    assert!(first_point > 0 && first_point < 44100);
    let mut d = reopen(bytes, "out.flac");
    assert_eq!(d.tracks()[0].duration, Some(44100));
    let back = read_all(d.as_mut());
    assert_eq!(back.len(), pk.len());
    assert!(back
        .iter()
        .zip(&pk)
        .all(|(a, b)| a.data == b.data && a.pts == b.pts));
    let got = d.seek(0, 30_000).unwrap();
    let p = d.next_packet().unwrap().unwrap();
    assert!(got <= 30_000 && got + i64::from(p.duration) > 30_000 && p.pts == got);
}

#[test]
fn ogg_opus_writer_round_trip() {
    let (t, pk) = source(fixture("long.opus"));
    let mut w = OggOpusWriter::new(Vec::new(), 0x1234, &t.config, "mkff").unwrap();
    for p in &pk {
        w.write_packet(&p.data).unwrap();
    }
    let bytes = w.finish(t.duration).unwrap();
    dump("out.opus", &bytes);
    let mut d = reopen(bytes, "out.opus");
    let t2 = d.tracks()[0].clone();
    assert_eq!(
        (t2.duration, t2.priming, t2.padding),
        (t.duration, t.priming, t.padding)
    );
    let back = read_all(d.as_mut());
    assert_eq!(back.len(), pk.len());
    assert!(back
        .iter()
        .zip(&pk)
        .all(|(a, b)| a.data == b.data && a.pts == b.pts && a.duration == b.duration));
}

#[test]
fn ogg_vorbis_and_flac_round_trip() {
    let (t, pk) = source(fixture("long_vorbis.ogg"));
    let headers = xiph_unlace(&t.config).unwrap();
    let mut w = OggWriter::new(Vec::new(), 7);
    w.write_packet(&headers[0], 0, true).unwrap();
    w.write_packet(&headers[1], 0, false).unwrap();
    w.write_packet(&headers[2], 0, true).unwrap();
    for p in &pk {
        w.write_packet(&p.data, p.pts + i64::from(p.duration), false)
            .unwrap();
    }
    let bytes = w.finish(Some(t.duration.unwrap() as i64)).unwrap();
    dump("out_vorbis.ogg", &bytes);
    let mut d = reopen(bytes, "out.ogg");
    let t2 = d.tracks()[0].clone();
    assert_eq!(
        (t2.duration, t2.padding, t2.config.clone()),
        (t.duration, t.padding, t.config.clone())
    );
    let back = read_all(d.as_mut());
    assert!(back
        .iter()
        .zip(&pk)
        .all(|(a, b)| a.data == b.data && a.pts == b.pts));

    let (t, pk) = source(fixture("seek.flac"));
    let [h0, h1] = ogg_flac_headers(&t.config, "mkff").unwrap();
    let mut w = OggWriter::new(Vec::new(), 9);
    w.write_packet(&h0, 0, true).unwrap();
    w.write_packet(&h1, 0, true).unwrap();
    for p in &pk {
        w.write_packet(&p.data, p.pts + i64::from(p.duration), false)
            .unwrap();
    }
    let bytes = w.finish(None).unwrap();
    dump("out_flac.oga", &bytes);
    let mut d = reopen(bytes, "out.oga");
    assert_eq!(d.tracks()[0].codec, CodecId::Flac);
    assert_eq!(d.tracks()[0].duration, Some(44100));
    let back = read_all(d.as_mut());
    assert_eq!(back.len(), pk.len());
    assert!(back
        .iter()
        .zip(&pk)
        .all(|(a, b)| a.data == b.data && a.pts == b.pts));
}

/// Remux a demuxed track into M4A and read it back.
fn m4a_round_trip(
    t: &Track,
    pk: &[DemuxPacket],
    name: &str,
    faststart: bool,
) -> (Track, Vec<DemuxPacket>, Vec<u8>) {
    let mut w = M4aWriter::new(Cursor::new(Vec::new())).unwrap();
    let ti = w.add_track(M4aTrack::from_track(t)).unwrap();
    for p in pk {
        w.write_packet(ti, &p.data, p.duration).unwrap();
    }
    let bytes = if faststart {
        w.finish_faststart()
    } else {
        w.finish()
    }
    .unwrap()
    .into_inner();
    dump(name, &bytes);
    let mut d = reopen(bytes.clone(), name);
    let t2 = d.tracks()[0].clone();
    let back = read_all(d.as_mut());
    (t2, back, bytes)
}

#[test]
fn m4a_writer_round_trips() {
    for (src, name) in [
        (fixture("aac.m4a"), "out_aac.m4a"),
        (clip("tone_alac.m4a"), "out_alac.m4a"),
        (fixture("flac.mp4"), "out_flac.mp4"),
        (fixture("opus.mp4"), "out_opus.mp4"),
        (fixture("long.opus"), "out_opus_from_ogg.mp4"),
        (fixture("vbr.mp3"), "out_mp3.m4a"),
        (clip("av1_aac.mp4"), "out_aac48.m4a"),
    ] {
        let (t, pk) = source(src);
        for fast in [false, true] {
            let name = if fast {
                format!("fast_{name}")
            } else {
                name.to_string()
            };
            let (t2, back, bytes) = m4a_round_trip(&t, &pk, &name, fast);
            assert_eq!(
                (t2.codec, t2.sample_rate, t2.channels, t2.config.clone()),
                (t.codec, t.sample_rate, t.channels, t.config.clone()),
                "{name}"
            );
            assert_eq!((t2.priming, t2.duration), (t.priming, t.duration), "{name}");
            assert_eq!(back.len(), pk.len(), "{name}");
            for (a, b) in back.iter().zip(&pk) {
                assert_eq!((&a.data, a.duration), (&b.data, b.duration), "{name}");
            }
            let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
            let mdat = bytes.windows(4).position(|w| w == b"mdat").unwrap();
            assert_eq!(moov < mdat, fast, "{name}");
            if t.codec == CodecId::Aac && t.priming > 0 {
                assert!(bytes.windows(8).any(|w| w == b"iTunSMPB"), "{name}");
            }
        }
    }
}

#[test]
fn m4a_writer_pcm() {
    for (src, fmt) in [
        ("s16_stereo.wav", PcmFormat::int(16, false)),
        ("s24.aiff", PcmFormat::int(24, true)),
        ("f32_mono.wav", PcmFormat::float(32, false)),
        ("f64_mono.wav", PcmFormat::float(64, true)),
    ] {
        let (t, pk) = source(fixture(src));
        let mut all = AudioBuffer::default();
        for p in &pk {
            all.append(&decode_pcm(&p.data, t.pcm.unwrap(), t.channels as usize));
        }
        let mut desc = M4aTrack::new(CodecId::Pcm, t.sample_rate, t.channels, Vec::new());
        desc.pcm = Some(fmt);
        let mut w = M4aWriter::new(Cursor::new(Vec::new())).unwrap();
        w.add_track(desc).unwrap();
        let data = encode_pcm(&all, fmt).unwrap();
        for chunk in data.chunks(1000 * fmt.bytes_per_sample() * t.channels as usize) {
            w.write_packet(0, chunk, 0).unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        let name = format!("out_pcm_{}.mp4", fmt.label());
        dump(&name, &bytes);
        let mut d = reopen(bytes, &name);
        let t2 = d.tracks()[0].clone();
        assert_eq!(
            (t2.pcm, t2.channels, t2.duration),
            (Some(fmt), t.channels, Some(4410)),
            "{name}"
        );
        let mut got = AudioBuffer::default();
        while let Some(p) = d.next_packet().unwrap() {
            got.append(&decode_pcm(&p.data, fmt, t.channels as usize));
        }
        assert_eq!(got.channels, all.channels, "{name}");
    }
}

#[test]
fn m4a_writer_two_tracks_interleaved() {
    let (ta, pa) = source(fixture("aac.m4a"));
    let (tf, pf) = source(fixture("flac.mp4"));
    let mut w = M4aWriter::new(Cursor::new(Vec::new())).unwrap();
    let a = w.add_track(M4aTrack::from_track(&ta)).unwrap();
    let f = w.add_track(M4aTrack::from_track(&tf)).unwrap();
    let (mut i, mut j) = (0, 0);
    while i < pa.len() || j < pf.len() {
        if j >= pf.len() || (i < pa.len() && pa[i].pts <= pf[j].pts) {
            w.write_packet(a, &pa[i].data, pa[i].duration).unwrap();
            i += 1;
        } else {
            w.write_packet(f, &pf[j].data, pf[j].duration).unwrap();
            j += 1;
        }
    }
    let bytes = w.finish().unwrap().into_inner();
    dump("out_two_tracks.mp4", &bytes);
    let mut d = reopen(bytes, "two.mp4");
    assert_eq!(d.tracks().len(), 2);
    let back = read_all(d.as_mut());
    let (na, _, _) = stats(&back, 0);
    let (nf, _, _) = stats(&back, 1);
    assert_eq!((na, nf), (pa.len(), pf.len()));
    // file order: track 1 packets appear between track 0 packets
    let first_f = back.iter().position(|p| p.track == 1).unwrap();
    assert!(first_f > 0 && first_f < back.len() - 1);
}

#[test]
fn m4a_itunsmpb_without_edit_list() {
    let (t, pk) = source(fixture("aac.m4a"));
    let (_, _, mut bytes) = m4a_round_trip(&t, &pk, "smpb.m4a", false);
    // hide the edit list: the reader falls back to iTunSMPB
    let at = bytes.windows(4).position(|w| w == b"edts").unwrap();
    bytes[at..at + 4].copy_from_slice(b"free");
    let d = reopen(bytes, "smpb.m4a");
    let t2 = &d.tracks()[0];
    assert_eq!(
        (t2.priming, t2.duration, t2.padding),
        (1024, Some(11025), 0)
    );
}
