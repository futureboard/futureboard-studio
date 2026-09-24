//! Evaluation driver for the rhythm and chord analysers.
//!
//! Reads a manifest (one `<id>\t<raw f32 mono path>\t<sample rate>` per line)
//! and prints one JSON object per track with beats, bar positions, tempo
//! sections and chords, for scoring against annotated datasets:
//!
//! ```text
//! cargo run --release -p sphere-audio-processor --example rhythm_eval -- manifest.tsv
//! ```

use std::io::{BufRead, Write};

use SphereAudioProcessor::analysis::{
    ChordOptions, RhythmOptions, analyze_rhythm, chroma_frames, estimate_key_ranked,
    recognize_chords,
};

fn main() {
    let manifest = std::env::args().nth(1).expect("manifest path");
    let file = std::fs::File::open(&manifest).expect("open manifest");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.expect("read manifest");
        let mut parts = line.split('\t');
        let (Some(id), Some(path), Some(sr)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let sr: f32 = sr.trim().parse().expect("sample rate");
        let bytes = std::fs::read(path).expect("read audio");
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let started = std::time::Instant::now();
        let chroma = chroma_frames(&samples, sr);
        let rhythm = analyze_rhythm(&samples, sr, chroma.as_ref(), RhythmOptions::default());
        let key = estimate_key_ranked(&samples, sr).into_iter().next();
        let chords = match (&chroma, &rhythm) {
            (Some(chroma), Some(rhythm)) => {
                let beats: Vec<f64> = rhythm.beats.iter().map(|b| b.seconds).collect();
                let down: Vec<bool> = rhythm.beats.iter().map(|b| b.position == 1).collect();
                recognize_chords(chroma, &beats, &down, ChordOptions { sevenths: true })
            }
            _ => Vec::new(),
        };
        let elapsed = started.elapsed().as_secs_f64();
        let json = serde_json::json!({
            "id": id,
            "seconds_to_analyze": elapsed,
            "bpm": rhythm.as_ref().map(|r| r.bpm),
            "variable": rhythm.as_ref().map(|r| r.variable),
            "beats_per_bar": rhythm.as_ref().map(|r| r.beats_per_bar),
            "beats": rhythm.as_ref().map(|r| r.beats.iter().map(|b| b.seconds).collect::<Vec<_>>()).unwrap_or_default(),
            "positions": rhythm.as_ref().map(|r| r.beats.iter().map(|b| b.position).collect::<Vec<_>>()).unwrap_or_default(),
            "locked": rhythm.as_ref().map(|r| r.beats.iter().map(|b| b.locked).collect::<Vec<_>>()).unwrap_or_default(),
            "sections": rhythm.as_ref().map(|r| r.sections.iter().map(|s| (s.start_seconds, s.end_seconds, s.bpm)).collect::<Vec<_>>()).unwrap_or_default(),
            "chords": chords.iter().map(|c| (c.start_seconds, c.end_seconds, c.chord.map(|l| l.harte()).unwrap_or_else(|| "N".into()))).collect::<Vec<_>>(),
            "key": key.map(|k| format!("{:?} {:?}", k.tonic, k.mode)),
        });
        writeln!(out, "{json}").expect("write");
    }
}
