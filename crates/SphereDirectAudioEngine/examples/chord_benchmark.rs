//! Chord benchmark: runs the Tempo & Key Finder's chord analysis over a
//! labelled folder and scores it against timestamped chord labels.
//!
//! ```text
//! cargo run --release -p sphere_directaudioengine --example chord_benchmark -- \
//!     <dataset dir> <labels.json> [--split dev|val|test|all] [--tolerance 0.1]
//!     [--json out.json] [--compare before.json] [--only <substring>] [--verbose]
//! ```
//!
//! Labels use the tempo/key benchmark schema. Chords are optional: either
//! `[{ "start": 0.0, "end": 1.48, "label": "Em" }, …]` or
//! `[{ "time": 0.0, "chord": "Em" }, …]` (each mark holding until the next,
//! the last until the end of the audio). Songs without chord labels get no
//! accuracy — only behaviour proxies (segments per minute, how much of the
//! song the most common chord takes), which describe the output, not its
//! correctness.
//!
//! Scores are duration-weighted over the labelled time, sampled every 10 ms:
//!
//! - **root**: the estimated root equals the reference root (reference
//!   no-chord time excluded; an estimated N is wrong);
//! - **majmin** (MIREX): chords reduced to maj/min, N counted as a class;
//!   reference chords that are neither (dim, aug, sus, power) are excluded;
//! - **sevenths** (MIREX): maj, min, 7, maj7, min7 and N;
//! - **full**: same root and the same set of chord tones (bass ignored);
//! - **N**: share of reference no-chord time estimated as N, and share of
//!   reference chord time wrongly estimated as N;
//! - **segmentation**: over/under-segmentation (1 − directional Hamming
//!   distance, as mir_eval), boundary MAE (each reference boundary to the
//!   nearest estimated one, capped at 1 s) and boundary recall/precision
//!   within `--tolerance` (evaluation only: estimated boundaries are never
//!   moved);
//! - **confidence reliability**: per confidence tenth, how much of the
//!   segments' time is root and maj/min correct.
//!
//! Detector switches: `--triads` (no sevenths), `--no-extended` (no sus,
//! dim, aug), `--key-prior` (the detected key as a soft prior). `--verbose`
//! adds a diagnostic dump per song: every segment with its candidates and
//! confidence, every harmonic span with its chroma, bass note and
//! candidates, plus tuning and harmonic rhythm against the beat grid.
//!
//! A labelled corpus can be rendered with `examples/chord_corpus.rs`; use
//! its `split` field to tune on `dev` only.

#[path = "support/chord_symbols.rs"]
mod chord_symbols;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chord_symbols::Symbol;
use serde::Deserialize;
use serde_json::{json, Value};
use SphereAudioProcessor::analysis::{
    analyze_chords, analyze_rhythm, chroma_frames, ChordLabel, ChordOptions, KeyContext,
    RhythmOptions,
};
use SphereAudioProcessor::{downmix_interleaved, pitch_class_profile, rank_keys, KeyMode};

const STEP: f64 = 0.01;
const BOUNDARY_CAP: f64 = 1.0;

#[derive(Deserialize)]
struct Labels {
    songs: Vec<Song>,
}

#[derive(Deserialize, Clone)]
struct Song {
    file: String,
    #[serde(default)]
    split: Option<String>,
    #[serde(default)]
    chords: Vec<Value>,
}

/// A reference or estimated chord span.
#[derive(Clone, Debug)]
struct Span {
    start: f64,
    end: f64,
    chord: Option<Symbol>,
}

fn reference(song: &Song, duration: f64) -> Result<Vec<Span>, String> {
    let mut spans = Vec::new();
    for (i, c) in song.chords.iter().enumerate() {
        let label = c["label"]
            .as_str()
            .or_else(|| c["chord"].as_str())
            .ok_or("chord entry without label")?;
        let start = c["start"]
            .as_f64()
            .or_else(|| c["time"].as_f64())
            .ok_or("chord entry without start/time")?;
        let end = c["end"].as_f64().unwrap_or_else(|| {
            song.chords
                .get(i + 1)
                .and_then(|n| n["start"].as_f64().or_else(|| n["time"].as_f64()))
                .unwrap_or(duration)
        });
        spans.push(Span {
            start,
            end,
            chord: chord_symbols::parse(label)?,
        });
    }
    Ok(spans)
}

/// The detector's label as a symbol.
fn estimated_symbol(label: Option<ChordLabel>) -> Option<Symbol> {
    chord_symbols::parse(&label?.name()).ok().flatten()
}

/// Detector settings under test.
#[derive(Clone, Copy)]
struct Setup {
    sevenths: bool,
    extended: bool,
    key_prior: bool,
}

fn at(spans: &[Span], t: f64) -> Option<&Span> {
    spans.iter().find(|s| s.start <= t && t < s.end)
}

#[derive(Default, Clone, Copy)]
struct Tally {
    hit: f64,
    total: f64,
}

impl Tally {
    fn add(&mut self, counted: bool, correct: bool) {
        if counted {
            self.total += STEP;
            if correct {
                self.hit += STEP;
            }
        }
    }
    fn rate(&self) -> Option<f64> {
        (self.total > 0.0).then(|| self.hit / self.total)
    }
}

#[derive(Default, Clone, Copy)]
struct Scores {
    root: Tally,
    majmin: Tally,
    sevenths: Tally,
    full: Tally,
    n_recall: Tally,
    false_n: Tally,
}

fn score(reference: &[Span], estimate: &[Span]) -> Scores {
    let mut s = Scores::default();
    let (Some(first), Some(last)) = (reference.first(), reference.last()) else {
        return s;
    };
    let mut t = first.start;
    while t < last.end {
        let r = at(reference, t).and_then(|s| s.chord.as_ref());
        let e = at(estimate, t).and_then(|s| s.chord.as_ref());
        match r {
            None => {
                s.n_recall.add(true, e.is_none());
                s.majmin.add(true, e.is_none());
                s.sevenths.add(true, e.is_none());
                s.full.add(true, e.is_none());
            }
            Some(r) => {
                s.false_n.add(true, e.is_none());
                s.root.add(true, e.is_some_and(|e| e.root == r.root));
                if let Some(minor) = r.majmin() {
                    s.majmin.add(
                        true,
                        e.is_some_and(|e| e.root == r.root && e.majmin() == Some(minor)),
                    );
                }
                if let Some(q) = r.sevenths() {
                    s.sevenths.add(
                        true,
                        e.is_some_and(|e| e.root == r.root && e.sevenths() == Some(q)),
                    );
                }
                let tones = |x: &Symbol| {
                    let mut v = x.pitch_classes();
                    v.sort();
                    v
                };
                s.full.add(
                    true,
                    e.is_some_and(|e| e.root == r.root && tones(e) == tones(r)),
                );
            }
        }
        t += STEP;
    }
    s
}

/// 1 − directional Hamming distance of `a` against `b`: how much of each
/// segment of `a` lies in the single segment of `b` it overlaps most.
fn directional(a: &[Span], b: &[Span]) -> f64 {
    let total: f64 = a.iter().map(|s| s.end - s.start).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let covered: f64 = a
        .iter()
        .map(|s| {
            b.iter()
                .map(|o| (s.end.min(o.end) - s.start.max(o.start)).max(0.0))
                .fold(0.0, f64::max)
        })
        .sum();
    covered / total
}

/// Boundaries between different chords (merged runs), inside the song.
fn boundaries(spans: &[Span]) -> Vec<f64> {
    spans
        .windows(2)
        .filter(|w| w[0].chord != w[1].chord)
        .map(|w| w[1].start)
        .collect()
}

fn merge(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for s in spans {
        if let Some(last) = out.last_mut() {
            if last.chord == s.chord && (s.start - last.end).abs() < 1e-6 {
                last.end = s.end;
                continue;
            }
        }
        out.push(s);
    }
    out
}

struct Outcome {
    /// Per confidence tenth: (correct seconds, seconds).
    reliability: [(f64, f64); 10],
    json: Value,
    scores: Option<Scores>,
    seg: Option<(f64, f64)>,
    boundary: Option<(f64, usize, usize, usize)>,
    seconds: f64,
    analysis_ms: u128,
}

fn analyze(
    dataset: &Path,
    song: &Song,
    tolerance: f64,
    verbose: bool,
    setup: Setup,
) -> Result<Outcome, String> {
    let buffer =
        DirectAudio::load_audio_file_for_edit(&dataset.join(&song.file).to_string_lossy())?;
    let sample_rate = buffer.sample_rate as f32;
    let mono = downmix_interleaved(&buffer.samples, buffer.channels.max(1));
    let seconds = mono.len() as f64 / sample_rate as f64;

    // The Finder's chord path (tempo_key_finder::run_analysis).
    let started = Instant::now();
    let frames = chroma_frames(&mono, sample_rate);
    let rhythm = analyze_rhythm(
        &mono,
        sample_rate,
        frames.as_ref(),
        RhythmOptions::default(),
    );
    // The key the Finder shows, for the optional key prior.
    let key = pitch_class_profile(&mono, sample_rate)
        .as_ref()
        .map(rank_keys)
        .and_then(|k| k.first().copied());
    let options = ChordOptions {
        sevenths: setup.sevenths,
        extended: setup.extended,
        key: if setup.key_prior {
            key.map(|k| KeyContext {
                tonic: k.tonic as u8,
                minor: k.mode == KeyMode::Minor,
            })
        } else {
            None
        },
    };
    let analysis = match (&frames, &rhythm) {
        (Some(frames), Some(rhythm)) => {
            let beats: Vec<f64> = rhythm.beats.iter().map(|b| b.seconds).collect();
            let downbeats: Vec<bool> = rhythm.beats.iter().map(|b| b.position == 1).collect();
            Some(analyze_chords(frames, &beats, &downbeats, options))
        }
        (Some(frames), None) => Some(analyze_chords(frames, &[], &[], options)),
        _ => None,
    };
    let analysis_ms = started.elapsed().as_millis();
    let segments = analysis
        .as_ref()
        .map(|a| a.segments.clone())
        .unwrap_or_default();

    let estimate = merge(
        segments
            .iter()
            .map(|s| Span {
                start: s.start_seconds,
                end: s.end_seconds,
                chord: estimated_symbol(s.chord),
            })
            .collect(),
    );
    let name = |s: &Option<Symbol>| s.as_ref().map_or("N".to_string(), Symbol::name);

    // Behaviour proxies, for every song.
    let mut time_per: BTreeMap<String, f64> = BTreeMap::new();
    for s in &estimate {
        *time_per.entry(name(&s.chord)).or_default() += s.end - s.start;
    }
    let mut top: Vec<(String, f64)> = time_per.into_iter().collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    let covered: f64 = estimate
        .iter()
        .map(|s| s.end - s.start)
        .sum::<f64>()
        .max(1e-9);
    let top_share = top.first().map_or(0.0, |t| t.1 / covered);
    let segments_per_minute = estimate.len() as f64 / (seconds / 60.0).max(1e-9);

    let mut json = json!({
        "file": song.file,
        "split": song.split,
        "seconds": (seconds * 100.0).round() / 100.0,
        "analysis_ms": analysis_ms,
        "detected_segments": estimate.len(),
        "segments_per_minute": (segments_per_minute * 10.0).round() / 10.0,
        "top_chord_share": (top_share * 1000.0).round() / 1000.0,
        "top_chords": top.iter().take(6).map(|(c, t)| json!([c, (t * 10.0).round() / 10.0])).collect::<Vec<_>>(),
    });
    json["tuning_cents"] = json!(analysis
        .as_ref()
        .map(|a| (a.tuning_cents * 10.0).round() / 10.0));
    json["key"] = json!(key.map(|k| k.display_label()));
    // Harmonic rhythm against the beat grid: chord lengths in detected
    // beats, and where changes land. Diagnostic for tempo/chord mutual
    // information (e.g. chords changing every 8 detected beats at a
    // double-time reading).
    if let Some(rhythm) = &rhythm {
        let beat_times: Vec<f64> = rhythm.beats.iter().map(|b| b.seconds).collect();
        let nearest_beat = |t: f64| {
            beat_times
                .iter()
                .enumerate()
                .min_by(|a, b| (a.1 - t).abs().total_cmp(&(b.1 - t).abs()))
                .map(|(i, b)| (i, (b - t).abs()))
        };
        let mut lengths: Vec<f64> = estimate
            .iter()
            .filter(|s| s.chord.is_some())
            .filter_map(|s| {
                let (a, _) = nearest_beat(s.start)?;
                let (b, _) = nearest_beat(s.end)?;
                (b > a).then(|| (b - a) as f64)
            })
            .collect();
        lengths.sort_by(|a, b| a.total_cmp(b));
        let changes: Vec<(usize, f64)> = estimate
            .windows(2)
            .filter_map(|w| nearest_beat(w[1].start))
            .collect();
        let period = 60.0 / rhythm.bpm.max(1.0) as f64;
        let on_beat = changes.iter().filter(|(_, d)| *d < 0.25 * period).count();
        let on_downbeat = changes
            .iter()
            .filter(|(i, d)| *d < 0.25 * period && rhythm.beats[*i].position == 1)
            .count();
        let share = |n: usize| {
            (!changes.is_empty()).then(|| (n as f64 / changes.len() as f64 * 100.0).round())
        };
        json["harmonic_rhythm"] = json!({
            "bpm": (rhythm.bpm * 10.0).round() / 10.0,
            "beats_per_bar": rhythm.beats_per_bar,
            "median_chord_beats": lengths.get(lengths.len() / 2),
            "changes_on_beat": share(on_beat),
            "changes_on_downbeat": share(on_downbeat),
        });
    }
    if verbose {
        // Diagnostic dump: every segment with its candidates, and every
        // harmonic span with its chroma, bass and candidates.
        let label = |c: &Option<ChordLabel>| c.map_or("N".to_string(), |c| c.name());
        if let Some(a) = &analysis {
            json["segments"] = a
                .segments
                .iter()
                .zip(&a.segment_candidates)
                .map(|(s, c)| {
                    json!({
                        "start": (s.start_seconds * 1000.0).round() / 1000.0,
                        "end": (s.end_seconds * 1000.0).round() / 1000.0,
                        "chord": label(&s.chord),
                        "confidence": (s.confidence * 100.0).round() / 100.0,
                        "candidates": c.iter().map(|(l, e)| json!([label(l), (e * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json["spans"] = a
                .spans
                .iter()
                .map(|s| {
                    let bass_pc = (0..12).max_by(|&x, &y| s.bass[x].total_cmp(&s.bass[y]));
                    json!({
                        "start": (s.start_seconds * 1000.0).round() / 1000.0,
                        "end": (s.end_seconds * 1000.0).round() / 1000.0,
                        "downbeat": s.downbeat,
                        "chroma": s.chroma.iter().map(|v| (v * 100.0).round() / 100.0).collect::<Vec<_>>(),
                        "bass_pc": bass_pc.filter(|&p| s.bass[p] > 0.0).map(|p| chord_symbols::NAMES[p]),
                        "candidates": s.candidates.iter().take(3).map(|(l, e)| json!([label(l), (e * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
                    })
                })
                .collect();
        }
    }

    if song.chords.is_empty() {
        return Ok(Outcome {
            reliability: [(0.0, 0.0); 10],
            json,
            scores: None,
            seg: None,
            boundary: None,
            seconds,
            analysis_ms,
        });
    }
    let reference = merge(reference(song, seconds)?);
    let scores = score(&reference, &estimate);
    // Confidence reliability: per confidence tenth, the time the segment's
    // chord is majmin-correct (root and maj/min), over segments naming a
    // chord.
    let mut reliability = [(0.0f64, 0.0f64); 10];
    for seg in &segments {
        let Some(e) = estimated_symbol(seg.chord) else {
            continue;
        };
        let bin = ((seg.confidence * 10.0) as usize).min(9);
        let mut t = seg.start_seconds;
        while t < seg.end_seconds {
            if let Some(r) = at(&reference, t) {
                reliability[bin].1 += STEP;
                let ok = r.chord.as_ref().is_some_and(|r| {
                    r.root == e.root && r.majmin().is_none_or(|m| e.majmin() == Some(m))
                });
                if ok {
                    reliability[bin].0 += STEP;
                }
            }
            t += STEP;
        }
    }
    // As mir_eval: over-segmentation looks at how the estimate splits each
    // reference segment, under-segmentation the other way round.
    let over = directional(&reference, &estimate);
    let under = directional(&estimate, &reference);
    let (rb, eb) = (boundaries(&reference), boundaries(&estimate));
    let nearest = |t: f64, set: &[f64]| {
        set.iter()
            .map(|b| (b - t).abs())
            .fold(BOUNDARY_CAP, f64::min)
    };
    let mae = if rb.is_empty() {
        0.0
    } else {
        rb.iter().map(|&t| nearest(t, &eb)).sum::<f64>() / rb.len() as f64
    };
    let hits = rb.iter().filter(|&&t| nearest(t, &eb) <= tolerance).count();
    let est_hits = eb.iter().filter(|&&t| nearest(t, &rb) <= tolerance).count();
    let rate = |t: Tally| t.rate().map(|r| (r * 1000.0).round() / 10.0);
    json["reference_segments"] = json!(reference.len());
    json["root"] = json!(rate(scores.root));
    json["majmin"] = json!(rate(scores.majmin));
    json["sevenths"] = json!(rate(scores.sevenths));
    json["full"] = json!(rate(scores.full));
    json["n_recall"] = json!(rate(scores.n_recall));
    json["false_n"] = json!(rate(scores.false_n));
    json["under_seg"] = json!((under * 1000.0).round() / 10.0);
    json["over_seg"] = json!((over * 1000.0).round() / 10.0);
    json["boundary_mae_ms"] = json!((mae * 1000.0).round());
    json["boundary_recall"] = json!(if rb.is_empty() {
        None
    } else {
        Some((hits as f64 / rb.len() as f64 * 1000.0).round() / 10.0)
    });
    json["boundary_precision"] = json!(if eb.is_empty() {
        None
    } else {
        Some((est_hits as f64 / eb.len() as f64 * 1000.0).round() / 10.0)
    });
    Ok(Outcome {
        reliability,
        json,
        scores: Some(scores),
        seg: Some((over, under)),
        boundary: Some((mae * rb.len() as f64, rb.len(), hits, eb.len().max(0))),
        seconds,
        analysis_ms,
    })
}

fn pct(v: Option<f64>) -> String {
    v.map_or("  —  ".into(), |v| format!("{:5.1}%", v * 100.0))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let (mut json_out, mut compare_with, mut only) =
        (None::<PathBuf>, None::<PathBuf>, None::<String>);
    let mut split = "all".to_string();
    let mut tolerance = 0.1;
    let mut verbose = false;
    let mut setup = Setup {
        sevenths: true,
        extended: true,
        key_prior: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json_out = args.next().map(PathBuf::from),
            "--compare" => compare_with = args.next().map(PathBuf::from),
            "--only" => only = args.next(),
            "--split" => split = args.next().unwrap_or(split),
            "--tolerance" => {
                tolerance = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(tolerance)
            }
            "--verbose" => verbose = true,
            "--triads" => setup.sevenths = false,
            "--no-extended" => setup.extended = false,
            "--key-prior" => setup.key_prior = true,
            _ => positional.push(arg),
        }
    }
    let [dataset, labels_path] = &positional[..] else {
        eprintln!(
            "usage: chord_benchmark <dataset dir> <labels.json> [--split dev|val|test|all] \
             [--tolerance s] [--json out] [--compare before] [--only substr] [--verbose]"
        );
        std::process::exit(2);
    };
    let dataset = PathBuf::from(dataset);
    let labels: Labels =
        serde_json::from_str(&std::fs::read_to_string(labels_path).expect("read labels"))
            .expect("labels json");
    let songs: Vec<Song> = labels
        .songs
        .into_iter()
        .filter(|s| split == "all" || s.split.as_deref() == Some(split.as_str()))
        .filter(|s| only.as_ref().is_none_or(|o| s.file.contains(o.as_str())))
        .collect();

    let started = Instant::now();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let results: Vec<(Song, Result<Outcome, String>)> = std::thread::scope(|scope| {
        let chunk = songs.len().div_ceil(workers).max(1);
        let handles: Vec<_> = songs
            .chunks(chunk)
            .map(|batch| {
                let dataset = dataset.clone();
                scope.spawn(move || {
                    batch
                        .iter()
                        .map(|song| {
                            (
                                song.clone(),
                                analyze(&dataset, song, tolerance, verbose, setup),
                            )
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

    let mut total = Scores::default();
    let (mut labelled, mut unlabelled) = (0usize, 0usize);
    let (mut over_w, mut under_w, mut dur_w) = (0.0, 0.0, 0.0);
    let (mut mae_sum, mut ref_bounds, mut hits, mut est_bounds, mut est_hits_total) =
        (0.0, 0usize, 0usize, 0usize, 0.0);
    let (mut audio_seconds, mut analysis_ms) = (0.0, 0u128);
    let mut proxies = Vec::new();
    let mut reliability = [(0.0f64, 0.0f64); 10];
    let mut all_json = Vec::new();
    for (song, outcome) in &results {
        let name = song.file.rsplit('/').next().unwrap_or(&song.file);
        let o = match outcome {
            Ok(o) => o,
            Err(e) => {
                println!("ERROR {name}: {e}");
                continue;
            }
        };
        audio_seconds += o.seconds;
        for (acc, r) in reliability.iter_mut().zip(&o.reliability) {
            acc.0 += r.0;
            acc.1 += r.1;
        }
        analysis_ms += o.analysis_ms;
        let j = &o.json;
        if let Some(s) = &o.scores {
            labelled += 1;
            for (t, x) in [
                (&mut total.root, s.root),
                (&mut total.majmin, s.majmin),
                (&mut total.sevenths, s.sevenths),
                (&mut total.full, s.full),
                (&mut total.n_recall, s.n_recall),
                (&mut total.false_n, s.false_n),
            ] {
                t.hit += x.hit;
                t.total += x.total;
            }
            let (over, under) = o.seg.unwrap();
            over_w += over * o.seconds;
            under_w += under * o.seconds;
            dur_w += o.seconds;
            let (mae, rb, h, eb) = o.boundary.unwrap();
            mae_sum += mae;
            ref_bounds += rb;
            hits += h;
            est_bounds += eb;
            est_hits_total += j["boundary_precision"].as_f64().unwrap_or(0.0) / 100.0 * eb as f64;
            println!(
                "{name}\n  segments ref {} / est {} | root {} majmin {} sevenths {} full {} | N {} falseN {} | over {} under {} | MAE {} ms, boundary recall {}",
                j["reference_segments"], j["detected_segments"], j["root"], j["majmin"], j["sevenths"], j["full"],
                j["n_recall"], j["false_n"], j["over_seg"], j["under_seg"], j["boundary_mae_ms"], j["boundary_recall"],
            );
        } else {
            unlabelled += 1;
            proxies.push((
                j["segments_per_minute"].as_f64().unwrap_or(0.0),
                j["top_chord_share"].as_f64().unwrap_or(0.0),
            ));
            println!(
                "{name} (no chord labels)\n  {} segments, {} per minute, most common chord takes {:.0}% of the song: {}",
                j["detected_segments"], j["segments_per_minute"], j["top_chord_share"].as_f64().unwrap_or(0.0) * 100.0, j["top_chords"],
            );
        }
        all_json.push(j.clone());
    }

    println!("\n==== CHORD SUMMARY ====");
    println!("files with chord labels: {labelled} (split {split}), without: {unlabelled}");
    let mut summary = json!({ "split": split, "labelled": labelled, "unlabelled": unlabelled, "tolerance": tolerance });
    if labelled > 0 {
        println!("root accuracy:        {}", pct(total.root.rate()));
        println!("maj/min (MIREX):      {}", pct(total.majmin.rate()));
        println!("sevenths (MIREX):     {}", pct(total.sevenths.rate()));
        println!("full symbol:          {}", pct(total.full.rate()));
        println!(
            "N recall:             {}   chord time called N: {}",
            pct(total.n_recall.rate()),
            pct(total.false_n.rate())
        );
        println!("over-segmentation:    {:5.1}%   under-segmentation: {:5.1}%  (1 − directional Hamming; higher is better)", over_w / dur_w * 100.0, under_w / dur_w * 100.0);
        println!(
            "boundary MAE:         {:.0} ms   recall ±{:.0} ms: {:.1}%   precision: {:.1}%",
            mae_sum / ref_bounds.max(1) as f64 * 1000.0,
            tolerance * 1000.0,
            hits as f64 / ref_bounds.max(1) as f64 * 100.0,
            est_hits_total / est_bounds.max(1) as f64 * 100.0,
        );
        summary["root"] = json!(total.root.rate());
        summary["majmin"] = json!(total.majmin.rate());
        summary["sevenths"] = json!(total.sevenths.rate());
        summary["full"] = json!(total.full.rate());
        summary["n_recall"] = json!(total.n_recall.rate());
        summary["false_n"] = json!(total.false_n.rate());
        summary["over_seg"] = json!(over_w / dur_w);
        summary["under_seg"] = json!(under_w / dur_w);
        summary["boundary_mae"] = json!(mae_sum / ref_bounds.max(1) as f64);
        summary["boundary_recall"] = json!(hits as f64 / ref_bounds.max(1) as f64);
        summary["boundary_precision"] = json!(est_hits_total / est_bounds.max(1) as f64);
        let total_time: f64 = reliability.iter().map(|r| r.1).sum::<f64>().max(1e-9);
        println!(
            "confidence reliability (segments naming a chord; share of time root+maj/min correct):"
        );
        for (i, (ok, n)) in reliability.iter().enumerate() {
            if *n > 0.0 {
                println!(
                    "  {:.1}–{:.1}: {:5.1}% correct over {:4.1}% of chord time",
                    i as f64 / 10.0,
                    (i + 1) as f64 / 10.0,
                    ok / n * 100.0,
                    n / total_time * 100.0
                );
            }
        }
        summary["reliability"] = json!(reliability
            .iter()
            .map(|(ok, n)| json!({ "correct": ok, "seconds": n }))
            .collect::<Vec<_>>());
    }
    if unlabelled > 0 {
        let n = proxies.len() as f64;
        let spm = proxies.iter().map(|p| p.0).sum::<f64>() / n;
        let share = proxies.iter().map(|p| p.1).sum::<f64>() / n;
        println!("unlabelled songs (behaviour only, not accuracy): {spm:.1} segments/min, most common chord {:.0}% of the song on average", share * 100.0);
        summary["unlabelled_segments_per_minute"] = json!(spm);
        summary["unlabelled_top_chord_share"] = json!(share);
    }
    println!(
        "analysis: {:.1} s for {:.0} s of audio ({:.0}x realtime, summed over threads)",
        analysis_ms as f64 / 1000.0,
        audio_seconds,
        audio_seconds / (analysis_ms as f64 / 1000.0).max(1e-9)
    );
    println!("(wall {:.1} s)", started.elapsed().as_secs_f32());
    summary["songs"] = json!(all_json);
    if let Some(path) = json_out {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&summary).unwrap() + "\n",
        )
        .expect("write json");
        println!("wrote {}", path.display());
    }
    if let Some(path) = compare_with {
        let before: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read before"))
                .expect("before json");
        println!("\n==== COMPARE with {} ====", path.display());
        for key in [
            "root",
            "majmin",
            "sevenths",
            "full",
            "n_recall",
            "false_n",
            "over_seg",
            "under_seg",
            "boundary_mae",
            "boundary_recall",
            "boundary_precision",
            "unlabelled_segments_per_minute",
            "unlabelled_top_chord_share",
        ] {
            let (b, a) = (before[key].as_f64(), summary[key].as_f64());
            let lower_better = matches!(key, "false_n" | "boundary_mae");
            let mark = match (b, a) {
                (Some(b), Some(a)) if (a - b).abs() < 1e-9 => "",
                (Some(b), Some(a)) if (a > b) != lower_better => "  improved",
                (Some(_), Some(_)) => "  REGRESSED",
                _ => "",
            };
            if b.is_some() || a.is_some() {
                println!(
                    "{key:<32} {:>8} -> {:<8}{mark}",
                    b.map_or("—".into(), |v| format!("{v:.3}")),
                    a.map_or("—".into(), |v| format!("{v:.3}"))
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: f64, end: f64, label: &str) -> Span {
        Span {
            start,
            end,
            chord: chord_symbols::parse(label).unwrap(),
        }
    }

    #[test]
    fn a_missing_extension_keeps_root_and_triad_credit() {
        let truth = [span(0.0, 2.0, "Em7")];
        let s = score(&truth, &[span(0.0, 2.0, "Em")]);
        assert!((s.root.rate().unwrap() - 1.0).abs() < 1e-9);
        assert!((s.majmin.rate().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(s.sevenths.rate(), Some(0.0));
        assert_eq!(s.full.rate(), Some(0.0));
        let wrong = score(&truth, &[span(0.0, 2.0, "Bb")]);
        assert_eq!(wrong.root.rate(), Some(0.0));
        assert_eq!(wrong.majmin.rate(), Some(0.0));
    }

    #[test]
    fn scores_are_weighted_by_duration() {
        // A 3 s chord right, a 1 s chord wrong: 75%.
        let truth = [span(0.0, 3.0, "C"), span(3.0, 4.0, "G")];
        let s = score(&truth, &[span(0.0, 4.0, "C")]);
        assert!((s.majmin.rate().unwrap() - 0.75).abs() < 0.01);
    }

    #[test]
    fn no_chord_is_scored_both_ways() {
        let truth = [span(0.0, 1.0, "N"), span(1.0, 3.0, "Am")];
        let s = score(&truth, &[span(0.0, 2.0, "N"), span(2.0, 3.0, "Am")]);
        assert!((s.n_recall.rate().unwrap() - 1.0).abs() < 1e-9);
        assert!((s.false_n.rate().unwrap() - 0.5).abs() < 0.01);
    }
}
