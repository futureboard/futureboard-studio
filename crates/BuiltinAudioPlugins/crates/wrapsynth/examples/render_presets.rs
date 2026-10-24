//! Renders every factory preset to a WAV file, and times the heaviest patch
//! the synth can be asked for.
//!
//! ```text
//! cargo run -p wrapsynth --example render_presets --release -- <out-dir>
//! ```
//!
//! Each preset plays the same short phrase — a chord, a held bass note under
//! a melody — at 48 kHz, 24-bit stereo, so presets can be compared by ear.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use builtin_dsp_core::Instrument;
use wrapsynth::{Dsp, MAX_VOICES, Params, factory_presets};

const RATE: u32 = 48_000;
const BLOCK: usize = 256;

/// (start in beats, length in beats, note, velocity) at 110 BPM.
const PHRASE: &[(f32, f32, u8, u8)] = &[
    (0.0, 3.5, 48, 96),
    (0.0, 3.5, 55, 90),
    (0.0, 3.5, 64, 92),
    (0.0, 3.5, 67, 88),
    (4.0, 1.0, 72, 110),
    (5.0, 0.5, 74, 100),
    (5.5, 0.5, 76, 104),
    (6.0, 2.0, 79, 112),
    (4.0, 4.0, 36, 100),
];
const BPM: f32 = 110.0;
const TAIL_SEC: f32 = 2.5;

fn render(params: Params) -> Vec<f32> {
    let mut dsp = Dsp::new(RATE as f32);
    dsp.set_params(params);
    dsp.set_tempo_bpm(BPM);
    let seconds_per_beat = 60.0 / BPM;
    let mut events: Vec<(usize, u8, u8, u8)> = Vec::new();
    for &(start, length, note, velocity) in PHRASE {
        let on = (start * seconds_per_beat * RATE as f32) as usize;
        let off = ((start + length) * seconds_per_beat * RATE as f32) as usize;
        events.push((on, 0x90, note, velocity));
        events.push((off, 0x80, note, 0));
    }
    events.sort_by_key(|event| event.0);
    let end = events.last().map_or(0, |event| event.0) + (TAIL_SEC * RATE as f32) as usize;
    let mut out = vec![0.0f32; end * 2];
    let mut next = 0;
    let mut frame = 0;
    while frame < end {
        let frames = BLOCK.min(end - frame);
        while next < events.len() && events[next].0 < frame + frames {
            let (at, status, note, velocity) = events[next];
            dsp.queue_midi((at - frame) as u32, status, note, velocity);
            next += 1;
        }
        dsp.process_block(&mut out[frame * 2..(frame + frames) * 2], frames);
        frame += frames;
    }
    out
}

fn write_wav(path: &PathBuf, samples: &[f32]) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data_len = (samples.len() * 3) as u32;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&2u16.to_le_bytes())?; // stereo
    file.write_all(&RATE.to_le_bytes())?;
    file.write_all(&(RATE * 2 * 3).to_le_bytes())?;
    file.write_all(&6u16.to_le_bytes())?;
    file.write_all(&24u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
        file.write_all(&value.to_le_bytes()[..3])?;
    }
    file.flush()
}

fn benchmark() {
    // Sixteen voices, each two oscillators of seven unison voices, the
    // heaviest filter and every effect: the most the synth can be asked to do.
    let mut params = wrapsynth::default_params();
    params.unison = 7;
    params.osc_a_wave = wrapsynth::Wavetable::Formant;
    params.osc_a_position = 0.37;
    params.osc_b_wave = wrapsynth::Wavetable::HardSync;
    params.osc_b_position = 0.61;
    params.osc_b_level = 0.6;
    params.sub_level = 0.3;
    params.noise_level = 0.1;
    params.filter_drive = 0.4;
    params.analog = 0.5;
    params.filter_type = wrapsynth::FilterType::FormantE;
    params.dist_on = true;
    params.chorus_on = true;
    params.delay_on = true;
    params.reverb_on = true;
    params.sustain = 1.0;
    let mut dsp = Dsp::new(RATE as f32);
    dsp.set_params(params);
    for note in 0..MAX_VOICES as u8 {
        dsp.note_on(40 + note * 2, 100);
    }
    let seconds = 10;
    let mut block = [0.0f32; BLOCK * 2];
    let started = Instant::now();
    for _ in 0..(seconds * RATE as usize / BLOCK) {
        dsp.process_block(&mut block, BLOCK);
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "worst case ({MAX_VOICES} voices × 2 osc × 7 unison, formant filter, all FX): \
         {:.1}% of one core ({:.3} s for {seconds} s of audio)",
        100.0 * elapsed / seconds as f64,
        elapsed
    );
    let mut params = wrapsynth::default_params();
    params.sustain = 1.0;
    let mut dsp = Dsp::new(RATE as f32);
    dsp.set_params(params);
    for note in [48, 55, 60, 64, 67, 71] {
        dsp.note_on(note, 100);
    }
    let started = Instant::now();
    for _ in 0..(seconds * RATE as usize / BLOCK) {
        dsp.process_block(&mut block, BLOCK);
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "typical (init patch, 6-note chord): {:.1}% of one core",
        100.0 * elapsed / seconds as f64
    );
}

fn main() -> std::io::Result<()> {
    let out_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "wrapsynth-presets".into()),
    );
    std::fs::create_dir_all(&out_dir)?;
    benchmark();
    for (index, preset) in factory_presets().into_iter().enumerate() {
        let samples = render(preset.params);
        let peak = samples.iter().fold(0.0f32, |peak, x| peak.max(x.abs()));
        let name = format!(
            "{:02} {}.wav",
            index,
            preset.name.replace(['/', '\\', ':'], "-")
        );
        let path = out_dir.join(&name);
        write_wav(&path, &samples)?;
        println!("{name}: peak {:.1} dBFS", 20.0 * peak.max(1.0e-9).log10());
    }
    Ok(())
}
