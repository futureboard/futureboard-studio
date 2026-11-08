//! Plain-data request describing what to render offline.

use crate::runtime::build_project_tempo_map;
use crate::types::EngineProjectSnapshot;

/// What to do after the last timeline content ends, so reverb/instrument tails
/// are not hard-cut.
#[derive(Debug, Clone, Copy, Default)]
pub enum ExportTailMode {
    #[default]
    None,
    FixedSeconds(f64),
    /// Keep rendering until the block peak drops below `threshold_db`, up to
    /// `max_seconds`.
    UntilSilence {
        max_seconds: f64,
        threshold_db: f32,
    },
}

/// How long the output must stay below the threshold before an
/// [`ExportTailMode::UntilSilence`] tail ends.
///
/// One quiet block is not the end of a tail: delay, echo and gated reverb
/// fall silent between repeats, and stopping at the first gap cut every
/// repeat after it. Two seconds spans the gap of any tempo-synced delay down
/// to a half note at 60 BPM. The quiet stretch is held back rather than
/// written, so a tail that does end leaves no trailing silence behind.
pub(crate) const SILENCE_HOLD_SECONDS: f64 = 2.0;

/// What [`SilenceTail`] makes of one block of the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TailBlock {
    /// At or above the threshold: write it, after any quiet blocks held back
    /// before it — they were a gap, not the end.
    Audible,
    /// Below the threshold: hold it back until the tail resumes or ends.
    Quiet,
    /// Quiet for [`SILENCE_HOLD_SECONDS`]: the tail is over. Drop what was
    /// held back, this block included, and stop.
    Ended,
}

/// Decides where an [`ExportTailMode::UntilSilence`] tail ends, fed the peak
/// of each block past the content.
#[derive(Debug, Clone)]
pub(crate) struct SilenceTail {
    threshold: f32,
    hold_frames: u64,
    quiet_frames: u64,
}

impl SilenceTail {
    /// `None` unless `tail` is [`ExportTailMode::UntilSilence`].
    pub(crate) fn new(tail: ExportTailMode, sample_rate: u32) -> Option<Self> {
        let ExportTailMode::UntilSilence { threshold_db, .. } = tail else {
            return None;
        };
        Some(Self {
            threshold: 10f32.powf(threshold_db / 20.0).clamp(0.0, 1.0),
            hold_frames: ((SILENCE_HOLD_SECONDS * f64::from(sample_rate)).round() as u64).max(1),
            quiet_frames: 0,
        })
    }

    /// Classify a tail block of `frames` frames whose peak is `peak`.
    pub(crate) fn observe(&mut self, peak: f32, frames: u64) -> TailBlock {
        // Invalid samples are not silence.
        if peak.is_nan() || peak >= self.threshold {
            self.quiet_frames = 0;
            return TailBlock::Audible;
        }
        self.quiet_frames = self.quiet_frames.saturating_add(frames);
        if self.quiet_frames >= self.hold_frames {
            TailBlock::Ended
        } else {
            TailBlock::Quiet
        }
    }
}

/// Post-render gain stage.
#[derive(Debug, Clone, Copy, Default)]
pub enum ExportNormalizeMode {
    #[default]
    None,
    /// Two-pass: analyze peak, then apply gain so the peak hits `db` dBFS.
    PeakDb(f32),
}

/// Geometry for an offline render. Sample positions are absolute project
/// samples at `sample_rate`. `end_sample` is exclusive.
#[derive(Debug, Clone)]
pub struct OfflineRenderRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub start_sample: u64,
    pub end_sample: u64,
    /// Linear master gain (mirrors the engine's master volume atomic).
    pub master_volume: f32,
    pub block_size: usize,
    pub tail: ExportTailMode,
    pub normalize: ExportNormalizeMode,
}

impl OfflineRenderRequest {
    /// Number of content frames (excluding tail) this request will render.
    pub fn content_frames(&self) -> u64 {
        self.end_sample.saturating_sub(self.start_sample)
    }

    /// Max tail frames implied by the tail mode (UntilSilence reports its cap).
    pub fn max_tail_frames(&self) -> u64 {
        let secs = match self.tail {
            ExportTailMode::None => 0.0,
            ExportTailMode::FixedSeconds(s) => s.max(0.0),
            ExportTailMode::UntilSilence { max_seconds, .. } => max_seconds.max(0.0),
        };
        (secs * self.sample_rate as f64).round() as u64
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.sample_rate == 0 {
            return Err("sample_rate must be non-zero".to_string());
        }
        if self.channels == 0 {
            return Err("channels must be non-zero".to_string());
        }
        if self.end_sample <= self.start_sample {
            return Err("range end must be greater than range start".to_string());
        }
        if self.block_size == 0 {
            return Err("block_size must be non-zero".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod silence_tail_tests {
    use super::*;

    fn tail() -> SilenceTail {
        SilenceTail::new(
            ExportTailMode::UntilSilence {
                max_seconds: 10.0,
                threshold_db: -60.0,
            },
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn only_until_silence_has_a_silence_tail() {
        assert!(SilenceTail::new(ExportTailMode::None, 48_000).is_none());
        assert!(SilenceTail::new(ExportTailMode::FixedSeconds(5.0), 48_000).is_none());
    }

    /// The reported case: a dry hit, then echoes of a 1/4-note delay with
    /// 40 % feedback, silent between repeats. The first gap is held back, not
    /// taken for the end; the tail ends only once the echoes have decayed
    /// below the threshold and stayed there for the hold.
    #[test]
    fn a_gap_between_echoes_is_not_the_end_of_a_decaying_tail() {
        let mut tail = tail();
        // 1 kHz, 100-frame blocks: a 50 ms echo every 500 ms (5 blocks).
        let mut verdicts = Vec::new();
        let mut level = 0.4f32; // the first echo; the dry hit was the content
        for block in 0..200 {
            let peak = if block % 5 == 0 { level } else { 0.0 };
            if block % 5 == 0 {
                level *= 0.4;
            }
            let verdict = tail.observe(peak, 100);
            verdicts.push(verdict);
            if verdict == TailBlock::Ended {
                break;
            }
        }
        // Echo n is 0.4^n, at or above 0.001 for n <= 7: seven audible
        // echoes, at blocks 0, 5, ..., 30.
        let audible: Vec<usize> = verdicts
            .iter()
            .enumerate()
            .filter(|(_, v)| **v == TailBlock::Audible)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(audible, [0, 5, 10, 15, 20, 25, 30]);
        // Every block between echoes was held, never the end.
        assert!(verdicts[1..30].iter().all(|v| *v != TailBlock::Ended));
        // The end: two seconds (20 blocks) after the last audible echo.
        assert_eq!(verdicts.len() - 1, 30 + 20);
        assert_eq!(*verdicts.last().unwrap(), TailBlock::Ended);
    }

    #[test]
    fn sound_after_a_gap_resets_the_hold() {
        let mut tail = tail();
        for _ in 0..19 {
            assert_eq!(tail.observe(0.0, 100), TailBlock::Quiet);
        }
        assert_eq!(tail.observe(0.5, 100), TailBlock::Audible);
        for _ in 0..19 {
            assert_eq!(tail.observe(0.0, 100), TailBlock::Quiet);
        }
        assert_eq!(tail.observe(0.0, 100), TailBlock::Ended);
        // Invalid samples are never silence.
        let mut tail = self::tail();
        assert_eq!(tail.observe(f32::NAN, 100), TailBlock::Audible);
    }
}

/// Compute the natural `[start, end)` sample bounds of the entire arrangement
/// from a snapshot: sample 0 to the latest content end across audio + MIDI
/// clips, converted through the project tempo map. Returns `(0, 0)` for an
/// empty arrangement.
pub fn arrangement_bounds_samples(
    snapshot: &EngineProjectSnapshot,
    sample_rate: u32,
) -> (u64, u64) {
    let sr = sample_rate.max(1) as f64;
    let mut end_beat = 0.0f64;
    for clip in &snapshot.clips {
        end_beat = end_beat.max(clip.start_beat + clip.duration_beats.max(0.0));
    }
    for clip in &snapshot.midi_clips {
        end_beat = end_beat.max(clip.start_beat + clip.length_beats.max(0.0));
    }
    if end_beat <= 0.0 {
        return (0, 0);
    }
    let tempo_map = build_project_tempo_map(snapshot);
    let end_sample = tempo_map.samples_at_beat(end_beat, sr);
    (0, end_sample)
}

/// Convert a project beat position to an absolute sample position at
/// `sample_rate`, honoring the snapshot's tempo map (not just a static BPM).
pub fn beats_to_samples(snapshot: &EngineProjectSnapshot, beat: f64, sample_rate: u32) -> u64 {
    let sr = sample_rate.max(1) as f64;
    let tempo_map = build_project_tempo_map(snapshot);
    tempo_map.samples_at_beat(beat.max(0.0), sr)
}

/// Scale the live device block duration to the output rate so block-driven
/// automation is evaluated at the same time intervals during offline export.
pub fn render_block_size_for_sample_rate(
    snapshot: &EngineProjectSnapshot,
    sample_rate: u32,
) -> usize {
    let source_rate = snapshot.sample_rate.max(1) as u64;
    let output_rate = sample_rate.max(1) as u64;
    let live_block = snapshot.routing.buffer_size.max(1) as u64;
    let output_block = live_block
        .saturating_mul(output_rate)
        .saturating_add(source_rate / 2)
        / source_rate;
    output_block
        .max(1)
        .min(crate::plugin_bridge::MAX_BRIDGE_BLOCK_FRAMES as u64) as usize
}
