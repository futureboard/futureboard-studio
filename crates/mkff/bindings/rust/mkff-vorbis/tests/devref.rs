//! Development-only comparison against full ffmpeg decodes (`MKFF_VORBIS_FULLREF` = directory of
//! `<fixture>.f32` files made with `ffmpeg -c:a vorbis -i <fixture>.ogg -f f32le`). Skipped when
//! the variable is unset.

mod common;

use common::*;
use mkff_audio_core::Decoder;
use mkff_vorbis::VorbisDecoder;

#[test]
fn full_reference_snr() {
    let Ok(dir) = std::env::var("MKFF_VORBIS_FULLREF") else {
        return;
    };
    let mut names: Vec<_> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".ogg"))
        .collect();
    names.sort();
    for name in names {
        let ogg = read_ogg(&fixture(&name));
        let mut dec = VorbisDecoder::new(&[]).unwrap();
        let mut out = mkff_audio_core::AudioBuffer::default();
        let t = std::time::Instant::now();
        for p in &ogg.packets {
            let b = dec.decode(p).unwrap();
            out.append(&b);
        }
        let el = t.elapsed().as_secs_f64();
        let decoded = out.frames();
        out.truncate(ogg.last_granule.max(0) as usize);
        let got = out.interleaved();
        let want =
            f32_file(&std::fs::read(format!("{dir}/{}", name.replace(".ogg", ".f32"))).unwrap());
        let ch = dec.channels();
        let secs = out.frames() as f64 / f64::from(dec.sample_rate());
        // gain estimate
        let (mut sab, mut sbb) = (0f64, 0f64);
        for (a, b) in want.iter().zip(&got) {
            sab += f64::from(*a) * f64::from(*b);
            sbb += f64::from(*b) * f64::from(*b);
        }
        let mut per_ch = Vec::new();
        for c in 0..ch {
            let w: Vec<f32> = want.iter().skip(c).step_by(ch).copied().collect();
            let g: Vec<f32> = got.iter().skip(c).step_by(ch).copied().collect();
            per_ch.push(format!("{:.1}", snr_db(&w, &g)));
        }
        println!(
            "{name}: ch {ch} rate {} decoded {decoded} granule {} ref {} frames; SNR {:.1} dB per-ch [{}]; gain {:.4}; {:.0}x realtime",
            dec.sample_rate(),
            ogg.last_granule,
            want.len() / ch,
            snr_db(&want, &got),
            per_ch.join(", "),
            sab / sbb,
            secs / el
        );
    }
}
