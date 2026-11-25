//! Strip Silence: find where an audio clip's audio is above a threshold and
//! keep only those parts of the clip.
//!
//! [`detect_kept_regions`] reads the audio the clip plays — its source window
//! in play order, as Convert to MIDI reads it — and answers in seconds into
//! that window. [`TimelineState::plan_strip_silence`] places those seconds on
//! the timeline through the same beat ↔ time mapping playback and Convert to
//! MIDI use, and cuts the clip there with the ordinary split, so each kept
//! piece plays exactly the audio it played before.

use super::*;

/// Strip Silence's settings, as the dialog edits them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripSilenceParams {
    /// Audio below this level (dBFS, short-window RMS) is silence.
    pub threshold_db: f32,
    /// Quieter stretches shorter than this are kept, not cut out.
    pub min_silence_ms: f32,
    /// Kept parts shorter than this are dropped.
    pub min_clip_ms: f32,
    /// Kept before audio rises above the threshold.
    pub pre_attack_ms: f32,
    /// Kept after audio falls below the threshold.
    pub post_release_ms: f32,
    /// Fade put on each new clip edge.
    pub fade_ms: f32,
}

impl Default for StripSilenceParams {
    fn default() -> Self {
        Self {
            threshold_db: -48.0,
            min_silence_ms: 250.0,
            min_clip_ms: 80.0,
            pre_attack_ms: 10.0,
            post_release_ms: 60.0,
            fade_ms: 5.0,
        }
    }
}

impl StripSilenceParams {
    pub const THRESHOLD_RANGE: (f32, f32) = (-96.0, 0.0);
    pub const MIN_SILENCE_RANGE: (f32, f32) = (0.0, 5000.0);
    pub const MIN_CLIP_RANGE: (f32, f32) = (0.0, 5000.0);
    pub const PRE_ATTACK_RANGE: (f32, f32) = (0.0, 1000.0);
    pub const POST_RELEASE_RANGE: (f32, f32) = (0.0, 2000.0);
    pub const FADE_RANGE: (f32, f32) = (0.0, 500.0);

    /// Every value inside its range.
    pub fn clamped(self) -> Self {
        let clamp = |value: f32, (lo, hi): (f32, f32)| {
            if value.is_finite() {
                value.clamp(lo, hi)
            } else {
                lo
            }
        };
        Self {
            threshold_db: clamp(self.threshold_db, Self::THRESHOLD_RANGE),
            min_silence_ms: clamp(self.min_silence_ms, Self::MIN_SILENCE_RANGE),
            min_clip_ms: clamp(self.min_clip_ms, Self::MIN_CLIP_RANGE),
            pre_attack_ms: clamp(self.pre_attack_ms, Self::PRE_ATTACK_RANGE),
            post_release_ms: clamp(self.post_release_ms, Self::POST_RELEASE_RANGE),
            fade_ms: clamp(self.fade_ms, Self::FADE_RANGE),
        }
    }
}

/// Hop between level readings, in seconds. Kept-region edges land on this
/// grid before padding.
const LEVEL_HOP_SECONDS: f64 = 0.005;

/// The parts of `samples` (one level value per frame at `sample_rate` Hz —
/// mono, or each frame's RMS across its channels) to keep, as
/// `(start, end)` seconds into the buffer, in order and not overlapping.
///
/// The level is a 10 ms RMS read every 5 ms. Runs at or above the threshold
/// are padded by the pre-attack and post-release times, runs closer than
/// the minimum silence are joined, and what is then shorter than the
/// minimum clip length is dropped.
pub fn detect_kept_regions(
    samples: &[f32],
    sample_rate: u32,
    params: &StripSilenceParams,
) -> Vec<(f64, f64)> {
    if samples.is_empty() || sample_rate == 0 {
        return Vec::new();
    }
    let params = params.clamped();
    let rate = sample_rate as f64;
    let total = samples.len();
    let duration = total as f64 / rate;
    let hop = ((rate * LEVEL_HOP_SECONDS).round() as usize).max(1);
    let threshold = 10f64.powf(params.threshold_db as f64 / 20.0);
    let threshold_power = threshold * threshold;

    // Loud runs, in frames. Each hop's level is read over a window twice
    // its length centred on it.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut hop_start = 0;
    while hop_start < total {
        let hop_end = (hop_start + hop).min(total);
        let from = hop_start.saturating_sub(hop / 2);
        let to = (hop_end + hop / 2).min(total);
        let power = samples[from..to]
            .iter()
            .map(|&s| {
                let s = s as f64;
                if s.is_finite() {
                    s * s
                } else {
                    0.0
                }
            })
            .sum::<f64>()
            / (to - from) as f64;
        if power >= threshold_power && power > 0.0 {
            match runs.last_mut() {
                Some((_, end)) if *end == hop_start => *end = hop_end,
                _ => runs.push((hop_start, hop_end)),
            }
        }
        hop_start = hop_end;
    }

    let pre = params.pre_attack_ms as f64 / 1000.0;
    let post = params.post_release_ms as f64 / 1000.0;
    let min_silence = params.min_silence_ms as f64 / 1000.0;
    let min_clip = params.min_clip_ms as f64 / 1000.0;

    let mut kept: Vec<(f64, f64)> = Vec::with_capacity(runs.len());
    for (start, end) in runs {
        let start = (start as f64 / rate - pre).max(0.0);
        let end = (end as f64 / rate + post).min(duration);
        match kept.last_mut() {
            Some((_, last_end)) if start - *last_end < min_silence => {
                *last_end = last_end.max(end);
            }
            _ => kept.push((start, end)),
        }
    }
    kept.retain(|(start, end)| end - start >= min_clip);
    kept
}

/// What Strip Silence does to one clip.
#[derive(Debug, Clone, PartialEq)]
pub enum StripSilenceOutcome {
    /// The clip is gone or is not audio.
    Missing,
    /// Nothing in the clip is kept: it is left as it is.
    Silent,
    /// The kept audio is the whole clip: nothing to cut.
    Unchanged,
    /// The clip becomes these pieces, in timeline order, with fresh ids.
    Pieces(Vec<ClipState>),
}

/// Kept edges this close (beats) to the clip's own edge are the clip's edge.
const STRIP_EDGE_BEATS: f64 = 1.0e-3;

/// A cut keeps at least this much (beats) on the side Strip Silence keeps.
const STRIP_SPLIT_MIN_BEATS: f32 = 1.0e-4;

impl TimelineState {
    /// `clip_id` cut down to `regions` — `(start, end)` seconds into its
    /// played source window, in play order, as [`detect_kept_regions`]
    /// answers. Each piece is an ordinary split of the clip at the kept
    /// region's edges ([`Self::plan_audio_clip_split_keeping`]), so it plays
    /// the audio it played before; new edges get `fade_ms`, the clip's own
    /// edges keep their fades. A piece is never shorter than an audio clip
    /// may be ([`MIN_AUDIO_CLIP_BEATS`]): a shorter one is widened inside the
    /// clip. Pieces are numbered `clip-{next_id}` on, advancing `next_id`, so
    /// several clips can be planned before any is inserted.
    ///
    /// The splits note their own scratch ids in `clip_lineage`; once every
    /// clip is planned the caller records each piece's origin, so a piece
    /// keeps its clip's ARA edits.
    pub fn plan_strip_silence(
        &self,
        clip_id: &str,
        regions: &[(f64, f64)],
        fade_ms: f32,
        next_id: &mut u32,
    ) -> StripSilenceOutcome {
        let Some((_, clip)) = self.find_clip(clip_id) else {
            return StripSilenceOutcome::Missing;
        };
        if !matches!(clip.clip_type, ClipType::Audio { .. }) {
            return StripSilenceOutcome::Missing;
        }
        let clip_start = clip.start_beat.max(0.0) as f64;
        let clip_end = clip_start + clip.duration_beats.max(0.0) as f64;
        let min_len = (MIN_AUDIO_CLIP_BEATS as f64).min(clip_end - clip_start);

        // The kept regions in timeline beats, inside the clip, each at least
        // an audio clip long, joined where they meet.
        let mut beats: Vec<(f64, f64)> = regions
            .iter()
            .filter_map(|&(from, to)| {
                let a = self.audio_clip_beat_at_window_seconds(clip, from.max(0.0));
                let b = self.audio_clip_beat_at_window_seconds(clip, to.max(from).max(0.0));
                let (a, b) = (a.min(b).max(clip_start), a.max(b).min(clip_end));
                (b > a).then_some((a, b))
            })
            .map(|(mut a, mut b)| {
                if b - a < min_len {
                    b = (a + min_len).min(clip_end);
                    a = (b - min_len).max(clip_start);
                }
                (a, b)
            })
            .collect();
        beats.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut joined: Vec<(f64, f64)> = Vec::with_capacity(beats.len());
        for (a, b) in beats {
            match joined.last_mut() {
                Some((_, end)) if a <= *end + STRIP_EDGE_BEATS => *end = end.max(b),
                _ => joined.push((a, b)),
            }
        }
        if joined.is_empty() {
            return StripSilenceOutcome::Silent;
        }
        if let [(a, b)] = joined.as_slice() {
            if *a <= clip_start + STRIP_EDGE_BEATS && *b >= clip_end - STRIP_EDGE_BEATS {
                return StripSilenceOutcome::Unchanged;
            }
        }

        // The window the clip plays, for mirroring a reversed clip's pieces.
        let window_start = clip.stretch.source_start_samples;
        let window_end = if clip.stretch.source_end_samples > window_start {
            clip.stretch.source_end_samples
        } else {
            clip.stretch.original_duration_samples
        };

        let mut pieces = Vec::with_capacity(joined.len());
        for (a, b) in joined {
            let mut piece = clip.clone();
            if a > clip_start + STRIP_EDGE_BEATS {
                if let Some((_, right)) =
                    self.plan_audio_clip_split_keeping(&piece, a as f32, STRIP_SPLIT_MIN_BEATS)
                {
                    piece = right;
                    piece.stretch.fade_in_ms = fade_ms.max(0.0);
                }
            }
            let piece_end = piece.start_beat as f64 + piece.duration_beats as f64;
            if b < piece_end - STRIP_EDGE_BEATS {
                if let Some((left, _)) =
                    self.plan_audio_clip_split_keeping(&piece, b as f32, STRIP_SPLIT_MIN_BEATS)
                {
                    piece = left;
                    piece.stretch.fade_out_ms = fade_ms.max(0.0);
                }
            }
            // A split cuts the window from its start; a reversed clip plays
            // it from its end, so its piece's window is the mirror image.
            if clip.stretch.reverse && window_end > window_start {
                let (from, to) = (
                    piece.stretch.source_start_samples,
                    piece.stretch.source_end_samples,
                );
                let mirror = |frame: u64| {
                    (window_start + window_end)
                        .saturating_sub(frame)
                        .clamp(window_start, window_end)
                };
                piece.stretch.apply_trim(mirror(to), mirror(from));
            }
            let id = format!("clip-{}", *next_id);
            *next_id += 1;
            piece.id = id;
            piece.name = clip.name.clone();
            pieces.push(piece);
        }
        StripSilenceOutcome::Pieces(pieces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// `seconds` of a 440 Hz tone at `amplitude`.
    fn tone(seconds: f64, amplitude: f32) -> Vec<f32> {
        let n = (seconds * RATE as f64).round() as usize;
        (0..n)
            .map(|i| {
                amplitude * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / RATE as f32).sin()
            })
            .collect()
    }

    fn silence(seconds: f64) -> Vec<f32> {
        vec![0.0; (seconds * RATE as f64).round() as usize]
    }

    fn no_padding() -> StripSilenceParams {
        StripSilenceParams {
            pre_attack_ms: 0.0,
            post_release_ms: 0.0,
            min_clip_ms: 0.0,
            min_silence_ms: 0.0,
            ..StripSilenceParams::default()
        }
    }

    fn close(actual: f64, expected: f64, within: f64) -> bool {
        (actual - expected).abs() <= within
    }

    #[test]
    fn tones_between_silences_are_kept() {
        let audio = [silence(1.0), tone(0.5, 0.5), silence(1.0), tone(0.25, 0.5)].concat();
        let kept = detect_kept_regions(&audio, RATE, &no_padding());
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(close(kept[0].0, 1.0, 0.006), "{kept:?}");
        assert!(close(kept[0].1, 1.5, 0.006), "{kept:?}");
        assert!(close(kept[1].0, 2.5, 0.006), "{kept:?}");
        assert!(
            close(kept[1].1, 2.75, 1.0e-9),
            "ends at the buffer: {kept:?}"
        );
    }

    #[test]
    fn padding_widens_and_stays_inside_the_audio() {
        let audio = [tone(0.5, 0.5), silence(1.0)].concat();
        let params = StripSilenceParams {
            pre_attack_ms: 100.0,
            post_release_ms: 200.0,
            ..no_padding()
        };
        let kept = detect_kept_regions(&audio, RATE, &params);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, 0.0, "pre-attack stops at the start");
        assert!(close(kept[0].1, 0.7, 0.006), "{kept:?}");
    }

    #[test]
    fn short_gaps_are_joined_and_short_blips_dropped() {
        let audio = [
            tone(0.3, 0.5),
            silence(0.1),
            tone(0.3, 0.5),
            silence(1.0),
            tone(0.02, 0.5),
            silence(1.0),
        ]
        .concat();
        let params = StripSilenceParams {
            min_silence_ms: 250.0,
            min_clip_ms: 80.0,
            ..no_padding()
        };
        let kept = detect_kept_regions(&audio, RATE, &params);
        assert_eq!(kept.len(), 1, "the 20 ms blip is dropped: {kept:?}");
        assert!(close(kept[0].0, 0.0, 0.006));
        assert!(close(kept[0].1, 0.7, 0.006), "{kept:?}");
    }

    #[test]
    fn quiet_audio_below_the_threshold_is_silence() {
        // -60 dBFS tone against a -48 dB threshold.
        let audio = [tone(1.0, 0.001 * std::f32::consts::SQRT_2), tone(0.5, 0.5)].concat();
        let kept = detect_kept_regions(&audio, RATE, &no_padding());
        assert_eq!(kept.len(), 1);
        assert!(close(kept[0].0, 1.0, 0.006), "{kept:?}");
        let all_quiet = detect_kept_regions(&silence(2.0), RATE, &no_padding());
        assert!(all_quiet.is_empty());
        assert!(detect_kept_regions(&[], RATE, &no_padding()).is_empty());
    }

    /// An audio clip at `start_beat`, `beats` long, over a 10 s 48 kHz file.
    fn audio_clip(state: &mut TimelineState, start_beat: f32, beats: f32) -> String {
        let track_id = state.create_audio_track();
        let mut clip = ClipState {
            id: "clip-1".into(),
            name: "Vocal".into(),
            start_beat,
            duration_beats: beats,
            source_duration_seconds: Some(10.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "file-1".into(),
                source_path: Some("vocal.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        clip.stretch.original_sample_rate = RATE;
        clip.stretch.original_duration_samples = 480_000;
        clip.stretch.source_start_samples = 0;
        clip.stretch.source_end_samples = 480_000;
        clip.stretch.fade_in_ms = 20.0;
        clip.stretch.fade_out_ms = 30.0;
        state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .unwrap()
            .clips
            .push(clip);
        "clip-1".into()
    }

    fn pieces(outcome: StripSilenceOutcome) -> Vec<ClipState> {
        match outcome {
            StripSilenceOutcome::Pieces(pieces) => pieces,
            other => panic!("expected pieces, got {other:?}"),
        }
    }

    #[test]
    fn kept_seconds_become_pieces_over_the_same_audio() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        // 10 s at 120 BPM: 20 beats.
        let clip = audio_clip(&mut state, 4.0, 20.0);
        let mut next = 7;
        let kept =
            pieces(state.plan_strip_silence(&clip, &[(1.0, 2.0), (5.0, 6.5)], 5.0, &mut next));
        assert_eq!(next, 9);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, "clip-7");
        assert_eq!(kept[1].id, "clip-8");
        assert!(kept.iter().all(|piece| piece.name == "Vocal"));
        // Half a second a beat.
        assert!((kept[0].start_beat - 6.0).abs() < 1.0e-4);
        assert!((kept[0].duration_beats - 2.0).abs() < 1.0e-4);
        assert!((kept[1].start_beat - 14.0).abs() < 1.0e-4);
        assert!((kept[1].duration_beats - 3.0).abs() < 1.0e-4);
        // Each piece plays the frames it played in the whole clip.
        assert_eq!(kept[0].stretch.source_start_samples, 48_000);
        assert_eq!(kept[0].stretch.source_end_samples, 96_000);
        assert_eq!(kept[1].stretch.source_start_samples, 240_000);
        assert_eq!(kept[1].stretch.source_end_samples, 312_000);
        // New edges fade; there are no old edges among them.
        assert_eq!(kept[0].stretch.fade_in_ms, 5.0);
        assert_eq!(kept[0].stretch.fade_out_ms, 5.0);
    }

    #[test]
    fn the_clips_own_edges_keep_their_fades() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 20.0);
        let mut next = 2;
        let kept =
            pieces(state.plan_strip_silence(&clip, &[(0.0, 3.0), (8.0, 10.0)], 5.0, &mut next));
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].start_beat, 0.0);
        assert_eq!(kept[0].stretch.fade_in_ms, 20.0, "the clip's start");
        assert_eq!(kept[0].stretch.fade_out_ms, 5.0);
        assert_eq!(kept[1].stretch.fade_in_ms, 5.0);
        assert_eq!(kept[1].stretch.fade_out_ms, 30.0, "the clip's end");
        assert!((kept[1].start_beat + kept[1].duration_beats - 20.0).abs() < 1.0e-4);
    }

    #[test]
    fn silent_and_whole_clips_are_left_alone() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 20.0);
        let mut next = 2;
        assert_eq!(
            state.plan_strip_silence(&clip, &[], 5.0, &mut next),
            StripSilenceOutcome::Silent
        );
        assert_eq!(
            state.plan_strip_silence(&clip, &[(0.0, 10.0)], 5.0, &mut next),
            StripSilenceOutcome::Unchanged
        );
        assert_eq!(
            state.plan_strip_silence("nope", &[(0.0, 1.0)], 5.0, &mut next),
            StripSilenceOutcome::Missing
        );
        assert_eq!(next, 2, "no ids handed out");
    }

    #[test]
    fn a_short_region_is_widened_to_a_usable_clip() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 20.0);
        let mut next = 2;
        // 40 ms is under a quarter beat (125 ms at 120 BPM).
        let kept = pieces(state.plan_strip_silence(&clip, &[(2.0, 2.04)], 5.0, &mut next));
        assert_eq!(kept.len(), 1);
        assert!((kept[0].duration_beats - MIN_AUDIO_CLIP_BEATS).abs() < 1.0e-4);
        assert!((kept[0].start_beat - 4.0).abs() < 1.0e-4);
    }

    #[test]
    fn a_stretched_clip_maps_through_its_ratio() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 40.0);
        for track in &mut state.tracks {
            for c in &mut track.clips {
                c.stretch.mode = StretchMode::Manual;
                c.stretch.preserve_pitch = true;
                c.stretch.stretch_ratio = 2.0;
            }
        }
        let mut next = 2;
        let kept = pieces(state.plan_strip_silence(&clip, &[(1.0, 2.0)], 5.0, &mut next));
        // One second of audio plays for two: four beats from beat four.
        assert!(
            (kept[0].start_beat - 4.0).abs() < 1.0e-3,
            "{}",
            kept[0].start_beat
        );
        assert!((kept[0].duration_beats - 4.0).abs() < 1.0e-3);
        assert_eq!(kept[0].stretch.source_start_samples, 48_000);
        assert_eq!(kept[0].stretch.source_end_samples, 96_000);
    }

    #[test]
    fn a_reversed_clip_keeps_the_audio_it_plays_there() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let clip = audio_clip(&mut state, 0.0, 20.0);
        for track in &mut state.tracks {
            for c in &mut track.clips {
                c.stretch.reverse = true;
            }
        }
        let mut next = 2;
        // The first two seconds it plays are the file's last two.
        let kept = pieces(state.plan_strip_silence(&clip, &[(1.0, 2.0)], 5.0, &mut next));
        assert!((kept[0].start_beat - 2.0).abs() < 1.0e-4);
        assert_eq!(kept[0].stretch.source_start_samples, 384_000);
        assert_eq!(kept[0].stretch.source_end_samples, 432_000);
    }
}
