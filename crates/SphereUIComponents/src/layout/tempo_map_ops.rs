//! Map the project's tempo onto a recording, and put its chords on the
//! Chord Track.
//!
//! "Map Tempo" keeps the audio where it is and moves the grid onto it: the
//! first detected downbeat becomes a bar line, every detected beat lands on a
//! project beat, and bars the band played short or long get their own meter.
//! Audio clips keep their wall-clock positions (the tempo map changes what a
//! beat is, not when the recording plays); MIDI stays on its beats.

use gpui::Context;

use crate::components::edit::{
    ClipSnapshot, EditCommand, TempoStateSnapshot, TimeSignatureStateSnapshot,
};
use crate::components::timeline::timeline_state::{
    ClipType, StretchMode, TempoCurve, TempoMap, TempoPoint, TimelineState,
};

use super::StudioLayout;

/// Largest drift, in seconds, a merged tempo segment may put any detected
/// beat off its project beat. Under what an ear resolves as a flam, and wide
/// enough that a live band's small push and pull does not become a point on
/// every bar.
const MERGE_TOLERANCE_SECONDS: f64 = 0.015;
/// The same inside a transition — a short run of off-grid beats next to a
/// steady section's grid (a ritardando or push into the next section) — so
/// each of its beats keeps its own tempo.
const TRANSITION_TOLERANCE_SECONDS: f64 = 0.004;
/// Off-grid runs up to this many bars long next to grid beats are
/// transitions; longer ones are a band playing freely.
const TRANSITION_MAX_BARS: usize = 2;
/// Tempo resolutions tried in order: a segment gets the coarsest one that
/// still keeps its beats within tolerance, so a song made at 120 maps as 120.
const TEMPO_STEPS: [f64; 3] = [1.0, 0.1, 0.01];

/// A tempo map laid over detected beats.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TempoMapPlan {
    /// `(beat, bpm)` Hold points, starting at beat 0.
    pub tempo: Vec<(f64, f64)>,
    /// `(beat, numerator, denominator)`, starting at beat 0.
    pub meter: Vec<(f64, u16, u16)>,
    /// Project beat of the first detected downbeat.
    pub first_downbeat_beat: f64,
}

/// The coarsest rounding of `bpm` in [`TEMPO_STEPS`] that `keeps`, else `bpm`.
fn clean_bpm(bpm: f64, keeps: impl Fn(f64) -> bool) -> f64 {
    TEMPO_STEPS
        .iter()
        .map(|step| (bpm / step).round() * step)
        .find(|&rounded| rounded > 0.0 && keeps(rounded))
        .unwrap_or(bpm)
}

/// How far each beat may sit off the map: tight inside a transition, loose
/// elsewhere.
fn beat_tolerances(locked: &[bool], beats_per_bar: usize) -> Vec<f64> {
    let mut out = vec![MERGE_TOLERANCE_SECONDS; locked.len()];
    let mut i = 0;
    while i < locked.len() {
        if locked[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < locked.len() && !locked[i] {
            i += 1;
        }
        let next_to_grid = (start > 0 && locked[start - 1]) || (i < locked.len() && locked[i]);
        if next_to_grid && i - start <= TRANSITION_MAX_BARS * beats_per_bar.max(1) {
            out[start..i].fill(TRANSITION_TOLERANCE_SECONDS);
        }
    }
    out
}

/// Plan a tempo map that puts every detected beat on a project beat.
///
/// `seconds` are the detected beats on the project's clock, `positions` their
/// 1-based bar positions (1 = downbeat), `locked` whether each sits on a
/// steady section's grid (may be empty), `beats_per_bar` the detected meter.
pub(crate) fn plan_tempo_map(
    seconds: &[f64],
    positions: &[u32],
    locked: &[bool],
    beats_per_bar: u32,
) -> Option<TempoMapPlan> {
    let bpb = beats_per_bar.clamp(1, 16) as u16;
    let first = positions.iter().position(|&p| p == 1).unwrap_or(0);
    let beats: Vec<f64> = seconds[first..].to_vec();
    let positions = &positions[first..];
    let tolerance = beat_tolerances(
        &(first..seconds.len())
            .map(|i| locked.get(i).copied().unwrap_or(false))
            .collect::<Vec<_>>(),
        bpb as usize,
    );
    if beats.len() < 2 || beats[0] < 0.0 {
        return None;
    }
    let intervals: Vec<f64> = beats.windows(2).map(|w| w[1] - w[0]).collect();
    if intervals.iter().any(|d| *d <= 0.0) {
        return None;
    }

    // Pre-roll: the time before the first downbeat, at the first bar's tempo.
    let mut head: Vec<f64> = intervals.iter().take(bpb as usize).copied().collect();
    head.sort_by(|a, b| a.total_cmp(b));
    let first_period = head[head.len() / 2];
    let mut pre_bpm = 60.0 / first_period;
    let mut offset = beats[0] / first_period;
    let whole = offset.round();
    // A pickup within tolerance of whole beats is rounded to them; otherwise
    // the pre-roll keeps the song's tempo and the first bar is a short one,
    // rather than a tempo change nobody played. A downbeat on a locked grid
    // is exact, so there only a few ms may go: more would sit on every later
    // beat, for the first transition to soak up.
    let pickup_tolerance = if locked.get(first).copied().unwrap_or(false) {
        TRANSITION_TOLERANCE_SECONDS
    } else {
        MERGE_TOLERANCE_SECONDS
    };
    if whole >= 1.0 && (offset - whole).abs() * first_period <= pickup_tolerance {
        offset = whole;
        pre_bpm = 60.0 * whole / beats[0];
    } else if beats[0] < 1.0e-3 {
        offset = 0.0;
    }

    // Tempo: grow each segment while one constant tempo keeps every beat in
    // it within tolerance of where it was heard. Segments start where the map
    // (not the detection) puts their first beat, so rounding a tempo never
    // lets error build up along the song.
    let mut tempo = Vec::new();
    let mut at = beats[0];
    if offset > 0.0 {
        let pre = clean_bpm(pre_bpm, |bpm| {
            (offset * 60.0 / bpm - beats[0]).abs() <= MERGE_TOLERANCE_SECONDS
        });
        tempo.push((0.0, pre));
        at = offset * 60.0 / pre;
    }
    let n = beats.len();
    let mut a = 0usize;
    while a + 1 < n {
        let fits = |start: f64, period: f64, b: usize| {
            (a + 1..=b).all(|j| {
                let tol = tolerance[j].min(tolerance[j - 1]);
                (start + (j - a) as f64 * period - beats[j]).abs() <= tol
            })
        };
        let mut end = a + 1;
        for b in a + 2..n {
            if !fits(at, (beats[b] - at) / (b - a) as f64, b) {
                break;
            }
            end = b;
        }
        let bpm = 60.0 * (end - a) as f64 / (beats[end] - at);
        let bpm = clean_bpm(bpm, |bpm| fits(at, 60.0 / bpm, end));
        // The same tempo as the point before just carries on.
        if tempo
            .last()
            .is_none_or(|&(_, last): &(f64, f64)| last != bpm)
        {
            tempo.push((offset + a as f64, bpm));
        }
        at += (end - a) as f64 * 60.0 / bpm;
        a = end;
    }

    // Meter: the song's meter from the downbeat, and any bar the band played
    // with a different number of beats gets that count for one bar.
    let mut meter = vec![(0.0, bpb, 4)];
    if offset > 0.0 && (offset / bpb as f64).fract().abs() > 1.0e-9 {
        meter.push((offset, bpb, 4));
    }
    let downbeats: Vec<usize> = positions
        .iter()
        .enumerate()
        .filter(|(_, p)| **p == 1)
        .map(|(i, _)| i)
        .collect();
    for pair in downbeats.windows(2) {
        let count = (pair[1] - pair[0]) as u16;
        if count != bpb && count > 0 {
            meter.push((offset + pair[0] as f64, count, 4));
            meter.push((offset + pair[1] as f64, bpb, 4));
        }
    }
    meter.sort_by(|a, b| a.0.total_cmp(&b.0));
    // A later point at the same beat wins; and drop points that repeat the
    // meter already in force.
    let mut cleaned: Vec<(f64, u16, u16)> = Vec::new();
    for point in meter {
        match cleaned.last_mut() {
            Some(last) if (last.0 - point.0).abs() < 1.0e-9 => *last = point,
            _ => cleaned.push(point),
        }
    }
    let mut deduped: Vec<(f64, u16, u16)> = Vec::new();
    for point in cleaned {
        if deduped
            .last()
            .is_some_and(|l| l.1 == point.1 && l.2 == point.2)
            && (point.0 - offset).abs() > 1.0e-9
        {
            continue;
        }
        deduped.push(point);
    }
    Some(TempoMapPlan {
        tempo,
        meter: deduped,
        first_downbeat_beat: offset,
    })
}

/// Apply `plan` to `state`: replace the tempo and meter maps, keep every
/// audio clip at its wall-clock start, and set the analysed clip to native
/// speed when `native`. Returns the already-applied edit for the undo history,
/// or `None` when nothing changed. Pure over the state, so it is testable
/// without a window.
pub(crate) fn apply_tempo_plan(
    state: &mut TimelineState,
    plan: &TempoMapPlan,
    clip_id: &str,
    native: bool,
) -> Option<EditCommand> {
    let tempo_prev = TempoStateSnapshot::capture(state);
    let meter_prev = TimeSignatureStateSnapshot::capture(state);
    // Every audio clip's wall-clock start under the old map.
    let audio: Vec<(String, f64, ClipSnapshot)> = state
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .filter(|c| matches!(c.clip_type, ClipType::Audio { .. }))
        .filter_map(|c| {
            Some((
                c.id.clone(),
                state.seconds_at_beat(c.start_beat.max(0.0) as f64),
                ClipSnapshot::capture(state, &c.id)?,
            ))
        })
        .collect();

    if native {
        if let Some(mut stretch) = state.clip_stretch(clip_id).cloned() {
            stretch.mode = StretchMode::Off;
            stretch.warp_markers.clear();
            stretch.dirty = true;
            state.set_clip_stretch(clip_id, stretch);
        }
    }
    let points: Vec<TempoPoint> = plan
        .tempo
        .iter()
        .map(|&(beat, bpm)| TempoPoint::new(beat, bpm, TempoCurve::Hold))
        .collect();
    let base_bpm = plan.tempo.first().map(|p| p.1).unwrap_or(state.bpm as f64);
    state.tempo_map = TempoMap::with_points(points);
    state.tempo_map.ensure_point_ids();
    state.bpm = base_bpm as f32;
    if let Some(&(_, num, den)) = plan.meter.first() {
        state
            .time_signature_map
            .reset_to_single_point(0.0, num, den);
    }
    for &(beat, num, den) in plan.meter.iter().skip(1) {
        state.time_signature_map.add_or_update_point(beat, num, den);
    }
    state.refresh_tempo_cache();
    // Audio stays on the clock.
    for (id, seconds, _) in &audio {
        let beat = state.beat_at_seconds(*seconds).max(0.0) as f32;
        for track in &mut state.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|c| &c.id == id) {
                clip.start_beat = beat;
            }
        }
    }
    state.reconcile_audio_clip_lengths();

    let tempo_next = TempoStateSnapshot::capture(state);
    let meter_next = TimeSignatureStateSnapshot::capture(state);
    let clips: Vec<(ClipSnapshot, ClipSnapshot)> = audio
        .into_iter()
        .filter_map(|(id, _, before)| {
            let after = ClipSnapshot::capture(state, &id)?;
            (after.clip != before.clip).then_some((before, after))
        })
        .collect();
    if tempo_next == tempo_prev && meter_next == meter_prev && clips.is_empty() {
        return None;
    }
    Some(EditCommand::MapTempo {
        label: "Map Tempo",
        tempo_prev,
        tempo_next,
        meter_prev,
        meter_next,
        clips,
    })
}

/// A detected chord to place: seconds on the source file's clock.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DetectedChord {
    pub start: f64,
    pub end: f64,
    pub chord: sphere_midi_service::chords::Chord,
}

impl StudioLayout {
    /// `(shift, scale)` with `project = shift + source × scale`: source-file
    /// seconds to project seconds for how the clip plays now (a stretched
    /// clip spreads its window over its drawn length). `None` when the clip
    /// is reversed — its audio runs backwards, so times read off the file do
    /// not describe the timeline.
    fn clip_source_to_project_seconds(&self, clip_id: &str, cx: &gpui::App) -> Option<(f64, f64)> {
        let state = &self.timeline.read(cx).state;
        let (_, clip) = state.find_clip(clip_id)?;
        if clip.stretch.reverse {
            return None;
        }
        let rate = clip.stretch.original_sample_rate.max(1) as f64;
        let window_start = clip.stretch.source_start_samples as f64 / rate;
        let window_seconds = clip.stretch.source_len_samples() as f64 / rate;
        let clip_start = state.seconds_at_beat(clip.start_beat.max(0.0) as f64);
        let clip_end =
            state.seconds_at_beat((clip.start_beat + clip.duration_beats).max(0.0) as f64);
        let scale = if clip.stretch.mode == StretchMode::Off || window_seconds <= 0.0 {
            1.0
        } else {
            ((clip_end - clip_start) / window_seconds).max(1e-6)
        };
        Some((clip_start - window_start * scale, scale))
    }

    /// Put the project's grid on the clip's detected beats. One undo step.
    pub(crate) fn map_tempo_to_clip(
        &mut self,
        clip_id: &str,
        source_beats: &[f64],
        positions: &[u32],
        locked: &[bool],
        beats_per_bar: u32,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        // The clip must play its audio at native speed for its beats to be
        // the project's beats: a tempo-synced or warped clip is stretched by
        // the very map being replaced.
        let needs_native = {
            let state = &self.timeline.read(cx).state;
            let (_, clip) = state
                .find_clip(clip_id)
                .ok_or_else(|| "The clip is gone".to_string())?;
            if !matches!(clip.clip_type, ClipType::Audio { .. }) {
                return Err("Pick an audio clip".into());
            }
            if clip.stretch.reverse {
                return Err("Turn off Reverse on the clip first".into());
            }
            clip.stretch.follows_project_tempo() || clip.stretch.mode != StretchMode::Off
        };
        // After mapping the clip plays at native speed, so its beats map 1:1.
        let shift = {
            let state = &self.timeline.read(cx).state;
            let (_, clip) = state.find_clip(clip_id).unwrap();
            let rate = clip.stretch.original_sample_rate.max(1) as f64;
            state.seconds_at_beat(clip.start_beat.max(0.0) as f64)
                - clip.stretch.source_start_samples as f64 / rate
        };
        let (window_start, window_end) = {
            let state = &self.timeline.read(cx).state;
            let (_, clip) = state.find_clip(clip_id).unwrap();
            let rate = clip.stretch.original_sample_rate.max(1) as f64;
            let start = clip.stretch.source_start_samples as f64 / rate;
            let end = if clip.stretch.source_end_samples > clip.stretch.source_start_samples {
                clip.stretch.source_end_samples as f64 / rate
            } else {
                f64::MAX
            };
            (start, end)
        };
        // Beats inside the clip, on the project clock.
        let kept: Vec<(f64, u32, bool)> = source_beats
            .iter()
            .zip(positions)
            .enumerate()
            .filter(|(_, (s, _))| **s >= window_start - 1e-6 && **s < window_end)
            .map(|(i, (s, p))| (s + shift, *p, locked.get(i).copied().unwrap_or(false)))
            .collect();
        let seconds: Vec<f64> = kept.iter().map(|k| k.0).collect();
        let kept_positions: Vec<u32> = kept.iter().map(|k| k.1).collect();
        let kept_locked: Vec<bool> = kept.iter().map(|k| k.2).collect();
        let plan = plan_tempo_map(&seconds, &kept_positions, &kept_locked, beats_per_bar)
            .ok_or_else(|| "Not enough beats inside the clip to map".to_string())?;

        let changed = self.timeline.update(cx, |timeline, cx| {
            let Some(command) = apply_tempo_plan(&mut timeline.state, &plan, clip_id, needs_native)
            else {
                return false;
            };
            timeline.record_executed_command(command, cx);
            cx.notify();
            true
        });
        if changed {
            self.mark_dirty();
            self.sync_tempo_map_to_engine(cx);
            self.sync_time_signature_map_to_engine(cx);
            self.push_project_settings_snapshot_to_window(cx);
            cx.notify();
        }
        let points = plan.tempo.len();
        Ok(if needs_native {
            format!("Tempo mapped — {points} tempo points · clip set to play at native speed")
        } else {
            format!("Tempo mapped — {points} tempo points")
        })
    }

    /// Place a clip's detected chords on the Chord Track at the positions
    /// they sound. One undo step.
    pub(crate) fn place_detected_chords(
        &mut self,
        clip_id: &str,
        chords: &[DetectedChord],
        flats: bool,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let (shift, scale) = self
            .clip_source_to_project_seconds(clip_id, cx)
            .ok_or_else(|| "Turn off Reverse on the clip first".to_string())?;
        let placements: Vec<(f64, f64, sphere_midi_service::chords::Chord)> = {
            let state = &self.timeline.read(cx).state;
            chords
                .iter()
                .filter_map(|c| {
                    let start = state.beat_at_seconds(shift + c.start * scale);
                    let end = state.beat_at_seconds(shift + c.end * scale);
                    (end > start + 1.0e-3).then_some((start, end - start, c.chord))
                })
                .collect()
        };
        if placements.is_empty() {
            return Err("No chords to place".into());
        }
        let count = placements.len();
        self.set_chord_track_visible(true, cx);
        self.edit_chords(
            "Add Detected Chords",
            move |timeline| {
                for (start, length, chord) in &placements {
                    timeline.state.place_chords(
                        *start,
                        &[
                            crate::components::timeline::timeline_state::ChordPlacement {
                                chord: *chord,
                                flats,
                                length_beats: *length,
                            },
                        ],
                    );
                }
            },
            cx,
        );
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steady(bpm: f64, start: f64, count: usize) -> Vec<f64> {
        (0..count).map(|i| start + i as f64 * 60.0 / bpm).collect()
    }

    fn bars(count: usize, per_bar: u32, pickup: usize) -> Vec<u32> {
        (0..count)
            .map(|i| {
                let i = i as i64 - pickup as i64;
                (i.rem_euclid(per_bar as i64) + 1) as u32
            })
            .collect()
    }

    #[test]
    fn a_steady_song_is_one_tempo_and_starts_its_bar_on_the_downbeat() {
        // 120 BPM, first downbeat 2.0 s in: exactly 4 beats of pre-roll.
        let beats = steady(120.0, 2.0, 64);
        let plan = plan_tempo_map(&beats, &bars(64, 4, 0), &[], 4).unwrap();
        assert_eq!(plan.first_downbeat_beat, 4.0);
        assert_eq!(plan.tempo, vec![(0.0, 120.0)]);
        // The downbeat is on a bar line already: no extra meter point.
        assert_eq!(plan.meter, vec![(0.0, 4, 4)]);
    }

    #[test]
    fn a_pickup_that_is_not_a_whole_bar_starts_a_new_bar_at_the_downbeat() {
        // Downbeat 0.35 s in at 120 BPM: 0.7 beats of pre-roll.
        let beats = steady(120.0, 0.35, 32);
        let plan = plan_tempo_map(&beats, &bars(32, 4, 0), &[], 4).unwrap();
        assert!((plan.first_downbeat_beat - 0.7).abs() < 1e-9);
        assert!(plan.meter.contains(&(0.7, 4, 4)), "{:?}", plan.meter);
        // Pre-roll plays at the song's own tempo.
        assert!((plan.tempo[0].1 - 120.0).abs() < 1e-6);
    }

    #[test]
    fn a_tempo_change_gets_its_own_tempo_point_on_the_right_beat() {
        let mut beats = steady(90.0, 1.0, 32);
        let last = *beats.last().unwrap();
        beats.extend(steady(140.0, last + 60.0 / 90.0, 32));
        let plan = plan_tempo_map(&beats, &bars(64, 4, 0), &[], 4).unwrap();
        let bpms: Vec<f64> = plan.tempo.iter().map(|p| p.1).collect();
        assert!(bpms.iter().any(|b| (b - 90.0).abs() < 0.01), "{bpms:?}");
        assert!(bpms.iter().any(|b| (b - 140.0).abs() < 0.01), "{bpms:?}");
        let change = plan
            .tempo
            .iter()
            .find(|p| (p.1 - 140.0).abs() < 0.01)
            .unwrap();
        assert!((change.0 - (plan.first_downbeat_beat + 32.0)).abs() < 1e-9);
    }

    #[test]
    fn every_beat_stays_within_tolerance_of_the_map() {
        // A drifting live take: tempo wanders ±3 %.
        let mut beats = vec![0.8];
        for i in 1..96 {
            let bpm = 100.0 * (1.0 + 0.03 * (i as f64 * 0.21).sin());
            beats.push(beats[i - 1] + 60.0 / bpm);
        }
        let positions = bars(96, 4, 0);
        let plan = plan_tempo_map(&beats, &positions, &[], 4).unwrap();
        let points: Vec<TempoPoint> = plan
            .tempo
            .iter()
            .map(|&(beat, bpm)| TempoPoint::new(beat, bpm, TempoCurve::Hold))
            .collect();
        let map = TempoMap::with_points(points);
        for (i, t) in beats.iter().enumerate() {
            let beat = plan.first_downbeat_beat + i as f64;
            let at = map.seconds_at_beat(beat, plan.tempo[0].1);
            assert!(
                (at - t).abs() <= MERGE_TOLERANCE_SECONDS + 1e-6,
                "beat {i}: {at} vs {t}"
            );
        }
        assert!(
            plan.tempo.len() < beats.len(),
            "merging should reduce points"
        );
    }

    #[test]
    fn a_click_track_with_a_few_ms_of_jitter_maps_as_one_whole_tempo() {
        // 120 BPM with ±6 ms of detection wobble, then a real move to 130.
        let jitter = |i: usize| 0.006 * ((i * 7919) % 13) as f64 / 6.0 - 0.006;
        let mut beats: Vec<f64> = (0..96).map(|i| 2.0 + i as f64 * 0.5 + jitter(i)).collect();
        let last = 2.0 + 95.0 * 0.5;
        beats.extend((1..=64).map(|i| last + i as f64 * 60.0 / 130.0 + jitter(i)));
        let plan = plan_tempo_map(&beats, &bars(160, 4, 0), &[], 4).unwrap();
        let bpms: Vec<f64> = plan.tempo.iter().map(|p| p.1).collect();
        assert_eq!(bpms, vec![120.0, 130.0], "{:?}", plan.tempo);
    }

    #[test]
    fn a_pickup_just_off_a_whole_beat_keeps_the_song_tempo() {
        // First downbeat 0.536 s in at 120 BPM: 1.072 beats, 36 ms off a
        // whole beat. One tempo for the whole song, a short first bar.
        let beats = steady(120.0, 0.536, 64);
        let plan = plan_tempo_map(&beats, &bars(64, 4, 0), &[], 4).unwrap();
        assert_eq!(plan.tempo, vec![(0.0, 120.0)]);
        assert!((plan.first_downbeat_beat - 1.072).abs() < 1e-9);
        assert!(plan.meter.contains(&(plan.first_downbeat_beat, 4, 4)));
    }

    #[test]
    fn a_ritardando_bar_gets_a_tempo_per_beat_then_the_new_sections() {
        // A bar slowing 108 → 100 → 97 → 85 into 12 bars at 64, then 120.
        let mut beats = steady(108.0, 2.0, 8);
        let mut t = beats[7] + 60.0 / 108.0;
        for bpm in [108.0, 100.0, 97.0, 85.0] {
            beats.push(t);
            t += 60.0 / bpm;
        }
        beats.extend(steady(64.0, t, 48));
        let t = beats.last().unwrap() + 60.0 / 64.0;
        beats.extend(steady(120.0, t, 32));
        // As the analysis marks them: beats 10 and 11 fit neither grid.
        let locked: Vec<bool> = (0..beats.len()).map(|i| !(10..=11).contains(&i)).collect();
        let plan = plan_tempo_map(&beats, &bars(beats.len(), 4, 0), &locked, 4).unwrap();
        let bpms: Vec<f64> = plan.tempo.iter().map(|p| p.1).collect();
        assert_eq!(bpms, vec![108.0, 100.0, 97.0, 85.0, 64.0, 120.0]);
        // The slow section starts on bar 4's downbeat (bar 1 is the pickup).
        let slow = plan.tempo.iter().find(|p| p.1 == 64.0).unwrap();
        assert_eq!(slow.0, plan.first_downbeat_beat + 12.0);
    }

    #[test]
    fn a_locked_downbeat_10_ms_off_a_whole_beat_is_not_rounded() {
        // 108 BPM on a locked grid, first downbeat 2.212 s in (3.98 beats):
        // rounding to 4 beats would put every beat 10 ms late.
        let beats = steady(108.0, 2.212, 32);
        let locked = vec![true; beats.len()];
        let plan = plan_tempo_map(&beats, &bars(32, 4, 0), &locked, 4).unwrap();
        assert_eq!(plan.tempo, vec![(0.0, 108.0)]);
        assert!((plan.first_downbeat_beat - 2.212 * 108.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn a_short_bar_gets_its_own_meter_for_one_bar() {
        // 4/4, but bar 3 has only 2 beats.
        let beats = steady(120.0, 2.0, 30);
        let mut positions = Vec::new();
        for bar in 0..8 {
            let count = if bar == 2 { 2 } else { 4 };
            for p in 1..=count {
                positions.push(p);
            }
        }
        positions.truncate(30);
        let plan = plan_tempo_map(&beats, &positions, &[], 4).unwrap();
        assert!(plan.meter.contains(&(4.0 + 8.0, 2, 4)), "{:?}", plan.meter);
        assert!(plan.meter.contains(&(4.0 + 10.0, 4, 4)), "{:?}", plan.meter);
    }

    fn project_with_song(start_beat: f32) -> TimelineState {
        use crate::components::timeline::timeline_state::{
            AudioClipStretchState, AudioImportState, ClipState, CreateTrackOptions,
            InputMonitorMode, TrackType,
        };
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let track = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Song".into(),
            color: gpui::Rgba {
                r: 0.5,
                g: 0.5,
                b: 0.5,
                a: 1.0,
            },
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let clip = ClipState {
            id: "song".into(),
            name: "song".into(),
            start_beat,
            duration_beats: 120.0,
            source_duration_seconds: Some(60.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "song.wav".into(),
                source_path: Some("song.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState {
                original_sample_rate: 48_000,
                source_start_samples: 0,
                source_end_samples: 48_000 * 60,
                ..AudioClipStretchState::default()
            },
        };
        state
            .tracks
            .iter_mut()
            .find(|t| t.id == track)
            .unwrap()
            .clips
            .push(clip);
        state
    }

    #[test]
    fn mapping_keeps_the_audio_on_the_clock_and_undoes_exactly() {
        let mut state = project_with_song(8.0);
        let clip_seconds = state.seconds_at_beat(8.0);
        // The song plays at 96 BPM with its first downbeat 1.2 s into the clip.
        let beats: Vec<f64> = (0..64)
            .map(|i| clip_seconds + 1.2 + i as f64 * 60.0 / 96.0)
            .collect();
        let plan = plan_tempo_map(&beats, &bars(64, 4, 0), &[], 4).unwrap();
        let before = state.clone();
        let command = apply_tempo_plan(&mut state, &plan, "song", false).expect("changed");

        // The clip still starts at the same second...
        let (_, clip) = state.find_clip("song").unwrap();
        let now = state.seconds_at_beat(clip.start_beat as f64);
        assert!((now - clip_seconds).abs() < 1e-3, "{now} vs {clip_seconds}");
        // ...and every detected beat now sits on a project beat.
        for (i, t) in beats.iter().enumerate() {
            let beat = state.beat_at_seconds(*t);
            let expected = plan.first_downbeat_beat + i as f64;
            assert!(
                (beat - expected).abs() < 0.01,
                "beat {i}: {beat} vs {expected}"
            );
        }
        assert!((state.bpm - 96.0).abs() < 0.01);

        command.undo(&mut state);
        assert_eq!(state.tempo_map.points, before.tempo_map.points);
        assert_eq!(state.bpm, before.bpm);
        assert_eq!(
            state.find_clip("song").unwrap().1.start_beat,
            before.find_clip("song").unwrap().1.start_beat
        );
        command.execute(&mut state);
        let (_, clip) = state.find_clip("song").unwrap();
        assert!((state.seconds_at_beat(clip.start_beat as f64) - clip_seconds).abs() < 1e-3);
    }
}
