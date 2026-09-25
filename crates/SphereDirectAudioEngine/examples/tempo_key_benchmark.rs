//! Tempo & Key Finder benchmark.
//!
//! Runs the exact analysis the Tempo & Key Finder window runs
//! (`components/tempo_key_finder::run_analysis`: decode → mono →
//! `estimate_bpm_candidates`, `pitch_class_profile` + `rank_keys`,
//! `analyze_rhythm`, `recognize_chords`) over a labelled folder of songs and
//! scores it.
//!
//! ```text
//! cargo run --release -p sphere_directaudioengine --example tempo_key_benchmark -- \
//!     "<dataset dir>" [labels.json] [--json out.json] [--compare before.json] [--only <substring>]
//! ```
//!
//! The dataset directory holds the audio (it is not in the repo); the labels
//! default to `benchmarks/tempo_key_labels.json` beside this crate.
//!
//! # Labels
//!
//! One entry per song; only `file` and `key` are required.
//!
//! - `tempos`: tempos in playing order — one value for a steady song, the
//!   sequence for one that changes.
//! - `tempo_map`: timestamped tempo marks, `[{ "time": 0.0, "bpm": 92 }, …]`,
//!   each holding until the next. When present it also supplies `tempos`.
//! - `chords`: timestamped chord labels, `[{ "time": 0.0, "chord": "Am" }, …]`,
//!   each holding until the next; `"N"` is no chord.
//! - `meter`: free text, informational.
//!
//! # Scores
//!
//! - **Tempo** (steady songs): `exact` = within 2% of the label; `octave` =
//!   within 2% of the label × {1, 2, ½, 3/2, 2/3}. Scored for the top
//!   periodicity candidate, for the BPM the Finder shows, and for the rhythm
//!   analyser's `bpm` (the tempo map).
//! - **Tempo changes**: the ordered label sequence against the detected tempo
//!   sections, 4% tolerance: the share of label tempos found in order
//!   (longest common subsequence), octave-tolerant and at the exact metrical
//!   level; plus whether the song was flagged variable.
//! - **Timed tempo** (songs with `tempo_map`): the share of the labelled time
//!   whose detected section tempo matches the label (exact level / octave).
//! - **Key**: MIREX weighting — exact 1.0, fifth 0.5, relative 0.3,
//!   parallel 0.2, else 0.
//! - **Chords** (songs with `chords`): the share of labelled time whose
//!   detected chord has the label's root and major/minor quality (MIREX
//!   "majmin"; sevenths reduce to their triad, `N` must match `N`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value};
use SphereAudioProcessor::analysis::{
    analyze_rhythm, chroma_frames, key_correlations, recognize_chords, tempo_families, ChordKind,
    ChordOptions, ChordSegment, RhythmAnalysis, RhythmOptions,
};
use SphereAudioProcessor::{
    downmix_interleaved, estimate_bpm_candidates, pitch_class_profile, rank_keys, KeyMode,
    TempoCandidate,
};

/// Same search range as the Finder window (`tempo_key_finder::MIN_BPM`/`MAX_BPM`).
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;
const TEMPO_TOLERANCE: f32 = 0.02;
const SECTION_TOLERANCE: f32 = 0.04;

#[derive(Deserialize)]
struct Labels {
    songs: Vec<Song>,
}

#[derive(Deserialize, Clone)]
struct Song {
    file: String,
    key: String,
    #[serde(default)]
    tempos: Vec<f32>,
    #[serde(default)]
    tempo_map: Vec<TempoMark>,
    #[serde(default)]
    chords: Vec<ChordMark>,
}

#[derive(Deserialize, Clone, Copy)]
struct TempoMark {
    time: f64,
    bpm: f32,
}

#[derive(Deserialize, Clone)]
struct ChordMark {
    time: f64,
    chord: String,
}

impl Song {
    /// Tempos in playing order, from `tempos` or else the tempo map.
    fn tempo_sequence(&self) -> Vec<f32> {
        if self.tempos.is_empty() {
            self.tempo_map.iter().map(|m| m.bpm).collect()
        } else {
            self.tempos.clone()
        }
    }
}

fn pitch_class(name: &str) -> Option<u8> {
    let name = name.trim();
    let mut chars = name.chars();
    let letter = chars.next()?;
    let base = match letter.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let accidental: i32 = match chars.as_str() {
        "" => 0,
        "#" | "♯" => 1,
        "b" | "♭" => -1,
        _ => return None,
    };
    Some(((base + accidental).rem_euclid(12)) as u8)
}

/// `"G minor"` → `(7, minor)`.
fn parse_key(label: &str) -> Option<(u8, bool)> {
    let mut parts = label.split_whitespace();
    let tonic = pitch_class(parts.next()?)?;
    let minor = match parts.next()?.to_ascii_lowercase().as_str() {
        "major" | "maj" => false,
        "minor" | "min" => true,
        _ => return None,
    };
    Some((tonic, minor))
}

/// A chord label reduced to MIREX majmin: `Some((root, minor))`, or `None`
/// for no chord. Accepts `Am`, `F#m7`, `Bbmaj7`, `C7`, `A:min`, `N`, and a
/// slash bass (`C/E`, ignored). `Err` for anything else.
fn parse_chord(label: &str) -> Result<Option<(u8, bool)>, String> {
    let label = label.trim();
    if label.is_empty() || label.eq_ignore_ascii_case("n") {
        return Ok(None);
    }
    let body = label.split('/').next().unwrap_or(label);
    let (root_part, quality) = match body.find(':') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => {
            let split = body
                .char_indices()
                .skip(1)
                .find(|(_, c)| !matches!(c, '#' | 'b' | '♯' | '♭'))
                .map_or(body.len(), |(i, _)| i);
            (&body[..split], &body[split..])
        }
    };
    let root = pitch_class(root_part).ok_or_else(|| format!("bad chord root in {label:?}"))?;
    let minor =
        quality.starts_with("min") || (quality.starts_with('m') && !quality.starts_with("maj"));
    Ok(Some((root, minor)))
}

/// MIREX key score.
fn key_score(truth: (u8, bool), guess: (u8, bool)) -> (f32, &'static str) {
    let (t, t_minor) = truth;
    let (g, g_minor) = guess;
    if t == g && t_minor == g_minor {
        return (1.0, "exact");
    }
    if t_minor == g_minor && (g == (t + 7) % 12 || g == (t + 5) % 12) {
        return (0.5, "fifth");
    }
    // Relative: C major <-> A minor.
    if !t_minor && g_minor && g == (t + 9) % 12 {
        return (0.3, "relative");
    }
    if t_minor && !g_minor && g == (t + 3) % 12 {
        return (0.3, "relative");
    }
    if t == g && t_minor != g_minor {
        return (0.2, "parallel");
    }
    (0.0, "wrong")
}

const NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn key_name((tonic, minor): (u8, bool)) -> String {
    format!(
        "{} {}",
        NAMES[tonic as usize],
        if minor { "minor" } else { "major" }
    )
}

fn within(a: f32, b: f32, tolerance: f32) -> bool {
    b > 0.0 && ((a - b) / b).abs() <= tolerance
}

const OCTAVE_FACTORS: [f32; 5] = [1.0, 2.0, 0.5, 1.5, 2.0 / 3.0];

fn tempo_class(detected: f32, truth: f32) -> &'static str {
    if within(detected, truth, TEMPO_TOLERANCE) {
        "exact"
    } else if OCTAVE_FACTORS
        .iter()
        .any(|f| within(detected, truth * f, TEMPO_TOLERANCE))
    {
        "octave"
    } else {
        "wrong"
    }
}

fn tempo_matches(detected: f32, truth: f32, tolerance: f32, octave: bool) -> bool {
    if octave {
        OCTAVE_FACTORS
            .iter()
            .any(|f| within(detected, truth * f, tolerance))
    } else {
        within(detected, truth, tolerance)
    }
}

/// Longest common subsequence of the label tempos found, in order, in the
/// detected section tempos.
fn ordered_hits(truth: &[f32], detected: &[f32], octave: bool) -> usize {
    let mut table = vec![vec![0usize; detected.len() + 1]; truth.len() + 1];
    for i in 1..=truth.len() {
        for j in 1..=detected.len() {
            table[i][j] = if tempo_matches(detected[j - 1], truth[i - 1], SECTION_TOLERANCE, octave)
            {
                table[i - 1][j - 1] + 1
            } else {
                table[i - 1][j].max(table[i][j - 1])
            };
        }
    }
    table[truth.len()][detected.len()]
}

/// Collapse consecutive sections of (nearly) the same tempo, as a reader of
/// the tempo map would.
fn collapse(sections: &[f32]) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    for &bpm in sections {
        if out
            .last()
            .is_some_and(|&last| within(bpm, last, SECTION_TOLERANCE))
        {
            continue;
        }
        out.push(bpm);
    }
    out
}

/// Share of the labelled time `[marks[0].time, end)` whose detected section
/// tempo matches the mark in force: `(exact level, octave-tolerant)`.
fn timed_tempo_accuracy(marks: &[TempoMark], rhythm: &RhythmAnalysis, end: f64) -> (f32, f32) {
    let Some(first) = marks.first() else {
        return (0.0, 0.0);
    };
    let detected_at = |t: f64| {
        rhythm
            .sections
            .iter()
            .rev()
            .find(|s| s.start_seconds <= t)
            .or(rhythm.sections.first())
            .map_or(0.0, |s| s.bpm)
    };
    // Integrate on a 0.1 s grid: sections and marks are seconds apart.
    const STEP: f64 = 0.1;
    let (mut total, mut exact, mut octave) = (0.0, 0.0, 0.0);
    let mut t = first.time;
    while t < end {
        let mark = marks.iter().rev().find(|m| m.time <= t).unwrap_or(first);
        let bpm = detected_at(t);
        total += STEP;
        if tempo_matches(bpm, mark.bpm, SECTION_TOLERANCE, false) {
            exact += STEP;
        }
        if tempo_matches(bpm, mark.bpm, SECTION_TOLERANCE, true) {
            octave += STEP;
        }
        t += STEP;
    }
    if total <= 0.0 {
        return (0.0, 0.0);
    }
    ((exact / total) as f32, (octave / total) as f32)
}

/// MIREX majmin chord accuracy over the labelled time.
fn chord_accuracy(marks: &[ChordMark], detected: &[ChordSegment], end: f64) -> Result<f32, String> {
    let labels: Vec<(f64, Option<(u8, bool)>)> = marks
        .iter()
        .map(|m| parse_chord(&m.chord).map(|c| (m.time, c)))
        .collect::<Result<_, _>>()?;
    let Some(&(start, _)) = labels.first() else {
        return Ok(0.0);
    };
    let detected_at = |t: f64| {
        detected
            .iter()
            .find(|s| s.start_seconds <= t && t < s.end_seconds)
            .and_then(|s| s.chord)
            .map(|c| {
                let minor = matches!(c.kind, ChordKind::Minor | ChordKind::Minor7);
                (c.root % 12, minor)
            })
    };
    const STEP: f64 = 0.05;
    let (mut total, mut hit) = (0.0, 0.0);
    let mut t = start;
    while t < end {
        let label = labels
            .iter()
            .rev()
            .find(|(time, _)| *time <= t)
            .and_then(|l| l.1);
        total += STEP;
        if detected_at(t) == label {
            hit += STEP;
        }
        t += STEP;
    }
    Ok(if total > 0.0 {
        (hit / total) as f32
    } else {
        0.0
    })
}

/// What the Finder shows as the BPM: the most confident candidate
/// (`tempo_key_finder::best_tempo_index`).
fn finder_bpm(candidates: &[TempoCandidate]) -> Option<f32> {
    candidates
        .iter()
        .max_by(|a, b| a.confidence.total_cmp(&b.confidence))
        .map(|c| c.bpm)
}

/// Key diagnostics. Nothing here feeds the detector; it describes how
/// decisive the evidence was.
///
/// - the raw profile correlations of the best four keys and the top-1/top-2
///   margin (a small margin is a near tie, whatever the winner);
/// - the entropy of the pitch-class profile, normalised so 1.0 is perfectly
///   flat (no tonal centre to find); seven equally weighted scale notes
///   would read 0.78, and full mixes, which leak energy into every class,
///   sit around 0.9-0.95 — the flatter end often means a modulation;
/// - the tuning offset the chroma analysis removed (a recording near ±50
///   cents sits between two semitones and can read a semitone off);
/// - the key of every 30 s stretch, by the same profile method, to show
///   modulations or a key the whole-file profile averaged away.
fn key_diagnostics(
    profile: Option<&[f32; 12]>,
    mono: &[f32],
    sample_rate: f32,
    tuning: Option<f32>,
) -> Value {
    const SECTION_SECONDS: f64 = 30.0;
    let top = |profile: &[f32; 12], n: usize| -> Vec<(String, f32)> {
        key_correlations(profile)
            .into_iter()
            .take(n)
            .map(|(score, tonic, mode)| (key_name((tonic as u8, mode == KeyMode::Minor)), score))
            .collect()
    };
    let Some(profile) = profile else {
        return json!(null);
    };
    let best = top(profile, 4);
    let margin = best[0].1 - best[1].1;
    let entropy = -profile
        .iter()
        .filter(|&&p| p > 0.0)
        .map(|&p| p * p.ln())
        .sum::<f32>()
        / 12f32.ln();
    let step = (SECTION_SECONDS * sample_rate as f64) as usize;
    let sections: Vec<Value> = (0..mono.len())
        .step_by(step.max(1))
        .filter_map(|start| {
            let end = (start + step).min(mono.len());
            // A short tail holds too little to name a key.
            if (end - start) < step / 3 {
                return None;
            }
            let profile = pitch_class_profile(&mono[start..end], sample_rate)?;
            let (key, score) = top(&profile, 1).into_iter().next()?;
            Some(json!({
                "start": (start as f64 / sample_rate as f64).round(),
                "end": (end as f64 / sample_rate as f64).round(),
                "key": key,
                "correlation": round2(score as f64),
            }))
        })
        .collect();
    json!({
        "top": best.iter().map(|(key, score)| json!({ "key": key, "correlation": round2(*score as f64) })).collect::<Vec<_>>(),
        "margin": (margin as f64 * 1000.0).round() / 1000.0,
        "profile_entropy": round2(entropy as f64),
        "tuning_cents": tuning.map(|t| (t as f64 * 100.0).round()),
        "sections": sections,
    })
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[derive(Default)]
struct Outcome {
    json: Value,
    /// Steady songs: class of the top candidate, the Finder BPM, the tempo map.
    tempo_top: Option<&'static str>,
    tempo_finder: Option<&'static str>,
    tempo_rhythm: Option<&'static str>,
    change_recall: Option<f32>,
    change_exact_recall: Option<f32>,
    change_flagged: Option<bool>,
    timed: Option<(f32, f32)>,
    chords: Option<f32>,
    key_score: f32,
    key_class: &'static str,
}

fn analyze(dataset: &Path, song: &Song) -> Result<Outcome, String> {
    let path = dataset.join(&song.file);
    let buffer = DirectAudio::load_audio_file_for_edit(&path.to_string_lossy())?;
    let channels = buffer.channels.max(1);
    let sample_rate = buffer.sample_rate as f32;
    let mono = downmix_interleaved(&buffer.samples, channels);
    let seconds = mono.len() as f64 / sample_rate as f64;

    let tempos = estimate_bpm_candidates(&mono, sample_rate, MIN_BPM, MAX_BPM);
    let profile = pitch_class_profile(&mono, sample_rate);
    let keys = profile.as_ref().map(rank_keys).unwrap_or_default();
    let frames = chroma_frames(&mono, sample_rate);
    let rhythm = analyze_rhythm(
        &mono,
        sample_rate,
        frames.as_ref(),
        RhythmOptions::default(),
    );

    let truth_key = parse_key(&song.key).ok_or_else(|| format!("bad key label {}", song.key))?;
    let guess_key = keys
        .first()
        .map(|k| (k.tonic as u8, k.mode == KeyMode::Minor));
    let (key_points, key_class) = guess_key
        .map(|g| key_score(truth_key, g))
        .unwrap_or((0.0, "none"));

    let sequence = song.tempo_sequence();
    if sequence.is_empty() {
        return Err("no tempo label".to_string());
    }
    let is_static = sequence.len() == 1;
    let top = tempos.first().map(|t| t.bpm);
    let finder = finder_bpm(&tempos);
    let sections: Vec<f32> = rhythm
        .as_ref()
        .map(|r| r.sections.iter().map(|s| s.bpm).collect())
        .unwrap_or_default();
    let collapsed = collapse(&sections);

    let mut outcome = Outcome {
        key_score: key_points,
        key_class,
        ..Default::default()
    };
    let class = |bpm: Option<f32>| bpm.map_or("none", |bpm| tempo_class(bpm, sequence[0]));
    if is_static {
        outcome.tempo_top = Some(class(top));
        outcome.tempo_finder = Some(class(finder));
        outcome.tempo_rhythm = Some(class(rhythm.as_ref().map(|r| r.bpm)));
    } else {
        let n = sequence.len() as f32;
        outcome.change_recall = Some(ordered_hits(&sequence, &collapsed, true) as f32 / n);
        outcome.change_exact_recall = Some(ordered_hits(&sequence, &collapsed, false) as f32 / n);
        outcome.change_flagged = Some(rhythm.as_ref().is_some_and(|r| r.variable));
    }
    if let (false, Some(r)) = (song.tempo_map.is_empty(), rhythm.as_ref()) {
        outcome.timed = Some(timed_tempo_accuracy(&song.tempo_map, r, seconds));
    }
    if !song.chords.is_empty() {
        let detected = match (&frames, &rhythm) {
            (Some(frames), Some(rhythm)) => {
                let beats: Vec<f64> = rhythm.beats.iter().map(|b| b.seconds).collect();
                let downbeats: Vec<bool> = rhythm.beats.iter().map(|b| b.position == 1).collect();
                recognize_chords(frames, &beats, &downbeats, ChordOptions { sevenths: true })
            }
            _ => Vec::new(),
        };
        outcome.chords = Some(chord_accuracy(&song.chords, &detected, seconds)?);
    }

    outcome.json = json!({
        "file": song.file,
        "seconds": round2(seconds),
        "key_truth": key_name(truth_key),
        "key_top": guess_key.map(key_name),
        "key_ranked": keys.iter().take(4).map(|k| json!({
            "key": k.display_label(),
            "confidence": round2(k.confidence as f64),
        })).collect::<Vec<_>>(),
        "key_class": key_class,
        "key_score": key_points,
        "key_diagnostics": key_diagnostics(
            profile.as_ref(),
            &mono,
            sample_rate,
            frames.as_ref().map(|f| f.tuning),
        ),
        "tempo_truth": sequence,
        "tempo_candidates": tempos.iter().map(|t| json!({
            "bpm": round2(t.bpm as f64),
            "confidence": round2(t.confidence as f64),
        })).collect::<Vec<_>>(),
        "candidate_families": tempo_families(&tempos).iter().map(|f| json!({
            "bpm": round2(f.canonical_bpm as f64),
            "other_levels": f.alternatives.iter().map(|h| round2(h.bpm as f64)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "tempo_top": top.map(|b| round2(b as f64)),
        "finder_bpm": finder.map(|b| round2(b as f64)),
        "rhythm_bpm": rhythm.as_ref().map(|r| round2(r.bpm as f64)),
        "rhythm_variable": rhythm.as_ref().map(|r| r.variable),
        "rhythm_confidence": rhythm.as_ref().map(|r| round2(r.confidence as f64)),
        "beats_per_bar": rhythm.as_ref().map(|r| r.beats_per_bar),
        "beat_count": rhythm.as_ref().map(|r| r.beats.len()),
        "sections": rhythm.as_ref().map(|r| r.sections.iter().enumerate().map(|(i, s)| json!({
            "start": (s.start_seconds * 10.0).round() / 10.0,
            "end": (s.end_seconds * 10.0).round() / 10.0,
            "bpm": round2(s.bpm as f64),
            "other_levels": r.families.get(i).map(|f| f.alternatives.iter().map(|h| json!({
                "bpm": round2(h.bpm as f64),
                "evidence": round2(h.evidence as f64),
                "score": round2(h.score as f64),
            })).collect::<Vec<_>>()),
        })).collect::<Vec<_>>()),
        "tracked_beat_count": rhythm.as_ref().map(|r| r.tracked_beats.len()),
        "sections_collapsed": collapsed.iter().map(|&b| round2(b as f64)).collect::<Vec<_>>(),
        "tempo_top_class": outcome.tempo_top,
        "tempo_finder_class": outcome.tempo_finder,
        "tempo_rhythm_class": outcome.tempo_rhythm,
        "change_recall": outcome.change_recall,
        "change_exact_recall": outcome.change_exact_recall,
        "timed_tempo": outcome.timed.map(|(e, o)| json!({ "exact": e, "octave": o })),
        "chord_majmin": outcome.chords,
    });
    Ok(outcome)
}

/// Before/after report against a JSON written by `--json`.
fn compare(before_path: &Path, after: &Value) {
    let before: Value = serde_json::from_str(
        &std::fs::read_to_string(before_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", before_path.display())),
    )
    .expect("before json");
    println!("\n==== COMPARE with {} ====", before_path.display());
    let metrics = [
        ("static", "top_exact"),
        ("static", "top_octave"),
        ("static", "finder_exact"),
        ("static", "finder_octave"),
        ("static", "rhythm_exact"),
        ("static", "rhythm_octave"),
        ("change", "flagged"),
        ("change", "mean_recall"),
        ("change", "mean_exact_recall"),
        ("key", "exact"),
        ("key", "mirex"),
    ];
    for (group, name) in metrics {
        let b = &before[group][name];
        let a = &after[group][name];
        let mark = match (b.as_f64(), a.as_f64()) {
            (Some(b), Some(a)) if a > b + 1e-9 => "  improved",
            (Some(b), Some(a)) if a < b - 1e-9 => "  REGRESSED",
            (None, _) | (_, None) => "  (not in both)",
            _ => "",
        };
        println!("{group:>6}.{name:<18} {b:>8} -> {a:<8}{mark}");
    }
    let index = |v: &Value| -> BTreeMap<String, Value> {
        v["songs"]
            .as_array()
            .map(|songs| {
                songs
                    .iter()
                    .map(|s| (s["file"].as_str().unwrap_or("").to_string(), s.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (b_songs, a_songs) = (index(&before), index(after));
    let fields = [
        "tempo_top",
        "finder_bpm",
        "rhythm_bpm",
        "sections_collapsed",
        "tempo_top_class",
        "tempo_finder_class",
        "tempo_rhythm_class",
        "change_recall",
        "change_exact_recall",
        "rhythm_variable",
        "beats_per_bar",
        "key_top",
        "key_class",
    ];
    println!("\nper-song changes:");
    let mut any = false;
    for (file, a) in &a_songs {
        let Some(b) = b_songs.get(file) else {
            println!("  {file}: new song");
            continue;
        };
        let changed: Vec<String> = fields
            .iter()
            .filter(|f| b[**f] != a[**f])
            .map(|f| format!("    {f}: {} -> {}", b[*f], a[*f]))
            .collect();
        if !changed.is_empty() {
            any = true;
            println!("  {}", file.rsplit('/').next().unwrap_or(file).trim());
            for line in changed {
                println!("{line}");
            }
        }
    }
    if !any {
        println!("  none");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut json_out: Option<PathBuf> = None;
    let mut compare_with: Option<PathBuf> = None;
    let mut only: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json_out = args.next().map(PathBuf::from),
            "--compare" => compare_with = args.next().map(PathBuf::from),
            "--only" => only = args.next(),
            _ => positional.push(arg),
        }
    }
    let Some(dataset) = positional.first().map(PathBuf::from) else {
        eprintln!(
            "usage: tempo_key_benchmark <dataset dir> [labels.json] [--json out.json] \
             [--compare before.json] [--only <substr>]"
        );
        std::process::exit(2);
    };
    let labels_path = positional.get(1).map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks/tempo_key_labels.json")
    });
    let labels: Labels = serde_json::from_str(
        &std::fs::read_to_string(&labels_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", labels_path.display())),
    )
    .expect("labels json");
    let songs: Vec<Song> = labels
        .songs
        .into_iter()
        .filter(|s| only.as_ref().is_none_or(|o| s.file.contains(o.as_str())))
        .collect();

    // One thread per song: the analysis is single-threaded and the songs are
    // independent.
    let started = Instant::now();
    let results: Vec<(Song, Result<Outcome, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = songs
            .iter()
            .map(|song| {
                let dataset = dataset.clone();
                let song = song.clone();
                scope.spawn(move || {
                    let outcome = analyze(&dataset, &song);
                    (song, outcome)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut static_n = 0u32;
    let (mut top_exact, mut top_octave) = (0u32, 0u32);
    let (mut finder_exact, mut finder_octave) = (0u32, 0u32);
    let (mut rhythm_exact, mut rhythm_octave) = (0u32, 0u32);
    let (mut change_n, mut change_flagged) = (0u32, 0u32);
    let (mut recall_sum, mut exact_recall_sum) = (0.0f32, 0.0f32);
    let (mut timed_n, mut timed_exact, mut timed_octave) = (0u32, 0.0f32, 0.0f32);
    let (mut chord_n, mut chord_sum) = (0u32, 0.0f32);
    let (mut key_n, mut key_sum, mut key_exact) = (0u32, 0.0f32, 0u32);
    let mut all_json = Vec::new();
    let hit = |class: &str| (class == "exact") as u32;
    let octave_ok = |class: &str| (class == "exact" || class == "octave") as u32;

    for (song, outcome) in &results {
        let name = song.file.rsplit('/').next().unwrap_or(&song.file).trim();
        let o = match outcome {
            Err(error) => {
                println!("ERROR  {name}: {error}");
                continue;
            }
            Ok(o) => o,
        };
        key_n += 1;
        key_sum += o.key_score;
        key_exact += (o.key_class == "exact") as u32;
        let j = &o.json;
        let key_line = format!(
            "  key   truth {} | ranked {} [{}]",
            j["key_truth"].as_str().unwrap_or(""),
            j["key_ranked"]
                .as_array()
                .map(|keys| keys
                    .iter()
                    .map(|k| format!("{} ({})", k["key"].as_str().unwrap_or(""), k["confidence"]))
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default(),
            o.key_class,
        );
        if let (Some(top), Some(finder), Some(rh)) = (o.tempo_top, o.tempo_finder, o.tempo_rhythm) {
            static_n += 1;
            top_exact += hit(top);
            top_octave += octave_ok(top);
            finder_exact += hit(finder);
            finder_octave += octave_ok(finder);
            rhythm_exact += hit(rh);
            rhythm_octave += octave_ok(rh);
            println!(
                "STATIC {name}\n  tempo truth {} | candidates {} | finder {} | rhythm {} [top {top} / finder {finder} / rhythm {rh}]\n{key_line}",
                j["tempo_truth"],
                j["tempo_candidates"]
                    .as_array()
                    .map(|c| c
                        .iter()
                        .map(|c| format!("{} ({})", c["bpm"], c["confidence"]))
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default(),
                j["finder_bpm"],
                j["rhythm_bpm"],
            );
        } else {
            change_n += 1;
            let recall = o.change_recall.unwrap_or(0.0);
            let exact_recall = o.change_exact_recall.unwrap_or(0.0);
            recall_sum += recall;
            exact_recall_sum += exact_recall;
            change_flagged += o.change_flagged.unwrap_or(false) as u32;
            println!(
                "CHANGE {name}\n  tempo truth {} | sections {} | variable {} | recall {:.0}% (exact level {:.0}%)\n{key_line}",
                j["tempo_truth"],
                j["sections_collapsed"],
                j["rhythm_variable"],
                recall * 100.0,
                exact_recall * 100.0,
            );
        }
        if let Some((exact, octave)) = o.timed {
            timed_n += 1;
            timed_exact += exact;
            timed_octave += octave;
            println!(
                "  timed tempo: {:.0}% of labelled time exact, {:.0}% octave-ok",
                exact * 100.0,
                octave * 100.0
            );
        }
        if let Some(accuracy) = o.chords {
            chord_n += 1;
            chord_sum += accuracy;
            println!("  chords majmin {:.0}%", accuracy * 100.0);
        }
        let diag = &j["key_diagnostics"];
        if !diag.is_null() {
            let list = |v: &Value, field: &str| {
                v.as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|i| format!("{} {}", i["key"].as_str().unwrap_or(""), i[field]))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default()
            };
            println!(
                "  key diag: {} | margin {} | profile entropy {} (1 = flat) | tuning {} cents",
                list(&diag["top"], "correlation"),
                diag["margin"],
                diag["profile_entropy"],
                diag["tuning_cents"],
            );
            let sections: Vec<String> = diag["sections"]
                .as_array()
                .map(|s| {
                    s.iter()
                        .map(|s| {
                            format!(
                                "{}-{}s {}",
                                s["start"],
                                s["end"],
                                s["key"].as_str().unwrap_or("")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            println!("  key by 30 s: {}", sections.join(" | "));
        }
        all_json.push(o.json.clone());
    }

    let mean = |sum: f32, n: u32| if n > 0 { sum / n as f32 } else { 0.0 };
    println!("\n==== SUMMARY ====");
    println!(
        "static tempo  ({static_n}): top candidate exact {top_exact}/{static_n}, octave-ok {top_octave}/{static_n} | finder bpm exact {finder_exact}/{static_n}, octave-ok {finder_octave}/{static_n} | rhythm bpm exact {rhythm_exact}/{static_n}, octave-ok {rhythm_octave}/{static_n}"
    );
    if change_n > 0 {
        println!(
            "tempo changes ({change_n}): flagged variable {change_flagged}/{change_n}, mean ordered recall {:.0}% (exact level {:.0}%)",
            mean(recall_sum, change_n) * 100.0,
            mean(exact_recall_sum, change_n) * 100.0,
        );
    }
    if timed_n > 0 {
        println!(
            "timed tempo   ({timed_n}): exact {:.0}%, octave-ok {:.0}% of labelled time",
            mean(timed_exact, timed_n) * 100.0,
            mean(timed_octave, timed_n) * 100.0
        );
    }
    if key_n > 0 {
        println!(
            "key           ({key_n}): exact {key_exact}/{key_n}, MIREX score {:.3}",
            mean(key_sum, key_n)
        );
    }
    if chord_n > 0 {
        println!(
            "chords        ({chord_n}): majmin {:.1}% of labelled time",
            mean(chord_sum, chord_n) * 100.0
        );
    } else {
        println!("chords: no chord labels in this dataset, not scored");
    }
    println!("({:.1} s)", started.elapsed().as_secs_f32());

    let summary = json!({
        "static": {
            "n": static_n,
            "top_exact": top_exact, "top_octave": top_octave,
            "finder_exact": finder_exact, "finder_octave": finder_octave,
            "rhythm_exact": rhythm_exact, "rhythm_octave": rhythm_octave,
        },
        "change": {
            "n": change_n,
            "flagged": change_flagged,
            "mean_recall": mean(recall_sum, change_n),
            "mean_exact_recall": mean(exact_recall_sum, change_n),
        },
        "timed": { "n": timed_n, "exact": mean(timed_exact, timed_n), "octave": mean(timed_octave, timed_n) },
        "key": { "n": key_n, "exact": key_exact, "mirex": mean(key_sum, key_n) },
        "chords": { "n": chord_n, "majmin": mean(chord_sum, chord_n) },
        "songs": all_json,
    });
    if let Some(path) = json_out {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&summary).unwrap() + "\n",
        )
        .expect("write json");
        println!("wrote {}", path.display());
    }
    if let Some(path) = compare_with {
        compare(&path, &summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_labels_reduce_to_majmin() {
        assert_eq!(parse_chord("Am"), Ok(Some((9, true))));
        assert_eq!(parse_chord("F"), Ok(Some((5, false))));
        assert_eq!(parse_chord("Bbmaj7"), Ok(Some((10, false))));
        assert_eq!(parse_chord("F#m7"), Ok(Some((6, true))));
        assert_eq!(parse_chord("C/E"), Ok(Some((0, false))));
        assert_eq!(parse_chord("A:min"), Ok(Some((9, true))));
        assert_eq!(parse_chord("N"), Ok(None));
        assert!(parse_chord("H7").is_err());
    }

    #[test]
    fn ordered_recall_separates_octave_from_exact_level() {
        let truth = [85.0, 90.0, 85.0];
        let detected = [170.0, 180.0, 170.0];
        assert_eq!(ordered_hits(&truth, &detected, true), 3);
        assert_eq!(ordered_hits(&truth, &detected, false), 0);
    }

    #[test]
    fn labels_accept_a_timestamped_tempo_map_and_chords() {
        let labels: Labels = serde_json::from_str(
            r#"{ "songs": [ { "file": "a.wav", "key": "A minor",
                "tempo_map": [ { "time": 0.0, "bpm": 92 }, { "time": 31.4, "bpm": 82 } ],
                "chords": [ { "time": 0.0, "chord": "Am" }, { "time": 2.667, "chord": "F" } ] } ] }"#,
        )
        .unwrap();
        assert_eq!(labels.songs[0].tempo_sequence(), vec![92.0, 82.0]);
        assert_eq!(labels.songs[0].chords.len(), 2);
    }
}
