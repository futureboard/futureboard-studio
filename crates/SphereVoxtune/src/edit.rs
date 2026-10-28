//! Note edit operations, shared by every editor front end.
//!
//! These mirror the Android editor (`lib/music.dart`, `lib/main.dart`) so a
//! project edited on the phone and on the desktop behaves the same: a cut
//! re-centres each half on its own part of the sung contour, a join keeps the
//! sung curve, and the macros (center, drift, quantize, transpose, level,
//! accent on beats) change exactly the notes the phone would.
//!
//! Everything here works on plain note lists; nothing touches the engine or a
//! device, so callers apply the result with [`crate::CantoEngine::set_notes`].

use crate::engine::{
    CONTOUR_POINTS, CantoNote, ORN_FALL, ORN_NONE, ORN_TURN_DOWN, ORN_TURN_UP, solfege_for_midi,
};
use crate::scale::{Scale, scale_mask};

/// Shortest note a timing edit may leave, in seconds.
pub const MIN_NOTE_SEC: f32 = 0.06;
/// Space kept between neighbouring sung notes by timing edits.
pub const NOTE_GAP_SEC: f32 = 0.012;
/// Shortest half a cut may leave.
pub const MIN_CUT_SEC: f32 = 0.04;
/// Ornament width and speed a note gets when a style is first chosen.
pub const DEFAULT_ORN_DEPTH: f32 = 1.0;
pub const DEFAULT_ORN_SPEED: f32 = 0.5;

fn sample_curve(curve: &[f32], t: f32) -> f32 {
    match curve.len() {
        0 => 0.0,
        1 => curve[0],
        n => {
            let pos = t.clamp(0.0, 1.0) * (n - 1) as f32;
            let i = (pos.floor() as usize).min(n - 1);
            let j = (i + 1).min(n - 1);
            curve[i] + (curve[j] - curve[i]) * (pos - i as f32)
        }
    }
}

/// A fresh id derived from `base` that no note in `notes` uses yet.
pub fn unique_id(notes: &[CantoNote], base: &str, seq: &mut u32) -> String {
    loop {
        *seq += 1;
        let id = format!("{base}.{seq}");
        if !notes.iter().any(|n| n.id == id) {
            return id;
        }
    }
}

/// Refreshes every pitched note's solfège lyric after pitch or key changes.
pub fn relabel(notes: &mut [CantoNote], key: u8) {
    for n in notes.iter_mut().filter(|n| !n.unpitched) {
        n.lyric = solfege_for_midi(n.pitch, key);
    }
}

/// Nearest in-scale semitone to `midi` (searching ±4), like the phone's
/// `nearestScale`.
pub fn nearest_scale_note(midi: f32, key: u8, scale: Scale) -> i32 {
    let base = midi.round() as i32;
    if scale == Scale::Chromatic {
        return base;
    }
    let mask = scale_mask(key, scale);
    let mut best = base;
    let mut best_dist = f32::INFINITY;
    for note in base - 4..=base + 4 {
        if mask & (1 << note.rem_euclid(12)) == 0 {
            continue;
        }
        let dist = (note as f32 - midi).abs();
        if dist < best_dist {
            best_dist = dist;
            best = note;
        }
    }
    best
}

/// Cuts `note` at `time` (seconds, inside the note) into two notes that each
/// play their own part of the source. Each half is re-centred on its part of
/// the contour; the user's edits carry over. None when either half would be
/// shorter than `min_dur`.
pub fn split_note(
    note: &CantoNote,
    time: f32,
    ids: (String, String),
    min_dur: f32,
) -> Option<(CantoNote, CantoNote)> {
    if note.dur <= 0.0 {
        return None;
    }
    let r = (time - note.start) / note.dur;
    if note.dur * r < min_dur || note.dur * (1.0 - r) < min_dur {
        return None;
    }
    let offset = note.pitch - note.detected;
    let half = |id: String, from: f32, to: f32| -> CantoNote {
        let at = |k: usize| from + (to - from) * k as f32 / (CONTOUR_POINTS - 1) as f32;
        let envelope = if note.envelope.is_empty() {
            Vec::new()
        } else {
            (0..CONTOUR_POINTS)
                .map(|k| sample_curve(&note.envelope, at(k)))
                .collect()
        };
        let (detected, contour) = if note.contour.is_empty() || note.unpitched {
            (note.detected, Vec::new())
        } else {
            let c: Vec<f32> = (0..CONTOUR_POINTS)
                .map(|k| sample_curve(&note.contour, at(k)))
                .collect();
            // This half's mean deviation becomes its detected pitch.
            let mean = c.iter().sum::<f32>() / c.len() as f32;
            (
                note.detected + mean / 100.0,
                c.iter().map(|v| v - mean).collect(),
            )
        };
        CantoNote {
            id,
            detected,
            pitch: detected + offset,
            detected_start: note.detected_start + note.detected_dur * from,
            start: note.start + note.dur * from,
            detected_dur: note.detected_dur * (to - from),
            dur: note.dur * (to - from),
            // The onset flick and accent stay with the first half, the fall
            // with the second; a shake continues in both.
            orn_style: match note.orn_style {
                ORN_TURN_UP | ORN_TURN_DOWN if from != 0.0 => ORN_NONE,
                ORN_FALL if to != 1.0 => ORN_NONE,
                s => s,
            },
            accent: if from == 0.0 { note.accent } else { 0.0 },
            contour,
            envelope,
            ..note.clone()
        }
    };
    Some((half(ids.0, 0.0, r), half(ids.1, r, 1.0)))
}

/// Joins `a` with the following note `b` into one note spanning both
/// (including any gap). The centre is the duration-weighted mean and the
/// contour is rebuilt relative to it, so the sung curve is unchanged.
pub fn join_notes(a: &CantoNote, b: &CantoNote, id: String) -> CantoNote {
    let start = a.start;
    let dur = b.start + b.dur - start;
    let det_start = a.detected_start;
    let det_dur = b.detected_start + b.detected_dur - det_start;
    let weight = (a.dur + b.dur).max(1e-6);
    let detected = (a.detected * a.dur + b.detected * b.dur) / weight;
    let mut contour = Vec::with_capacity(CONTOUR_POINTS);
    let mut envelope = Vec::with_capacity(CONTOUR_POINTS);
    for k in 0..CONTOUR_POINTS {
        // Position in source time, so the curve follows the audio.
        let t = det_start + det_dur * k as f32 / (CONTOUR_POINTS - 1) as f32;
        let n = if t <= a.detected_start + a.detected_dur {
            a
        } else {
            b
        };
        let u = ((t - n.detected_start) / n.detected_dur.max(1e-6)).clamp(0.0, 1.0);
        let cents = if n.contour.is_empty() {
            0.0
        } else {
            sample_curve(&n.contour, u)
        };
        contour.push(cents + (n.detected - detected) * 100.0);
        envelope.push(if n.envelope.is_empty() {
            1.0
        } else {
            sample_curve(&n.envelope, u)
        });
    }
    let offset = ((a.pitch - a.detected) * a.dur + (b.pitch - b.detected) * b.dur) / weight;
    // The joined note starts like `a` and ends like `b`.
    let fall = b.orn_style == ORN_FALL;
    CantoNote {
        id,
        lyric: a.lyric.clone(),
        unpitched: a.unpitched && b.unpitched,
        detected,
        pitch: detected + offset,
        detected_start: det_start,
        start,
        detected_dur: det_dur,
        dur,
        modulation: (a.modulation + b.modulation) / 2.0,
        formant: (a.formant + b.formant) / 2.0,
        gain_db: (a.gain_db * a.dur + b.gain_db * b.dur) / weight,
        detected_gain: (a.detected_gain * a.dur + b.detected_gain * b.dur) / weight,
        pinned: a.pinned || b.pinned,
        muted: a.muted && b.muted,
        orn_style: if fall { ORN_FALL } else { a.orn_style },
        orn_depth: if fall { b.orn_depth } else { a.orn_depth },
        orn_speed: if fall { b.orn_speed } else { a.orn_speed },
        accent: a.accent,
        scoop: a.scoop,
        drift: a.drift,
        vib: a.vib,
        vib_cycles: a.vib_cycles,
        vib_phase: a.vib_phase,
        contour,
        envelope,
    }
}

/// Which part of a note a timing drag moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingEdge {
    Start,
    End,
    Body,
}

/// Keeps `notes[index]` inside the song and off its sung neighbours after a
/// timing edit, the same way the phone does.
pub fn clamp_timing(notes: &mut [CantoNote], index: usize, edge: TimingEdge, duration: f32) {
    let Some(id) = notes.get(index).map(|n| n.id.clone()) else {
        return;
    };
    let sung: Vec<usize> = (0..notes.len()).filter(|&i| !notes[i].unpitched).collect();
    let pos = sung.iter().position(|&i| notes[i].id == id);
    let prev = pos.and_then(|p| p.checked_sub(1)).map(|p| &notes[sung[p]]);
    let next = pos.and_then(|p| sung.get(p + 1)).map(|&i| &notes[i]);
    let min_start = prev.map_or(0.0, |p| p.start + p.dur + NOTE_GAP_SEC);
    let max_end = next.map_or(duration, |n| n.start - NOTE_GAP_SEC);
    let note = &mut notes[index];
    match edge {
        TimingEdge::Start => {
            let end = note.start + note.dur;
            note.start = note.start.max(min_start).min(end - MIN_NOTE_SEC);
            note.dur = (end - note.start).max(MIN_NOTE_SEC);
        }
        TimingEdge::End => {
            note.dur = note.dur.min(max_end - note.start).max(MIN_NOTE_SEC);
        }
        TimingEdge::Body => {
            note.dur = note.dur.max(MIN_NOTE_SEC);
            note.start = note.start.min(max_end - note.dur).max(min_start);
        }
    }
    if note.start < 0.0 {
        note.start = 0.0;
    }
    if note.start + note.dur > duration {
        note.dur = (duration - note.start).max(MIN_NOTE_SEC);
    }
}

/// Back to exactly what analysis found for this note.
pub fn reset_note(note: &mut CantoNote, key: u8) {
    note.pitch = note.detected;
    note.start = note.detected_start;
    note.dur = note.detected_dur;
    note.modulation = 1.0;
    note.formant = 0.0;
    note.gain_db = note.detected_gain;
    note.pinned = false;
    note.muted = false;
    note.orn_style = ORN_NONE;
    note.accent = 0.0;
    if !note.unpitched {
        note.lyric = solfege_for_midi(note.pitch, key);
    }
}

/// Sets a note's ornament style; a note getting its first style also gets the
/// default width and speed (an analysed note starts with both at zero).
pub fn set_ornament_style(note: &mut CantoNote, style: u8) {
    note.orn_style = if style <= ORN_FALL { style } else { ORN_NONE };
    if note.orn_style != ORN_NONE && note.orn_depth <= 0.0 {
        note.orn_depth = DEFAULT_ORN_DEPTH;
        if note.orn_speed <= 0.0 {
            note.orn_speed = DEFAULT_ORN_SPEED;
        }
    }
}

/// A macro from the Advanced tools.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Macro {
    /// Pull pitch centres toward the nearest scale note (0..=1).
    Center { amount: f32 },
    /// Flatten pitch drift / modulation (0..=1).
    Drift { amount: f32 },
    /// Pull note starts toward a beat grid (`grid_beats`, 1 = quarter).
    Quantize { amount: f32, grid_beats: f32 },
    /// Move pitched notes by whole semitones.
    Transpose { semitones: i32 },
    /// Pull note levels toward their mean (0..=1).
    Level { amount: f32 },
    /// Accent the notes that start on a beat.
    AccentBeats { amount: f32 },
}

impl Macro {
    /// History label code, as the phone writes it (`adv:transpose:2`, …).
    pub fn label(&self) -> String {
        match self {
            Macro::Center { .. } => "adv:center".into(),
            Macro::Drift { .. } => "adv:drift".into(),
            Macro::Quantize { .. } => "adv:quantize".into(),
            Macro::Transpose { semitones } => format!("adv:transpose:{semitones}"),
            Macro::Level { .. } => "adv:level".into(),
            Macro::AccentBeats { .. } => "adv:accentBeats".into(),
        }
    }
}

/// Musical context a macro needs.
#[derive(Debug, Clone, Copy)]
pub struct MacroContext {
    pub key: u8,
    pub scale: Scale,
    pub bpm: f32,
    pub duration: f32,
}

/// Runs `op` over `notes[only]` (or every note when None). Returns how many
/// notes changed. Lyrics are refreshed afterwards.
pub fn apply_macro(
    notes: &mut [CantoNote],
    only: Option<usize>,
    op: Macro,
    cx: MacroContext,
) -> usize {
    let scope: Vec<usize> = match only {
        Some(i) if i < notes.len() => vec![i],
        Some(_) => Vec::new(),
        None => (0..notes.len()).collect(),
    };
    let beat = 60.0 / cx.bpm.max(1.0);
    let mut count = 0;
    match op {
        Macro::Center { amount } => {
            for &i in &scope {
                let n = &mut notes[i];
                if n.unpitched || n.muted {
                    continue;
                }
                let target = nearest_scale_note(n.pitch, cx.key, cx.scale) as f32;
                let next = n.pitch + (target - n.pitch) * amount;
                if (next - n.pitch).abs() < 1e-4 {
                    continue;
                }
                n.pitch = next;
                n.pinned = true;
                count += 1;
            }
        }
        Macro::Drift { amount } => {
            for &i in &scope {
                let n = &mut notes[i];
                if n.unpitched || n.muted {
                    continue;
                }
                let next = (n.modulation * (1.0 - amount)).clamp(0.0, 1.45);
                if (next - n.modulation).abs() < 1e-4 {
                    continue;
                }
                n.modulation = next;
                count += 1;
            }
        }
        Macro::Quantize { amount, grid_beats } => {
            let grid = beat * grid_beats.max(1.0 / 64.0);
            let mut order = scope.clone();
            order.sort_by(|&a, &b| notes[a].start.total_cmp(&notes[b].start));
            for i in order {
                if notes[i].muted {
                    continue;
                }
                let before = notes[i].start;
                let snapped = (before / grid).round() * grid;
                notes[i].start += (snapped - before) * amount;
                clamp_timing(notes, i, TimingEdge::Body, cx.duration);
                if (notes[i].start - before).abs() > 1e-4 {
                    count += 1;
                }
            }
        }
        Macro::Transpose { semitones } => {
            for &i in &scope {
                let n = &mut notes[i];
                if n.unpitched {
                    continue;
                }
                n.pitch += semitones as f32;
                n.pinned = true;
                count += 1;
            }
        }
        Macro::Level { amount } => {
            let voiced: Vec<usize> = scope.iter().copied().filter(|&i| !notes[i].muted).collect();
            if voiced.len() >= 2 {
                let mean =
                    voiced.iter().map(|&i| notes[i].gain_db).sum::<f32>() / voiced.len() as f32;
                for i in voiced {
                    let n = &mut notes[i];
                    let next = (n.gain_db + (mean - n.gain_db) * amount).clamp(-18.0, 6.0);
                    if (next - n.gain_db).abs() < 1e-3 {
                        continue;
                    }
                    n.gain_db = next;
                    count += 1;
                }
            }
        }
        Macro::AccentBeats { amount } => {
            // Notes that start on a beat (within 60 ms) get the accent.
            for &i in &scope {
                let n = &mut notes[i];
                if n.unpitched || n.muted {
                    continue;
                }
                let off_beat = (n.start - (n.start / beat).round() * beat).abs();
                if off_beat > 0.06 || (n.accent - amount).abs() < 1e-3 {
                    continue;
                }
                n.accent = amount;
                count += 1;
            }
        }
    }
    relabel(notes, cx.key);
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, start: f32, dur: f32, detected: f32) -> CantoNote {
        CantoNote {
            id: id.into(),
            lyric: String::new(),
            unpitched: false,
            detected,
            pitch: detected,
            detected_start: start,
            start,
            detected_dur: dur,
            dur,
            modulation: 1.0,
            formant: 0.0,
            gain_db: 0.0,
            detected_gain: 0.0,
            pinned: false,
            muted: false,
            orn_style: ORN_NONE,
            orn_depth: 0.0,
            orn_speed: 0.0,
            accent: 0.0,
            scoop: 0.0,
            drift: 0.0,
            vib: 0.0,
            vib_cycles: 0.0,
            vib_phase: 0.0,
            contour: vec![0.0; CONTOUR_POINTS],
            envelope: vec![1.0; CONTOUR_POINTS],
        }
    }

    #[test]
    fn cut_recentres_each_half_on_its_own_pitch() {
        // A slide from -100 c to +100 c around A4.
        let mut n = note("n1", 1.0, 1.0, 69.0);
        n.contour = (0..CONTOUR_POINTS)
            .map(|k| -100.0 + 200.0 * k as f32 / (CONTOUR_POINTS - 1) as f32)
            .collect();
        n.pitch = 70.0; // user edit: +1 st
        n.accent = 0.5;
        n.orn_style = ORN_FALL;
        let (a, b) = split_note(&n, 1.5, ("a".into(), "b".into()), MIN_CUT_SEC).unwrap();
        assert!((a.detected - 68.5).abs() < 0.05, "{}", a.detected);
        assert!((b.detected - 69.5).abs() < 0.05, "{}", b.detected);
        assert!((a.pitch - a.detected - 1.0).abs() < 1e-4);
        assert_eq!((a.start, b.start), (1.0, 1.5));
        assert_eq!((a.accent, b.accent), (0.5, 0.0));
        assert_eq!((a.orn_style, b.orn_style), (ORN_NONE, ORN_FALL));
        assert!(split_note(&n, 1.01, ("x".into(), "y".into()), MIN_CUT_SEC).is_none());
    }

    #[test]
    fn join_then_cut_keeps_the_sung_curve() {
        let a = note("a", 0.0, 0.5, 60.0);
        let b = note("b", 0.5, 0.5, 62.0);
        let j = join_notes(&a, &b, "j".into());
        assert_eq!((j.start, j.dur), (0.0, 1.0));
        assert!((j.detected - 61.0).abs() < 1e-4);
        // First point sits on a (-100 c), last on b (+100 c).
        assert!((j.contour[0] + 100.0).abs() < 1e-3);
        assert!((j.contour[CONTOUR_POINTS - 1] - 100.0).abs() < 1e-3);
    }

    #[test]
    fn timing_clamp_keeps_notes_apart() {
        let mut notes = vec![note("a", 0.0, 0.5, 60.0), note("b", 0.6, 0.5, 62.0)];
        notes[1].start = 0.2;
        clamp_timing(&mut notes, 1, TimingEdge::Body, 2.0);
        assert!((notes[1].start - (0.5 + NOTE_GAP_SEC)).abs() < 1e-5);
        notes[0].dur = 5.0;
        clamp_timing(&mut notes, 0, TimingEdge::End, 2.0);
        assert!(notes[0].dur <= notes[1].start - NOTE_GAP_SEC + 1e-5);
    }

    #[test]
    fn macros_change_what_the_phone_changes() {
        let cx = MacroContext {
            key: 0,
            scale: Scale::Major,
            bpm: 120.0,
            duration: 4.0,
        };
        let mut notes = vec![note("a", 0.02, 0.4, 61.3), note("b", 0.73, 0.4, 64.2)];
        // 61.3 is between C (60) and D (62) in C major: D is nearer.
        assert_eq!(
            apply_macro(&mut notes, None, Macro::Center { amount: 1.0 }, cx),
            2
        );
        assert_eq!(notes[0].pitch, 62.0);
        assert_eq!(notes[1].pitch, 64.0);
        assert!(notes[0].pinned);
        assert_eq!(notes[0].lyric, "re");

        let n = apply_macro(
            &mut notes,
            Some(1),
            Macro::Quantize {
                amount: 1.0,
                grid_beats: 0.5,
            },
            cx,
        );
        assert_eq!(n, 1);
        assert!((notes[1].start - 0.75).abs() < 1e-5);

        assert_eq!(
            apply_macro(&mut notes, None, Macro::AccentBeats { amount: 0.8 }, cx),
            1
        );
        assert_eq!((notes[0].accent, notes[1].accent), (0.8, 0.0));
        assert_eq!(
            Macro::Transpose { semitones: -2 }.label(),
            "adv:transpose:-2"
        );
    }
}
