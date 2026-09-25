//! Render a chord-labelled evaluation corpus from a chord-progression catalog
//! and a General MIDI SoundFont.
//!
//! ```text
//! cargo run --release -p sphere_directaudioengine --example chord_corpus -- \
//!     <catalog.json> <soundfont.sf2> <out dir> [--limit N] [--seed S]
//! ```
//!
//! Every progression becomes one clip with its own seeded arrangement: a
//! comping instrument (piano, electric piano, organ, guitars — clean or
//! distorted — strings, brass or pad, sometimes two layered), a bass line
//! on the chord's bass note, a drum kit in one of several grooves, and a
//! lead melody that plays chord tones on strong beats and scale passing
//! tones between them (the contamination a vocal brings). Key, tempo and a
//! global detuning (up to ±35 cents) are drawn per clip; some clips open
//! with a drum-only bar, labelled no-chord. The labels are the exact
//! note-on times the renderer used, so they are ground truth for this
//! audio — not for any real recording.
//!
//! Output: `<out>/audio/<id>.wav` (22.05 kHz, 16-bit mono) and
//! `<out>/labels.json` in the benchmark schema, each song tagged with a
//! fixed `split` (dev / val / test, from a hash of the progression id), so
//! tuning on dev never touches the test clips.
//!
//! The catalog is expected in the `chordgen` format: `progressions[]` with
//! `id`, `reference_key`, `modes`, `reference_chords`, `durations_beats`,
//! `tempo_reference`, `corpus` and `provenance.license`.

#[path = "support/chord_symbols.rs"]
mod chord_symbols;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chord_symbols::{Symbol, NAMES};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};
use serde_json::{json, Value};

const SAMPLE_RATE: i32 = 22_050;
const DRUMS: i32 = 9;
const CH_COMP: i32 = 0;
const CH_LAYER: i32 = 1;
const CH_BASS: i32 = 2;
const CH_LEAD: i32 = 3;
const CH_RIFF: i32 = 4;
const CH_PEDAL: i32 = 5;

/// Deterministic xorshift RNG.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn int(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo + 1) as u64) as i32
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next() % items.len() as u64) as usize]
    }
}

fn hash(text: &str) -> u64 {
    // FNV-1a: stable across runs and platforms.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[derive(Clone, Copy)]
struct Event {
    seconds: f64,
    /// Sort order at equal times: note-offs before note-ons.
    order: u8,
    channel: i32,
    command: i32,
    data1: i32,
    data2: i32,
}

struct Score {
    events: Vec<Event>,
}

impl Score {
    fn note(&mut self, channel: i32, key: i32, velocity: i32, start: f64, end: f64) {
        if !(0..=127).contains(&key) || end <= start {
            return;
        }
        self.events.push(Event {
            seconds: start,
            order: 1,
            channel,
            command: 0x90,
            data1: key,
            data2: velocity.clamp(1, 127),
        });
        self.events.push(Event {
            seconds: end,
            order: 0,
            channel,
            command: 0x80,
            data1: key,
            data2: 0,
        });
    }
    fn control(&mut self, channel: i32, command: i32, data1: i32, data2: i32) {
        self.events.push(Event {
            seconds: 0.0,
            order: 0,
            channel,
            command,
            data1,
            data2,
        });
    }
}

/// Chord voiced in close position from a random inversion, lowest note in
/// `lo..lo+12`.
fn voicing(symbol: &Symbol, lo: i32, inversion: usize) -> Vec<i32> {
    let mut pcs: Vec<i32> = symbol.pitch_classes().iter().map(|&p| p as i32).collect();
    // A power chord is played root-fifth-octave.
    if symbol.quality == "5" {
        pcs.push(symbol.root as i32);
    }
    let n = pcs.len();
    let rotated: Vec<i32> = (0..n).map(|i| pcs[(i + inversion) % n]).collect();
    let mut out = Vec::with_capacity(n);
    let mut last = lo - 1;
    for pc in rotated {
        let mut key = lo - lo.rem_euclid(12) + pc;
        while key <= last {
            key += 12;
        }
        out.push(key);
        last = key;
    }
    out
}

struct Clip {
    audio: Vec<f32>,
    chords: Vec<Value>,
    key: String,
    bpm: f64,
    detune_cents: f64,
    arrangement: Value,
}

#[allow(clippy::too_many_arguments)]
fn render(
    font: &Arc<SoundFont>,
    progression: &[(Symbol, f64)],
    key_root: u8,
    minor: bool,
    tempo: Option<f64>,
    seed: u64,
) -> Result<Clip, String> {
    let mut rng = Rng::new(seed);
    let bpm = tempo.unwrap_or_else(|| (rng.range(72f64.ln(), 176f64.ln())).exp().round());
    let beat = 60.0 / bpm;
    let transpose = rng.int(-5, 6);
    let progression: Vec<(Symbol, f64)> = progression
        .iter()
        .map(|(s, d)| (s.transposed(transpose), *d))
        .collect();
    let key_root = ((key_root as i32 + transpose).rem_euclid(12)) as u8;
    let scale: [i32; 7] = if minor {
        [0, 2, 3, 5, 7, 8, 10]
    } else {
        [0, 2, 4, 5, 7, 9, 11]
    };

    let drums = rng.chance(0.75);
    let groove = rng.int(0, 3);
    let bass_on = rng.chance(0.85);
    let lead_on = rng.chance(0.7);
    let comp_program = rng.pick(&[0, 0, 4, 16, 24, 25, 27, 29, 30, 48, 50, 61, 89]);
    let layer_program = if rng.chance(0.4) {
        Some(rng.pick(&[48, 49, 89, 91, 52]))
    } else {
        None
    };
    let bass_program = rng.pick(&[32, 33, 34, 35, 38]);
    let lead_program = rng.pick(&[73, 65, 52, 54, 80, 71, 56, 81]);
    // 0 sustain, 1 quarters, 2 eighths, 3 arpeggio, 4 strum.
    let pattern = rng.int(0, 4);
    let detune_cents = if rng.chance(0.6) {
        rng.range(-35.0, 35.0)
    } else {
        0.0
    };
    let intro_bars = if drums && rng.chance(0.35) { 1 } else { 0 };
    // What makes real mixes hard, each drawn per clip: a sung-style lead
    // that is loud and holds notes across chord changes; a riff on the key's
    // pentatonic scale that ignores the chords; a pedal (tonic and fifth
    // held under everything); and chords pushed an eighth ahead of the beat.
    let lead_volume = rng.int(90, 118);
    let legato = rng.chance(0.5);
    let riff_program = if rng.chance(0.4) {
        Some(rng.pick(&[81, 27, 5, 11, 46]))
    } else {
        None
    };
    let pedal = rng.chance(0.2);
    let anticipate = rng.chance(0.3);

    let loop_beats: f64 = progression.iter().map(|(_, d)| d).sum();
    if loop_beats <= 0.0 {
        return Err("empty progression".into());
    }
    let loops = ((26.0 / (loop_beats * beat)).ceil() as usize).clamp(2, 6);
    let lead_in = 0.25;
    let t0 = lead_in + intro_bars as f64 * 4.0 * beat;

    let mut score = Score { events: Vec::new() };
    // Programs, volumes, pitch bend (detune) on every melodic channel.
    let bend = (8192.0 + detune_cents / 200.0 * 8192.0).round() as i32;
    for (ch, program, volume) in [
        (CH_COMP, comp_program, 100),
        (CH_LAYER, layer_program.unwrap_or(48), 70),
        (CH_BASS, bass_program, 105),
        (CH_LEAD, lead_program, lead_volume),
        (CH_RIFF, riff_program.unwrap_or(81), 80),
        (CH_PEDAL, 89, 75),
    ] {
        score.control(ch, 0xC0, program, 0);
        score.control(ch, 0xB0, 7, volume);
        score.control(ch, 0xE0, bend & 0x7F, (bend >> 7) & 0x7F);
    }
    score.control(DRUMS, 0xB0, 7, 95);

    let mut chords = Vec::new();
    if t0 > 0.0 {
        chords.push(json!({ "start": 0.0, "end": round3(t0), "label": "N" }));
    }
    // Chord times: nominal, then pushed an eighth early when anticipated
    // (not the first chord), each chord ending where the next one starts.
    let sequence: Vec<(&Symbol, f64)> = (0..loops)
        .flat_map(|_| progression.iter().map(|(s, b)| (s, *b)))
        .collect();
    let mut starts = Vec::with_capacity(sequence.len() + 1);
    let mut nominal = t0;
    for (k, (_, beats)) in sequence.iter().enumerate() {
        let push = if anticipate && k > 0 { beat * 0.5 } else { 0.0 };
        starts.push(nominal - push);
        nominal += beats * beat;
    }
    starts.push(nominal);
    let mut t = t0;
    let mut last_bass = 40;
    for (k, (symbol, _)) in sequence.iter().enumerate() {
        let symbol = *symbol;
        {
            let start = starts[k];
            let end = starts[k + 1];
            chords.push(
                json!({ "start": round3(start), "end": round3(end), "label": symbol.name() }),
            );
            let inversion = rng.int(0, symbol.intervals.len() as i32 - 1) as usize;
            let lo = rng.int(55, 62);
            let notes = voicing(symbol, lo, inversion);
            let vel = rng.int(70, 96);
            let hold = end - 0.02;
            match pattern {
                0 => {
                    for &k in &notes {
                        score.note(CH_COMP, k, vel, start, hold);
                    }
                }
                1 | 2 => {
                    let step = if pattern == 1 { beat } else { beat * 0.5 };
                    let mut s = start;
                    while s < end - 1e-6 {
                        let e = (s + step * 0.9).min(hold);
                        for &k in &notes {
                            score.note(CH_COMP, k, vel - rng.int(0, 12), s, e);
                        }
                        s += step;
                    }
                }
                3 => {
                    let step = beat * 0.5;
                    let mut s = start;
                    let mut j = 0;
                    while s < end - 1e-6 {
                        let k = notes[j % notes.len()]
                            + if (j / notes.len()) % 2 == 1 { 12 } else { 0 };
                        score.note(CH_COMP, k, vel, s, hold.min(s + beat * 1.5));
                        s += step;
                        j += 1;
                    }
                }
                _ => {
                    let mut s = start;
                    let mut j = 0;
                    while s < end - 1e-6 {
                        for (n, &k) in notes.iter().enumerate() {
                            let strum = n as f64 * 0.012;
                            score.note(
                                CH_COMP,
                                k,
                                vel - (j % 2) * 10,
                                s + strum,
                                hold.min(s + beat * 1.9),
                            );
                        }
                        s += if j % 2 == 0 { beat * 1.5 } else { beat * 0.5 };
                        j += 1;
                    }
                }
            }
            if layer_program.is_some() {
                for &k in &notes {
                    score.note(CH_LAYER, k + 12, vel - 20, start, hold);
                }
            }
            if bass_on {
                // Bass on the chord's bass note, nearest the last one.
                let pc = symbol.bass as i32;
                let mut key = 28 + (pc - 28).rem_euclid(12);
                if (key + 12 - last_bass).abs() < (key - last_bass).abs() && key + 12 <= 45 {
                    key += 12;
                }
                last_bass = key;
                let fifth = key + if symbol.intervals.contains(&7) { 7 } else { 0 };
                let mut s = start;
                let mut j = 0;
                while s < end - 1e-6 {
                    let k = if j % 4 == 2 && rng.chance(0.3) {
                        fifth
                    } else {
                        key
                    };
                    let len = if groove == 1 { beat * 0.45 } else { beat * 0.9 };
                    score.note(CH_BASS, k, rng.int(85, 105), s, (s + len).min(hold));
                    s += beat;
                    j += 1;
                }
            }
            if lead_on {
                // Chord tones on beats, scale passing tones on off-beats;
                // legato holds each note to the next, and now and then over
                // the chord change.
                let chord_pcs: Vec<i32> =
                    symbol.pitch_classes().iter().map(|&p| p as i32).collect();
                let mut s = start;
                let mut j = 0;
                let mut last = 72;
                while s < end - 1e-6 {
                    let step = if rng.chance(0.5) { beat * 0.5 } else { beat };
                    if rng.chance(0.8) {
                        let pcs: Vec<i32> = if j % 2 == 0 {
                            chord_pcs.clone()
                        } else {
                            scale.iter().map(|d| (key_root as i32 + d) % 12).collect()
                        };
                        let k = (64..=81)
                            .filter(|k| pcs.contains(&(k % 12)))
                            .min_by_key(|k| ((k - last).abs(), rng.next() % 3))
                            .unwrap_or(last);
                        last = k;
                        let until = if legato {
                            let over = if s + step >= end - 1e-6 && rng.chance(0.4) {
                                beat
                            } else {
                                0.0
                            };
                            s + step + over
                        } else {
                            (s + step * 0.95).min(end)
                        };
                        score.note(CH_LEAD, k, rng.int(75, 105), s, until);
                    }
                    s += step;
                    j += 1;
                }
            }
            t = end;
        }
    }
    let chords_end = t;
    // Riff: a two-bar eighth-note ostinato on the key's pentatonic scale.
    if riff_program.is_some() {
        let pent: [i32; 5] = if minor {
            [0, 3, 5, 7, 10]
        } else {
            [0, 2, 4, 7, 9]
        };
        let motif: Vec<Option<i32>> = (0..16)
            .map(|_| rng.chance(0.75).then(|| pent[rng.int(0, 4) as usize]))
            .collect();
        let mut s = t0;
        let mut j = 0;
        while s < chords_end - 1e-6 {
            if let Some(d) = motif[j % 16] {
                let key = 72 + (key_root as i32 + d) % 12;
                score.note(CH_RIFF, key, rng.int(60, 85), s, s + beat * 0.45);
            }
            s += beat * 0.5;
            j += 1;
        }
    }
    // Pedal: tonic and fifth held under the whole progression.
    if pedal {
        let tonic = 48 + key_root as i32 % 12;
        score.note(CH_PEDAL, tonic, 70, t0, chords_end);
        score.note(CH_PEDAL, tonic + 7, 60, t0, chords_end);
    }
    // Drums from the start of the intro to the end of the last chord.
    if drums {
        let mut s = lead_in;
        let mut b = 0usize;
        while s < chords_end - 1e-6 {
            let pos = b % 4;
            let kick = match groove {
                0 => pos == 0 || pos == 2,
                1 => true,
                2 => pos == 0,
                _ => pos == 0 || (pos == 2 && b % 8 == 2),
            };
            let snare = match groove {
                2 => pos == 2,
                _ => pos == 1 || pos == 3,
            };
            if kick {
                score.note(DRUMS, 36, 110, s, s + 0.1);
            }
            if snare {
                score.note(DRUMS, 38, 100, s, s + 0.1);
            }
            score.note(DRUMS, 42, 75, s, s + 0.05);
            score.note(
                DRUMS,
                if groove == 1 { 46 } else { 42 },
                60,
                s + beat * 0.5,
                s + beat * 0.5 + 0.05,
            );
            if b % 16 == 0 {
                score.note(DRUMS, 49, 90, s, s + 0.5);
            }
            s += beat;
            b += 1;
        }
    }
    let tail = 1.5;
    chords.push(
        json!({ "start": round3(chords_end), "end": round3(chords_end + tail), "label": "N" }),
    );
    let total = chords_end + tail;

    // Render.
    let mut settings = SynthesizerSettings::new(SAMPLE_RATE);
    settings.enable_reverb_and_chorus = true;
    let mut synth = Synthesizer::new(font, &settings).map_err(|e| format!("{e:?}"))?;
    score
        .events
        .sort_by(|a, b| a.seconds.total_cmp(&b.seconds).then(a.order.cmp(&b.order)));
    let frames = (total * SAMPLE_RATE as f64) as usize;
    let mut left = vec![0f32; frames];
    let mut right = vec![0f32; frames];
    let mut pos = 0usize;
    for ev in &score.events {
        let at = ((ev.seconds * SAMPLE_RATE as f64) as usize).min(frames);
        if at > pos {
            synth.render(&mut left[pos..at], &mut right[pos..at]);
            pos = at;
        }
        synth.process_midi_message(ev.channel, ev.command, ev.data1, ev.data2);
    }
    if pos < frames {
        synth.render(&mut left[pos..], &mut right[pos..]);
    }
    let audio: Vec<f32> = left
        .iter()
        .zip(&right)
        .map(|(l, r)| 0.5 * (l + r))
        .collect();
    let peak = audio.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-6);
    let audio = audio.iter().map(|v| v / peak * 0.8).collect();

    let names = |p: i32| match p {
        0 => "piano",
        4 => "e-piano",
        16 => "organ",
        24 => "nylon guitar",
        25 => "steel guitar",
        27 => "clean guitar",
        29 => "overdrive guitar",
        30 => "distortion guitar",
        48 | 49 => "strings",
        50 => "synth strings",
        52 => "choir",
        61 => "brass",
        89 | 91 => "pad",
        _ => "other",
    };
    let pattern_name = ["sustain", "quarters", "eighths", "arpeggio", "strum"][pattern as usize];
    let groove_name = ["backbeat", "four-on-floor", "half-time", "sparse"][groove as usize];
    Ok(Clip {
        audio,
        chords,
        key: format!(
            "{} {}",
            NAMES[key_root as usize],
            if minor { "minor" } else { "major" }
        ),
        bpm,
        detune_cents,
        arrangement: json!({
            "comp": names(comp_program),
            "pattern": pattern_name,
            "layer": layer_program.map(names),
            "bass": bass_on,
            "drums": drums.then_some(groove_name),
            "lead": lead_on.then_some(if legato { "legato" } else { "detached" }),
            "riff": riff_program.is_some(),
            "pedal": pedal,
            "anticipated": anticipate,
            "transpose": transpose,
        }),
    })
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn write_wav(path: &Path, audio: &[f32]) -> std::io::Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    let data_len = (audio.len() * 2) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&(SAMPLE_RATE as u32).to_le_bytes())?;
    w.write_all(&(SAMPLE_RATE as u32 * 2).to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for v in audio {
        w.write_all(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    w.flush()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut limit = usize::MAX;
    let mut seed = 1u64;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--limit" => limit = args.next().and_then(|v| v.parse().ok()).unwrap_or(limit),
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()).unwrap_or(seed),
            _ => positional.push(arg),
        }
    }
    let [catalog, soundfont, out] = &positional[..] else {
        eprintln!(
            "usage: chord_corpus <catalog.json> <soundfont.sf2> <out dir> [--limit N] [--seed S]"
        );
        std::process::exit(2);
    };
    let catalog: Value =
        serde_json::from_str(&std::fs::read_to_string(catalog).expect("read catalog"))
            .expect("catalog json");
    let font = Arc::new(
        SoundFont::new(&mut File::open(soundfont).expect("open soundfont"))
            .expect("parse soundfont"),
    );
    let out = PathBuf::from(out);
    std::fs::create_dir_all(out.join("audio")).expect("create out dir");

    let progressions = catalog["progressions"].as_array().expect("progressions");
    let jobs: Vec<&Value> = progressions.iter().take(limit).collect();
    let songs: Vec<Option<Value>> = std::thread::scope(|scope| {
        let chunk = jobs.len().div_ceil(8).max(1);
        let handles: Vec<_> = jobs
            .chunks(chunk)
            .map(|batch| {
                let font = font.clone();
                let out = out.clone();
                scope.spawn(move || {
                    batch
                        .iter()
                        .map(|p| {
                            let id = p["id"].as_str()?;
                            let parsed: Result<Vec<(Symbol, f64)>, String> = p["reference_chords"]
                                .as_array()?
                                .iter()
                                .zip(p["durations_beats"].as_array()?)
                                .map(|(c, d)| {
                                    let symbol = chord_symbols::parse(c.as_str().unwrap_or(""))?
                                        .ok_or_else(|| "no-chord in progression".to_string())?;
                                    Ok((symbol, d.as_f64().unwrap_or(4.0)))
                                })
                                .collect();
                            let progression = match parsed {
                                Ok(p) => p,
                                Err(e) => {
                                    eprintln!("skip {id}: {e}");
                                    return None;
                                }
                            };
                            let key_root =
                                chord_symbols::pitch_class(p["reference_key"].as_str()?)?;
                            let mode = p["modes"][0].as_str().unwrap_or("major");
                            let minor =
                                mode.contains("minor") || mode == "dorian" || mode == "phrygian";
                            let tempo = p["tempo_reference"].as_f64();
                            let clip = match render(
                                &font,
                                &progression,
                                key_root,
                                minor,
                                tempo,
                                hash(id) ^ seed,
                            ) {
                                Ok(c) => c,
                                Err(e) => {
                                    eprintln!("skip {id}: {e}");
                                    return None;
                                }
                            };
                            let file = format!("audio/{id}.wav");
                            write_wav(&out.join(&file), &clip.audio).ok()?;
                            let split = match hash(id) % 10 {
                                0..=5 => "dev",
                                6 | 7 => "val",
                                _ => "test",
                            };
                            Some(json!({
                                "file": file,
                                "key": clip.key,
                                "tempo_map": [{ "time": 0.0, "bpm": clip.bpm }],
                                "chords": clip.chords,
                                "split": split,
                                "tuning_cents": (clip.detune_cents * 10.0).round() / 10.0,
                                "arrangement": clip.arrangement,
                                "source": {
                                    "catalog_id": id,
                                    "corpus": p["corpus"],
                                    "license": p["provenance"]["license"],
                                },
                            }))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let songs: Vec<Value> = songs.into_iter().flatten().collect();
    let labels = json!({
        "description": "Synthetic chord-labelled corpus rendered by examples/chord_corpus.rs. Labels are exact for this rendered audio; the progressions come from the catalog's corpora (see each song's source.license) and the audio is for local evaluation only.",
        "songs": songs,
    });
    std::fs::write(
        out.join("labels.json"),
        serde_json::to_string_pretty(&labels).unwrap(),
    )
    .expect("write labels");
    let count = |s: &str| songs.iter().filter(|x| x["split"] == s).count();
    println!(
        "rendered {} clips (dev {}, val {}, test {}) into {}",
        songs.len(),
        count("dev"),
        count("val"),
        count("test"),
        out.display()
    );
}
