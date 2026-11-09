//! CANTO Vocal Audio Engine, Real-Time TD-PSOLA Processor & Audio Analyzer.
//!
//! Decodes real audio files (`audio_io`), analyzes pitch contours, note segments,
//! musical key/scale, and BPM (`voicetune` + Krumhansl-Kessler key profiles), and
//! streams real-time TD-PSOLA pitch/formant/time/level shifted audio via `Shifter`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use serde::{Deserialize, Serialize};

use crate::audio_io::{DecodedAudio, decode_audio_bytes_to_mono, decode_audio_file_to_mono};
use crate::backing::Backing;
use crate::dsp_core::{SoftKneeCompressor, StereoEffect, clamp, db_to_linear};
use crate::scale::{ALL_NOTES, Scale, nearest_target, refine_scale, scale_mask};
use crate::shifter::{Control, MAX_FORMANT, Shifter};
use crate::stretch::Stretcher;
use crate::voicetune::{PitchFrame, calculate_rms, hz_to_midi_cents};
use crate::whitesharp_dsp::WhiteSharpDsp;

pub const DEFAULT_BPM: f32 = 120.0;
pub const BEATS_PER_BAR: f32 = 4.0;
pub const CONTOUR_POINTS: usize = 32;
/// Ramp length for note-preview starts, loop points and stops.
const AUDITION_FADE_SEC: f32 = 0.008;
/// Length change below which a note is read directly instead of stretched.
const STRETCH_EPSILON: f64 = 0.002;
/// Crossfade over a jump in the source read position during playback.
const SPLICE_FADE_SEC: f32 = 0.006;
/// Source movement per sample that still counts as continuous (a stretched
/// note's source runs at its own speed).
const SPLICE_TOLERANCE_SEC: f64 = 0.004;

/// What fades out across a splice.
#[derive(Debug, Clone, Copy)]
enum Tail {
    Silence,
    /// The source read straight on from here (seconds).
    Source(f64),
    /// This stretched note, carried on by the tail stretcher.
    Stretched(usize),
}

/// Key names by pitch class, as analysis reports them.
pub const KEY_NAMES: [&str; 12] = [
    "C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B",
];

const SOLFEGE_SYLLABLES: [&str; 12] = [
    "do", "di", "re", "me", "mi", "fa", "fi", "sol", "le", "la", "te", "ti",
];

// Krumhansl-Kessler tonal hierarchy profiles borrowed from `SphereAudioProcessor::analysis::key`.
const MAJOR_PROFILE: [f32; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const MINOR_PROFILE: [f32; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CantoNote {
    pub id: String,
    pub lyric: String,
    pub unpitched: bool,
    pub detected: f32,
    pub pitch: f32,
    pub detected_start: f32,
    pub start: f32,
    pub detected_dur: f32,
    pub dur: f32,
    pub modulation: f32,
    pub formant: f32,
    pub gain_db: f32,
    pub detected_gain: f32,
    pub pinned: bool,
    /// Cut from the output: plays silence over its span. Older saves lack it.
    #[serde(default)]
    pub muted: bool,
    /// ลูกคอ (vocal ornament) style, one of `ORN_*`. Older saves lack it.
    #[serde(default)]
    pub orn_style: u8,
    /// Ornament width in semitones.
    #[serde(default)]
    pub orn_depth: f32,
    /// Ornament speed 0 (slow, wide) ..= 1 (fast, tight).
    #[serde(default)]
    pub orn_speed: f32,
    /// Onset emphasis 0..=1: up to +6 dB over the note's attack.
    #[serde(default)]
    pub accent: f32,
    pub scoop: f32,
    pub drift: f32,
    pub vib: f32,
    pub vib_cycles: f32,
    pub vib_phase: f32,
    /// Real analyzed pitch contour in cents relative to `detected` (`CONTOUR_POINTS` samples across `0..1`).
    pub contour: Vec<f32>,
    /// Real analyzed normalized amplitude envelope (`CONTOUR_POINTS` samples across `0..1`).
    pub envelope: Vec<f32>,
}

impl CantoNote {
    pub fn present(&self, bypass: bool) -> Self {
        if !bypass {
            return self.clone();
        }
        Self {
            pitch: self.detected,
            start: self.detected_start,
            dur: self.detected_dur,
            modulation: 1.0,
            formant: 0.0,
            gain_db: self.detected_gain,
            orn_style: ORN_NONE,
            accent: 0.0,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CantoAnalysisResult {
    pub ok: bool,
    pub error: Option<String>,
    pub sample_rate: u32,
    pub duration_sec: f64,
    pub bpm: f32,
    pub key: String,
    pub mode: String,
    pub notes: Vec<CantoNote>,
    pub waveform_peaks: Vec<f32>,
    /// The sung pitch through the take; older cached results have none.
    #[serde(default)]
    pub pitch: PitchTrack,
}

/// The pitch actually sung, one value per analysis hop: MIDI, or 0 where
/// the voice is unvoiced or silent. A note's `contour` is a smoothed,
/// clamped 32-point summary of this; rendering needs the real thing, since
/// grains cut at the wrong period crackle and leave vibrato behind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PitchTrack {
    pub midi: Vec<f32>,
    /// Seconds between values.
    pub hop_sec: f32,
    /// Time of the first value (the centre of its analysis window).
    pub offset_sec: f32,
}

impl PitchTrack {
    fn from_frames(frames: &[PitchFrame], sample_rate: u32) -> Self {
        let midi = frames
            .iter()
            .map(|f| {
                if f.frequency_hz > 0.0 && f.confidence >= PITCH_VOICED {
                    (69.0 + 12.0 * (f.frequency_hz / 440.0).log2()) as f32
                } else {
                    0.0
                }
            })
            .collect();
        // Each value describes the stretch YIN compared: the first `span`
        // samples of the window against the same shifted by one period, so
        // its centre sits at (span + period) / 2, not mid-window. Using the
        // take's typical period keeps every value within ~2 ms of its time.
        let sr = sample_rate as f64;
        let mut periods: Vec<f64> = frames
            .iter()
            .filter(|f| f.frequency_hz > 0.0 && f.confidence >= PITCH_VOICED)
            .map(|f| sr / f.frequency_hz)
            .collect();
        periods.sort_by(f64::total_cmp);
        let period = periods
            .get(periods.len() / 2)
            .copied()
            .unwrap_or(sr / 220.0);
        let span = yin_span(sample_rate) as f64;
        Self {
            midi,
            hop_sec: pitch_hop(sample_rate) as f32 / sample_rate as f32,
            offset_sec: ((span + period) / 2.0 / sr) as f32,
        }
    }

    /// The sung pitch at `time` seconds, interpolated between hops; None
    /// where either neighbour is unvoiced or `time` is outside the track.
    /// Allocation-free: called on the audio thread.
    #[inline]
    pub fn at(&self, time: f32) -> Option<f32> {
        if self.midi.len() < 2 || self.hop_sec <= 0.0 {
            return None;
        }
        let pos = (time - self.offset_sec) / self.hop_sec;
        if !(0.0..(self.midi.len() - 1) as f32).contains(&pos) {
            return None;
        }
        let i = pos as usize;
        let (a, b) = (self.midi[i], self.midi[i + 1]);
        if a <= 0.0 || b <= 0.0 {
            return None;
        }
        Some(a + (b - a) * (pos - i as f32))
    }
}

#[inline]
pub fn midi_to_hz(midi: f32) -> f32 {
    440.0 * 2.0f32.powf((midi - 69.0) / 12.0)
}

#[inline]
pub fn smoothstep(value: f32) -> f32 {
    let x = clamp(value, 0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

#[inline]
fn sample_curve(curve: &[f32], t: f32) -> f32 {
    if curve.is_empty() {
        return 0.0;
    }
    if curve.len() == 1 {
        return curve[0];
    }
    let pos = clamp(t, 0.0, 1.0) * (curve.len() - 1) as f32;
    let idx = (pos.floor() as usize).min(curve.len() - 1);
    let next = (idx + 1).min(curve.len() - 1);
    let frac = pos - idx as f32;
    curve[idx] + (curve[next] - curve[idx]) * frac
}

pub fn note_energy(note: &CantoNote, t: f32) -> f32 {
    if !note.envelope.is_empty() {
        return sample_curve(&note.envelope, t).clamp(0.22, 1.0);
    }
    let attack = if note.unpitched { 0.4 } else { 0.07 };
    let release = if note.unpitched { 0.45 } else { 0.18 };
    let mut env = 1.0;
    if t < attack {
        env = smoothstep(t / attack);
    } else if t > 1.0 - release {
        env = smoothstep((1.0 - t) / release);
    }
    if !note.unpitched {
        env *= 0.84 + 0.16 * (clamp(t, 0.0, 1.0) * std::f32::consts::PI).sin();
    }
    env.max(if note.unpitched { 0.35 } else { 0.22 })
}

pub fn contour_cents(note: &CantoNote, t: f32, modulation: f32) -> f32 {
    if note.unpitched {
        return 0.0;
    }
    if !note.contour.is_empty() {
        return sample_curve(&note.contour, t) * modulation;
    }
    let mut cents = 0.0;
    let scoop_len = 0.18;
    if note.scoop.abs() > 0.001 && t < scoop_len {
        let u = t / scoop_len;
        cents += note.scoop * (1.0 - smoothstep(u));
    }
    if note.drift.abs() > 0.001 {
        cents += note.drift * (t - 0.35);
    }
    if note.vib.abs() > 0.001 {
        let env = clamp((t - 0.16) / 0.28, 0.0, 1.0);
        let cycles = if note.vib_cycles <= 0.0 {
            4.0
        } else {
            note.vib_cycles
        };
        cents += ((t * cycles + note.vib_phase) * std::f32::consts::TAU).sin() * note.vib * env;
    }
    cents * modulation
}

pub fn pitch_at(note: &CantoNote, t: f32) -> f32 {
    note.pitch + contour_cents(note, t, note.modulation) / 100.0 + ornament_semis(note, t)
}

/// ลูกคอ styles (Thai vocal ornaments), added on top of the sung contour.
pub const ORN_NONE: u8 = 0;
/// Quick flick up to the upper neighbour and back, right after the onset.
pub const ORN_TURN_UP: u8 = 1;
/// The same flick down to the lower neighbour.
pub const ORN_TURN_DOWN: u8 = 2;
/// Alternates with the upper neighbour through the note (trill-like).
pub const ORN_SHAKE: u8 = 3;
/// Drops away at the end of the note.
pub const ORN_FALL: u8 = 4;

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * clamp(t, 0.0, 1.0)
}

/// Raised-cosine bump: 0 outside `start..start + len`, 1 at its middle.
#[inline]
fn bump(t: f32, start: f32, len: f32) -> f32 {
    if len <= 0.0 || t <= start || t >= start + len {
        return 0.0;
    }
    0.5 - 0.5 * ((t - start) / len * std::f32::consts::TAU).cos()
}

/// Ornament pitch offset in semitones at normalized position `t`. Shapes are
/// timed in seconds (so speed sounds the same on long and short notes) and
/// shrink to fit short notes.
pub fn ornament_semis(note: &CantoNote, t: f32) -> f32 {
    if note.orn_style == ORN_NONE || note.unpitched || note.orn_depth == 0.0 {
        return 0.0;
    }
    let dur = note.dur.max(0.05);
    let sec = clamp(t, 0.0, 1.0) * dur;
    let depth = clamp(note.orn_depth, -4.0, 4.0);
    let speed = clamp(note.orn_speed, 0.0, 1.0);
    match note.orn_style {
        ORN_TURN_UP | ORN_TURN_DOWN => {
            let len = lerp(0.26, 0.09, speed).min(dur * 0.6);
            let start = 0.03f32.min(dur * 0.1);
            let sign = if note.orn_style == ORN_TURN_UP {
                1.0
            } else {
                -1.0
            };
            sign * depth * bump(sec, start, len)
        }
        ORN_SHAKE => {
            let hz = lerp(4.5, 9.0, speed);
            let start = (dur * 0.25).max(0.08).min(dur * 0.5);
            let end = dur - 0.05f32.min(dur * 0.1);
            if sec <= start || sec >= end {
                return 0.0;
            }
            let fade_in = smoothstep((sec - start) / 0.08);
            let fade_out = smoothstep((end - sec) / 0.05);
            let wave = 0.5 - 0.5 * ((sec - start) * hz * std::f32::consts::TAU).cos();
            depth * wave * fade_in * fade_out
        }
        ORN_FALL => {
            let len = lerp(0.32, 0.12, speed).min(dur * 0.5);
            -depth * smoothstep((sec - (dur - len)) / len)
        }
        _ => 0.0,
    }
}

/// Accent gain at normalized position `t`: a boost over the attack that
/// eases back to the note's own level.
pub fn accent_gain(note: &CantoNote, t: f32) -> f32 {
    if note.accent <= 0.0 || note.unpitched {
        return 1.0;
    }
    let dur = note.dur.max(0.05);
    let attack = (dur * 0.15).clamp(0.06, 0.2);
    let sec = clamp(t, 0.0, 1.0) * dur;
    let boost_db = 6.0 * clamp(note.accent, 0.0, 1.0) * (1.0 - smoothstep(sec / attack));
    db_to_linear(boost_db)
}

pub fn detected_pitch_at(note: &CantoNote, t: f32) -> f32 {
    note.detected + contour_cents(note, t, 1.0) / 100.0
}

pub fn solfege_for_midi(midi: f32, key_pc: u8) -> String {
    let pc = (midi.round() as i32).rem_euclid(12) as usize;
    let rel = (pc + 12 - (key_pc as usize % 12)) % 12;
    SOLFEGE_SYLLABLES[rel].to_string()
}

/// Live progress of an analysis running on another thread, and a way to
/// stop it early.
#[derive(Debug, Default)]
pub struct AnalysisProgress {
    /// 0..=1000 through pitch extraction (the slow part).
    pub permille: AtomicU32,
    pub cancel: AtomicBool,
}

/// What analysis finds in a vocal take.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub key: u8,
    pub scale: Scale,
    pub bpm: f32,
    pub notes: Vec<CantoNote>,
    /// Empty when the analysis came from a document rather than the audio:
    /// rendering then falls back to each note's contour.
    pub pitch: PitchTrack,
}

impl Analysis {
    /// Takes back an earlier result (the app's cache) for audio lasting
    /// `duration_sec`. None if it doesn't fit that audio, so a stale entry
    /// falls back to a fresh analysis.
    pub fn from_cached(result: CantoAnalysisResult, duration_sec: f64) -> Option<Self> {
        if !result.ok || (result.duration_sec - duration_sec).abs() > 0.05 {
            return None;
        }
        let key = KEY_NAMES.iter().position(|k| *k == result.key)? as u8;
        let scale = Scale::from_name(&result.mode).unwrap_or(Scale::Minor);
        Some(Self {
            key,
            scale,
            bpm: result.bpm.clamp(68.0, 172.0),
            notes: result.notes,
            pitch: result.pitch,
        })
    }
}

/// Pitch, notes, key and tempo of mono `samples`. Pure, so it runs without
/// the engine lock; returns None once `progress.cancel` is set.
pub fn analyze_samples(
    samples: &[f32],
    sample_rate: f32,
    progress: &AnalysisProgress,
) -> Option<Analysis> {
    let frames = extract_pitch_frames(samples, sample_rate as u32, progress)?;
    let (key, scale) = estimate_key_and_mode(&frames);
    let bpm = estimate_bpm_from_frames(&frames).clamp(68.0, 172.0);
    let notes = segment_canto_notes(&frames, key);
    let pitch = PitchTrack::from_frames(&frames, sample_rate as u32);
    progress.permille.store(1000, Ordering::Relaxed);
    Some(Analysis {
        key,
        scale,
        bpm,
        notes,
        pitch,
    })
}

/// Stereo frames `render_stereo_i16` renders per pass through its scratch.
/// Covers every usual Android burst in one pass; longer callbacks loop.
const I16_RENDER_CHUNK_FRAMES: usize = 1024;

/// Notes and bypass published by the editor engine and read by playback forks.
///
/// Several regions can play one modification. Each fork has its own playhead
/// and shifter, and pulls this list so a drag reaches every region without
/// copying samples on the audio thread. Bypass is one atomic load per block.
#[derive(Debug)]
struct NoteFeed {
    generation: AtomicU64,
    current: Mutex<Arc<Vec<CantoNote>>>,
    /// Previous lists the audio thread could not free. Cleared on the thread
    /// that publishes.
    retired: Mutex<Vec<Arc<Vec<CantoNote>>>>,
    bypass: AtomicBool,
    /// The editor's level meter. The source `Shared` reads this word; a
    /// reporting playback fork writes its block peak into it.
    editor_meter: Arc<AtomicU32>,
}

#[derive(Debug, Clone)]
pub struct CantoEngine {
    sample_rate: f32,
    pub bpm: f32,
    pub key: u8,
    pub scale: Scale,
    pub playing: bool,
    pub loop_enabled: bool,
    pub bypass: bool,
    /// Playback forks set this and adopt [`NoteFeed::bypass`] every block.
    /// The edited engine leaves it clear, so an offline render can force
    /// bypass off without publishing that to the forks.
    follow_shared_bypass: bool,
    pub playhead_sec: f64,
    pub correct_amount: f32,
    /// Decoded mono audio samples of the loaded vocal file (at `sample_rate`).
    /// Shared with playback forks so a second region does not copy the take.
    pub audio_samples: Arc<Vec<f32>>,
    /// Total duration of the loaded audio in seconds.
    pub duration_sec: f64,
    /// Downsampled peak envelope for the overview waveform.
    pub waveform_peaks: Vec<f32>,
    /// Analyzed and user-edited vocal notes.
    ///
    /// Replaced, not edited through a shared `Vec`, so a playback fork can
    /// keep rendering the previous list until it pulls the new one.
    pub notes: Arc<Vec<CantoNote>>,
    note_feed: Arc<NoteFeed>,
    /// `note_feed.generation` last applied to `notes`.
    seen_note_gen: u64,
    /// Real-time TD-PSOLA pitch & formant shifter.
    shifter: Shifter,
    /// Spreads a note whose length was edited over its new span, before
    /// the shifter, keeping its pitch.
    stretcher: Stretcher,
    /// Source position of the last dry sample in playback (None: silence).
    splice_src: Option<f64>,
    /// Stretched note the last dry sample came through, if any.
    splice_path: Option<usize>,
    /// A crossfade in progress: what is fading out, frames left, total.
    splice_fade: Option<(Tail, u32, u32)>,
    /// Keeps a stretched note going while it fades out of a splice.
    tail_stretcher: Stretcher,
    /// Studio vocal leveling compressor.
    compressor: SoftKneeCompressor,
    /// Embedded WhiteSharp live audio processor for external input buffers.
    pub whitesharp: WhiteSharpDsp,
    control_counter: usize,
    meter_level: f32,
    /// When set, this engine publishes its block peak to the editor meter.
    reports_meter: Arc<AtomicBool>,
    /// Note being previewed (press-to-hear in the editor), if any.
    audition_note_index: Option<usize>,
    /// Read position in the source audio (input side of the shifter).
    audition_cursor_sec: f64,
    /// While held the note loops; once released it plays to its end.
    audition_held: bool,
    /// After the last input sample: frames left to flush the shifter's
    /// latency so the note's ending is heard instead of cut.
    audition_tail: Option<usize>,
    /// Immediate stop: output fade frames left.
    audition_fade: Option<usize>,
    /// Index of the note that contained the playhead last sample; checked
    /// first so the per-sample lookup is O(1) inside a note.
    active_hint: usize,
    /// `render_stereo_i16`'s float buffer: [`I16_RENDER_CHUNK_FRAMES`] stereo
    /// frames, allocated once here and never grown.
    scratch: Vec<f32>,
    /// The sung pitch of the loaded audio (see [`PitchTrack`]).
    pitch_track: PitchTrack,
    /// The instrumental mixed under the vocal during playback (not in
    /// note previews or exports).
    backing: Option<Backing>,
}

impl CantoEngine {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8_000.0);
        let longest_period = sr / 50.0;
        let mut shifter = Shifter::new(sr, longest_period);
        shifter.set_longest_period(sr / 65.0, 0);

        let mut compressor = SoftKneeCompressor::new(sr);
        compressor.set_curve(-16.0, 2.5, 6.0, 1.5);
        compressor.set_timing(0.008, 0.09);

        let notes = Arc::new(Vec::new());
        let note_feed = Arc::new(NoteFeed {
            generation: AtomicU64::new(0),
            current: Mutex::new(Arc::clone(&notes)),
            retired: Mutex::new(Vec::with_capacity(32)),
            bypass: AtomicBool::new(false),
            editor_meter: Arc::new(AtomicU32::new(0)),
        });

        Self {
            sample_rate: sr,
            bpm: DEFAULT_BPM,
            key: 9, // A
            scale: Scale::Minor,
            playing: false,
            loop_enabled: true,
            bypass: false,
            follow_shared_bypass: false,
            playhead_sec: 0.0,
            correct_amount: 0.0,
            audio_samples: Arc::new(Vec::new()),
            duration_sec: 0.0,
            waveform_peaks: Vec::new(),
            notes,
            note_feed,
            seen_note_gen: 0,
            shifter,
            stretcher: Stretcher::new(longest_period),
            splice_src: None,
            splice_path: None,
            splice_fade: None,
            tail_stretcher: Stretcher::new(longest_period),
            compressor,
            whitesharp: WhiteSharpDsp::new(sr),
            control_counter: 0,
            meter_level: 0.0,
            reports_meter: Arc::new(AtomicBool::new(false)),
            audition_note_index: None,
            audition_cursor_sec: 0.0,
            audition_held: false,
            audition_tail: None,
            audition_fade: None,
            active_hint: 0,
            scratch: vec![0.0; I16_RENDER_CHUNK_FRAMES * 2],
            pitch_track: PitchTrack::default(),
            backing: None,
        }
    }

    /// Plays `backing` under the vocal from now on (None: the vocal alone).
    /// Returns the previous one, for the caller to drop outside any lock
    /// shared with an audio thread.
    pub fn set_backing(&mut self, backing: Option<Backing>) -> Option<Backing> {
        std::mem::replace(&mut self.backing, backing)
    }

    pub fn backing(&self) -> Option<&Backing> {
        self.backing.as_ref()
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn meter_level(&self) -> f32 {
        self.meter_level
    }

    /// Publishes playback bypass to every fork of this take. Forks load it
    /// at the start of their next block; no lock on the audio thread.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
        self.note_feed.bypass.store(bypass, Ordering::Release);
    }

    pub(crate) fn editor_meter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.note_feed.editor_meter)
    }

    pub(crate) fn reports_meter_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.reports_meter)
    }

    pub(crate) fn use_meter_flag(&mut self, flag: Arc<AtomicBool>) {
        self.reports_meter = flag;
    }

    pub fn has_audio(&self) -> bool {
        !self.audio_samples.is_empty()
    }

    /// Loads an audio file from `path`, decodes + resamples it to `self.sample_rate`,
    /// and immediately runs full vocal pitch/note/key/BPM analysis.
    pub fn load_audio_file(&mut self, path: &Path) -> Result<CantoAnalysisResult, String> {
        let decoded = decode_audio_file_to_mono(path, self.sample_rate as u32)?;
        self.install_decoded(decoded)
    }

    /// Loads audio from raw file bytes in memory, decodes + resamples it to `self.sample_rate`,
    /// and immediately runs full vocal pitch/note/key/BPM analysis.
    pub fn load_audio_bytes(
        &mut self,
        bytes: &[u8],
        ext_hint: Option<&str>,
    ) -> Result<CantoAnalysisResult, String> {
        let decoded = decode_audio_bytes_to_mono(bytes, ext_hint, self.sample_rate as u32)?;
        self.install_decoded(decoded)
    }

    /// Swaps in already-decoded audio (decoded at `self.sample_rate`) and runs
    /// analysis. Split from decoding so callers sharing the engine with an
    /// audio thread can decode without holding the engine lock.
    pub fn install_decoded(
        &mut self,
        decoded: DecodedAudio,
    ) -> Result<CantoAnalysisResult, String> {
        self.swap_audio(decoded);
        self.analyze_loaded_audio()
    }

    /// Swaps in decoded audio and adopts an earlier analysis of the same
    /// file (the app's analysis cache) instead of re-running it. Analysis is
    /// the slow part of opening a file; decoding is not.
    ///
    /// Rejects an analysis whose duration doesn't match the audio, so a
    /// stale cache entry falls back to a fresh analysis.
    pub fn install_decoded_with_analysis(
        &mut self,
        decoded: DecodedAudio,
        analysis: CantoAnalysisResult,
    ) -> Result<CantoAnalysisResult, String> {
        let analysis = Analysis::from_cached(analysis, decoded.duration_sec)
            .ok_or_else(|| "Cached analysis does not match the audio".to_string())?;
        Ok(self.install_analyzed(decoded, analysis))
    }

    fn swap_audio(&mut self, decoded: DecodedAudio) {
        self.playing = false;
        self.playhead_sec = 0.0;
        self.correct_amount = 0.0;
        self.audio_samples = Arc::new(decoded.samples);
        self.duration_sec = decoded.duration_sec;
        self.waveform_peaks = decoded.waveform_peaks;
        self.shifter.reset();
        self.compressor.reset();
        self.clear_audition();
        self.active_hint = 0;
    }

    /// Loads raw mono `f32` samples directly into the engine and runs analysis.
    pub fn load_mono_samples(
        &mut self,
        samples: Vec<f32>,
        sample_rate: u32,
    ) -> Result<CantoAnalysisResult, String> {
        if samples.is_empty() || sample_rate == 0 {
            return Err("Empty audio sample buffer".to_string());
        }
        let target_sr = self.sample_rate as u32;
        let mono = if sample_rate == target_sr {
            samples
        } else {
            let mono = crate::audio_io::resample_mono_cubic(&samples, sample_rate, target_sr);
            if mono.is_empty() {
                return Err(
                    "Audio is too long to load or its sample rate is unsupported".to_string(),
                );
            }
            mono
        };
        self.playing = false;
        self.playhead_sec = 0.0;
        self.correct_amount = 0.0;
        self.duration_sec = mono.len() as f64 / self.sample_rate as f64;
        self.waveform_peaks = crate::audio_io::compute_waveform_peaks(&mono, 240);
        self.audio_samples = Arc::new(mono);
        self.shifter.reset();
        self.compressor.reset();
        self.analyze_loaded_audio()
    }

    /// Analyzes `self.audio_samples` to extract vocal notes, real pitch contours,
    /// amplitude envelopes, musical key/scale, and tempo (BPM).
    ///
    /// The app caches this result per file: bump `kAnalysisCacheVersion` in
    /// `lib/cache.dart` whenever the output for the same audio changes.
    pub fn analyze_loaded_audio(&mut self) -> Result<CantoAnalysisResult, String> {
        if self.audio_samples.is_empty() {
            return Err("No audio loaded to analyze".to_string());
        }
        let analysis = analyze_samples(
            &self.audio_samples,
            self.sample_rate,
            &AnalysisProgress::default(),
        )
        .ok_or_else(|| "Analysis cancelled".to_string())?;
        Ok(self.adopt(analysis))
    }

    /// Takes over an analysis made off the engine lock (see `analyze_samples`).
    pub fn adopt(&mut self, analysis: Analysis) -> CantoAnalysisResult {
        self.key = analysis.key;
        self.scale = analysis.scale;
        self.bpm = analysis.bpm;
        self.correct_amount = 0.0;
        self.notes = Arc::new(analysis.notes);
        self.pitch_track = analysis.pitch;
        self.publish_notes();
        self.analysis_result()
    }

    /// Swaps in decoded audio together with its analysis.
    pub fn install_analyzed(
        &mut self,
        decoded: DecodedAudio,
        analysis: Analysis,
    ) -> CantoAnalysisResult {
        self.swap_audio(decoded);
        self.adopt(analysis)
    }

    fn analysis_result(&self) -> CantoAnalysisResult {
        CantoAnalysisResult {
            ok: true,
            error: None,
            sample_rate: self.sample_rate as u32,
            duration_sec: self.duration_sec,
            bpm: self.bpm,
            key: KEY_NAMES[(self.key as usize) % 12].to_string(),
            mode: self.scale.name().to_string(),
            notes: self.notes.as_ref().clone(),
            waveform_peaks: self.waveform_peaks.clone(),
            pitch: self.pitch_track.clone(),
        }
    }

    pub fn set_key_and_scale(&mut self, key: u8, scale: Scale) {
        self.key = key % 12;
        self.scale = scale;
        {
            let key = self.key;
            for note in Arc::make_mut(&mut self.notes) {
                if !note.unpitched {
                    note.lyric = solfege_for_midi(note.pitch, key);
                }
            }
        }
        self.reapply_correct();
    }

    pub fn set_correct_amount(&mut self, amount: f32) {
        self.correct_amount = clamp(amount, 0.0, 1.0);
        self.reapply_correct();
    }

    pub fn reapply_correct(&mut self) {
        let allowed = scale_mask(self.key, self.scale) & ALL_NOTES;
        let amount = self.correct_amount;
        for note in Arc::make_mut(&mut self.notes) {
            if note.unpitched || note.pinned {
                continue;
            }
            if let Some(target) = nearest_target(note.detected, allowed, None) {
                note.pitch = note.detected + (target as f32 - note.detected) * amount;
            }
        }
        self.publish_notes();
    }

    pub fn update_note(
        &mut self,
        index: usize,
        pitch: f32,
        start: f32,
        dur: f32,
        modulation: f32,
        formant: f32,
        gain_db: f32,
        pinned: bool,
        muted: bool,
    ) {
        let key = self.key;
        let notes = Arc::make_mut(&mut self.notes);
        if let Some(note) = notes.get_mut(index) {
            // The lyric only depends on the rounded pitch; skip the String
            // rebuild while a drag keeps the note on the same semitone.
            let relabel = note.pitch.round() != pitch.round();
            note.pitch = pitch;
            note.start = start;
            note.dur = dur.max(0.05);
            note.modulation = clamp(modulation, 0.0, 1.45);
            note.formant = clamp(formant, -12.0, 12.0);
            note.gain_db = clamp(gain_db, -18.0, 6.0);
            note.pinned = pinned;
            note.muted = muted;
            if relabel && !note.unpitched {
                note.lyric = solfege_for_midi(note.pitch, key);
            }
        }
        self.publish_notes();
    }

    /// Replaces one note and publishes it to playback forks.
    pub fn replace_note(&mut self, index: usize, note: CantoNote) {
        {
            let notes = Arc::make_mut(&mut self.notes);
            if index >= notes.len() {
                return;
            }
            notes[index] = note;
        }
        self.publish_notes();
    }

    /// Sets a note's ลูกคอ (ornament) and accent.
    pub fn set_expression(
        &mut self,
        index: usize,
        orn_style: u8,
        orn_depth: f32,
        orn_speed: f32,
        accent: f32,
    ) {
        if let Some(note) = Arc::make_mut(&mut self.notes).get_mut(index) {
            note.orn_style = if orn_style <= ORN_FALL {
                orn_style
            } else {
                ORN_NONE
            };
            note.orn_depth = clamp(orn_depth, 0.0, 4.0);
            note.orn_speed = clamp(orn_speed, 0.0, 1.0);
            note.accent = clamp(accent, 0.0, 1.0);
        }
        self.publish_notes();
    }

    /// Replaces the note list after structural edits (cut, join) or undo.
    /// Notes must stay in time order; the UI keeps them sorted.
    pub fn set_notes(&mut self, notes: Vec<CantoNote>) {
        self.notes = Arc::new(notes);
        self.active_hint = 0;
        if self
            .audition_note_index
            .is_some_and(|i| i >= self.notes.len())
        {
            self.clear_audition();
        }
        self.publish_notes();
    }

    fn publish_notes(&mut self) {
        {
            let mut slot = self
                .note_feed
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *slot = Arc::clone(&self.notes);
        }
        let published = self.note_feed.generation.fetch_add(1, Ordering::Release) + 1;
        self.seen_note_gen = published;
        if let Ok(mut retired) = self.note_feed.retired.lock() {
            retired.clear();
        }
    }

    /// Audio thread: adopt notes published since this engine last rendered.
    /// A list it can no longer hold is parked for the publisher to free.
    fn pull_notes(&mut self) {
        let published = self.note_feed.generation.load(Ordering::Acquire);
        if published == self.seen_note_gen {
            return;
        }
        let next = match self.note_feed.current.try_lock() {
            Ok(guard) => Arc::clone(&guard),
            Err(TryLockError::Poisoned(p)) => Arc::clone(&p.into_inner()),
            Err(TryLockError::WouldBlock) => return,
        };
        if Arc::ptr_eq(&self.notes, &next) {
            self.seen_note_gen = published;
            return;
        }
        let old = std::mem::replace(&mut self.notes, Arc::clone(&next));
        if let Err(old) = self.park_notes(old) {
            // The feed still holds `next`, so dropping it does not free.
            let abandoned = std::mem::replace(&mut self.notes, old);
            drop(abandoned);
            return;
        }
        self.seen_note_gen = published;
    }

    /// Parks `old` without growing the retired buffer. `Err` returns it.
    fn park_notes(&self, old: Arc<Vec<CantoNote>>) -> Result<(), Arc<Vec<CantoNote>>> {
        let mut retired = match self.note_feed.retired.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(old),
        };
        if retired.capacity() == 0 || retired.len() >= retired.capacity() {
            return Err(old);
        }
        retired.push(old);
        Ok(())
    }

    /// A second playback of this take: same samples and notes, its own
    /// playhead and shifter. Called off the audio thread (the clone copies
    /// the shifter's delay lines).
    pub fn fork_playback(&self) -> Self {
        let mut fork = self.clone();
        fork.playing = false;
        fork.playhead_sec = 0.0;
        fork.loop_enabled = false;
        fork.meter_level = 0.0;
        fork.control_counter = 0;
        fork.active_hint = 0;
        fork.shifter.reset();
        fork.compressor.reset();
        fork.stretcher.reset();
        fork.tail_stretcher.reset();
        fork.splice_src = None;
        fork.splice_path = None;
        fork.splice_fade = None;
        fork.clear_audition();
        // The clone can race a publish. The next block pulls the current list.
        fork.seen_note_gen = self.seen_note_gen.wrapping_sub(1);
        fork.follow_shared_bypass = true;
        fork.reports_meter = Arc::new(AtomicBool::new(true));
        fork
    }

    /// Starts previewing a note from its beginning (press on a note).
    ///
    /// The shifter is reset and pre-rolled through its latency here, on the
    /// caller's thread, so the first rendered sample is already the note's
    /// onset: the preview sounds the moment the finger lands.
    pub fn trigger_note_audition(&mut self, note_index: usize) {
        if note_index >= self.notes.len() || self.audio_samples.is_empty() {
            return;
        }
        self.audition_note_index = Some(note_index);
        self.audition_cursor_sec = self.notes[note_index].start as f64;
        self.audition_held = true;
        self.audition_tail = None;
        self.audition_fade = None;
        self.shifter.reset();
        self.compressor.reset();
        self.control_counter = 0;
        for _ in 0..self.shifter.latency() {
            if self.audition_step().is_none() {
                break;
            }
        }
    }

    /// Finger lifted: finish the current pass of the note, then stop.
    pub fn release_audition(&mut self) {
        self.audition_held = false;
    }

    /// Stops the preview with a short fade (no click).
    pub fn stop_audition(&mut self) {
        if self.audition_note_index.is_some() && self.audition_fade.is_none() {
            self.audition_fade = Some(self.audition_fade_frames());
        }
    }

    /// Drops the preview immediately (transport start, new audio).
    fn clear_audition(&mut self) {
        self.audition_note_index = None;
        self.audition_tail = None;
        self.audition_fade = None;
    }

    /// Renders the whole clip with all edits applied, mono, for export.
    ///
    /// Works on a clone so the live engine (and its audio thread) is never
    /// touched. The shifter's latency is rendered past the end and trimmed
    /// from the start so the export lines up with the source sample-exactly.
    pub fn render_offline(&self) -> Vec<f32> {
        if self.audio_samples.is_empty() || self.duration_sec <= 0.0 {
            return Vec::new();
        }
        let mut e = self.clone();
        e.clear_audition();
        // The export is the tuned vocal; the backing track is only a guide.
        e.backing = None;
        e.bypass = false;
        // The clone shares the live bypass word. Follow the local value, and
        // don't publish this forced-off state back to the forks.
        e.follow_shared_bypass = false;
        e.loop_enabled = false;
        e.playing = true;
        e.playhead_sec = 0.0;
        e.shifter.reset();
        e.compressor.reset();
        e.control_counter = 0;
        e.active_hint = 0;
        e.stretcher.reset();
        e.tail_stretcher.reset();
        e.splice_src = None;
        e.splice_path = None;
        e.splice_fade = None;
        let latency = e.shifter.latency() as usize;
        let total = (self.duration_sec * self.sample_rate as f64).round() as usize;
        // Past the source end `sample_interpolated` returns silence, which
        // flushes the shifter's tail.
        e.duration_sec = self.duration_sec + latency as f64 / self.sample_rate as f64;

        let mut out = Vec::with_capacity(total + latency);
        let mut block = vec![0.0f32; 1024 * 2];
        while out.len() < total + latency && e.playing {
            e.render_stereo_f32(&mut block);
            out.extend(block.chunks_exact(2).map(|f| f[0]));
        }
        out.drain(..latency.min(out.len()));
        out.truncate(total);
        out
    }

    pub fn is_auditioning(&self) -> bool {
        self.audition_note_index.is_some()
    }

    /// Starts or stops the transport. Starting playback drops any preview.
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
        if playing {
            self.clear_audition();
        }
    }

    fn audition_fade_frames(&self) -> usize {
        ((self.sample_rate * AUDITION_FADE_SEC) as usize).max(1)
    }

    /// One preview sample, or `None` once the preview has finished.
    fn audition_step(&mut self) -> Option<(f32, f32)> {
        let idx = self.audition_note_index?;
        if idx >= self.notes.len() {
            self.clear_audition();
            return None;
        }
        // The note's edited span: a stretched note previews at its new length.
        let (start, end) = {
            let n = &self.notes[idx];
            let start = n.start as f64;
            (start, start + n.dur.max(0.05) as f64)
        };

        let (dry, local_t) = match self.audition_tail {
            Some(0) => {
                self.clear_audition();
                return None;
            }
            Some(left) => {
                self.audition_tail = Some(left - 1);
                (0.0, 1.0)
            }
            None => {
                // A drag may move the note's start past the cursor.
                let cursor = self.audition_cursor_sec.clamp(start, end);
                // Short ramps at both ends of the note keep loop restarts
                // and the final stop free of clicks.
                let fade = AUDITION_FADE_SEC as f64;
                let env = ((cursor - start) / fade)
                    .min((end - cursor) / fade)
                    .clamp(0.0, 1.0) as f32;
                let local_t = ((cursor - start) / (end - start).max(0.01)).clamp(0.0, 1.0) as f32;
                let dry = self.note_dry(idx, cursor) * env;
                self.audition_cursor_sec = cursor + 1.0 / self.sample_rate as f64;
                if self.audition_cursor_sec >= end {
                    if self.audition_held {
                        self.audition_cursor_sec = start;
                    } else {
                        self.audition_tail = Some(self.shifter.latency() as usize);
                    }
                }
                (dry, local_t)
            }
        };

        let bypass = self.bypass;
        let (mut l, mut r) = self.shift_sample(dry, |e| {
            if bypass {
                e.neutral_control()
            } else {
                e.note_control(idx, local_t)
            }
        });
        if let Some(left) = self.audition_fade {
            if left == 0 {
                self.clear_audition();
                return None;
            }
            let gain = left as f32 / self.audition_fade_frames() as f32;
            l *= gain;
            r *= gain;
            self.audition_fade = Some(left - 1);
        }
        Some((l, r))
    }

    pub fn set_direct_voice(&mut self, _hz: f32, _cutoff: f32, gain: f32) {
        if gain <= 0.001 {
            self.stop_audition();
        }
    }

    pub fn seek(&mut self, time_sec: f64) {
        let dur = self.duration_sec.max(0.0);
        self.playhead_sec = time_sec.clamp(0.0, dur);
    }

    /// Index of the edited note covering `time_sec` (last one wins, matching
    /// draw order). Only used on the non-bypass path, where the presented note
    /// is the note itself, so nothing is cloned.
    fn note_index_at(&mut self, time_sec: f32) -> Option<usize> {
        if let Some(note) = self.notes.get(self.active_hint) {
            if time_sec >= note.start && time_sec <= note.start + note.dur {
                return Some(self.active_hint);
            }
        }
        let found = self
            .notes
            .iter()
            .rposition(|n| time_sec >= n.start && time_sec <= n.start + n.dur);
        if let Some(index) = found {
            self.active_hint = index;
        }
        found
    }

    /// Shifter control for `note_index` at normalized position `local_t`.
    /// Called once per control tick (every 64 samples), not per sample.
    fn note_control(&self, note_index: usize, local_t: f32) -> Control {
        let note = &self.notes[note_index];
        let gain = db_to_linear(note.gain_db - note.detected_gain).clamp(0.05, 4.0)
            * accent_gain(note, local_t);
        let centre = self.shifter.now();
        if note.unpitched {
            return Control {
                centre,
                period: None,
                ratio: 1.0,
                formant: 1.0,
                gain,
            };
        }
        // The source period is the pitch actually sung at this point of the
        // source, not the note's smoothed contour: that one misses fast
        // vibrato and scoops, and grains cut at the wrong period crackle
        // and wobble. The edit keeps (or, with Drift, scales) the same real
        // movement. A track value far from the note (an octave slip) or a
        // missing one falls back to the contour.
        let contour = detected_pitch_at(note, local_t);
        let src_time = note.detected_start + local_t * note.detected_dur;
        let sung = self
            .pitch_track
            .at(src_time)
            .filter(|m| (m - contour).abs() < 2.0)
            .unwrap_or(contour);
        let target = pitch_at(note, local_t) + (sung - contour) * note.modulation;
        let src_hz = midi_to_hz(sung).clamp(55.0, 1400.0);
        let dst_hz = midi_to_hz(target).clamp(55.0, 1400.0);
        Control {
            centre,
            period: Some(self.sample_rate / src_hz),
            ratio: (dst_hz / src_hz).clamp(0.25, 4.0),
            formant: 2.0f32
                .powf(note.formant / 12.0)
                .clamp(1.0 / MAX_FORMANT, MAX_FORMANT),
            gain,
        }
    }

    fn neutral_control(&self) -> Control {
        Control {
            centre: self.shifter.now(),
            period: None,
            ratio: 1.0,
            formant: 1.0,
            gain: 1.0,
        }
    }

    /// Runs one sample through the shifter and compressor, pushing `control`
    /// on control ticks. Returns the clipped output sample pair.
    #[inline]
    fn shift_sample(&mut self, dry: f32, control: impl FnOnce(&Self) -> Control) -> (f32, f32) {
        if self.control_counter == 0 {
            let c = control(self);
            self.shifter.push_control(c);
        }
        self.control_counter = (self.control_counter + 1) & 63;
        let (wet, _) = self.shifter.process(dry, dry);
        let (l, r) = self.compressor.process_stereo_linked(wet[0], wet[1]);
        (clamp(l, -0.98, 0.98), clamp(r, -0.98, 0.98))
    }

    fn is_vacated_original_region(&self, time_sec: f32) -> bool {
        if self.bypass {
            return false;
        }
        for note in self.notes.iter() {
            if time_sec >= note.detected_start
                && time_sec <= note.detected_start + note.detected_dur
            {
                let moved = (note.start - note.detected_start).abs() > 0.025
                    || (note.dur - note.detected_dur).abs() > 0.025;
                if moved {
                    return true;
                }
            }
        }
        false
    }

    /// Between notes: the audio that followed the note just played, for as
    /// long as the gap after it was when sung. Resized or moved legato notes
    /// thus keep the breath or glide between them instead of a silent hole.
    /// For unedited notes this is the source at the playhead.
    fn gap_source(&self, time_sec: f32) -> Option<f64> {
        let prev = self.notes.get(self.active_hint)?;
        let end = prev.start + prev.dur;
        if time_sec < end {
            return None;
        }
        let sung_end = prev.detected_start + prev.detected_dur;
        let room = self
            .notes
            .get(self.active_hint + 1)
            .map_or(self.duration_sec as f32 - sung_end, |n| {
                n.detected_start - sung_end
            });
        let into = time_sec - end;
        (into < room).then_some((sung_end + into) as f64)
    }

    /// Call before reading a playback sample: `src` is where it will be read
    /// from in the source (None for silence) and `path` the stretched note
    /// it goes through, if any. A jump (entering or leaving a moved or
    /// resized note, a mute, a seek) starts a crossfade in which what was
    /// playing carries on and fades out. The stretcher reads near, not at,
    /// its source position, so switching it in or out is a jump too.
    fn begin_splice(&mut self, src: Option<f64>, path: Option<usize>) {
        let jumped = path != self.splice_path
            || match (self.splice_src, src) {
                (Some(a), Some(b)) => (b - a).abs() > SPLICE_TOLERANCE_SEC,
                (None, None) => false,
                _ => true,
            };
        if jumped {
            let tail = match (self.splice_path, self.splice_src) {
                (Some(note), _) => {
                    // The outgoing note keeps its stretcher for the fade.
                    std::mem::swap(&mut self.stretcher, &mut self.tail_stretcher);
                    self.stretcher.reset();
                    Tail::Stretched(note)
                }
                (None, Some(at)) => Tail::Source(at + 1.0 / self.sample_rate as f64),
                (None, None) => Tail::Silence,
            };
            let frames = ((self.sample_rate * SPLICE_FADE_SEC) as u32).max(1);
            self.splice_fade = Some((tail, frames, frames));
        }
        self.splice_src = src;
        self.splice_path = path;
    }

    /// Mixes the fading-out tail of a splice under the new sample `dry`.
    fn end_splice(&mut self, dry: f32) -> f32 {
        let Some((tail, left, len)) = self.splice_fade else {
            return dry;
        };
        let gain = 0.5 - 0.5 * (std::f32::consts::PI * (len - left) as f32 / len as f32).cos();
        let (old, next) = match tail {
            Tail::Silence => (0.0, tail),
            Tail::Source(at) => (
                self.sample_interpolated(at),
                Tail::Source(at + 1.0 / self.sample_rate as f64),
            ),
            Tail::Stretched(note) => (self.stretched_sample(note, self.playhead_sec, true), tail),
        };
        self.splice_fade = (left > 1).then_some((next, left - 1, len));
        dry * gain + old * (1.0 - gain)
    }

    fn is_stretched(&self, index: usize) -> bool {
        let n = &self.notes[index];
        (n.detected_dur as f64 / n.dur.max(0.01) as f64 - 1.0).abs() >= STRETCH_EPSILON
    }

    /// Source audio for note `index` at output time `time_sec`: the note's
    /// sung region spread over its edited span. A note whose length changed
    /// goes through the stretcher so its pitch stays put; any other note is
    /// read directly.
    fn note_dry(&mut self, index: usize, time_sec: f64) -> f32 {
        if self.is_stretched(index) {
            return self.stretched_sample(index, time_sec, false);
        }
        let n = &self.notes[index];
        let local = ((time_sec - n.start as f64) / n.dur.max(0.01) as f64).clamp(0.0, 1.0);
        self.sample_interpolated(n.detected_start as f64 + local * n.detected_dur as f64)
    }

    /// Note `index` stretched to its edited span, at output time `time_sec`,
    /// through the main stretcher or the one finishing a splice (`tail`).
    fn stretched_sample(&mut self, index: usize, time_sec: f64, tail: bool) -> f32 {
        let Some(n) = self.notes.get(index) else {
            return 0.0;
        };
        let (start, dur) = (n.start as f64, n.dur.max(0.01) as f64);
        let (det_start, det_dur) = (n.detected_start as f64, n.detected_dur as f64);
        let sr = self.sample_rate as f64;
        let unvoiced = n.unpitched.then(|| self.shifter.unvoiced_period() as f64);
        let at = (time_sec * sr).round() as i64;
        let stretcher = if tail {
            &mut self.tail_stretcher
        } else {
            &mut self.stretcher
        };
        stretcher.sample(index, at, &self.audio_samples, |pos| {
            let local = ((pos / sr - start) / dur).clamp(0.0, 1.0);
            let period = unvoiced.unwrap_or_else(|| {
                sr / midi_to_hz(detected_pitch_at(n, local as f32)).clamp(55.0, 1400.0) as f64
            });
            ((det_start + local * det_dur) * sr, period)
        })
    }

    #[inline]
    fn sample_interpolated(&self, time_sec: f64) -> f32 {
        if self.audio_samples.is_empty() || time_sec < 0.0 {
            return 0.0;
        }
        let pos = time_sec * self.sample_rate as f64;
        let idx = pos.floor() as usize;
        if idx >= self.audio_samples.len() {
            return 0.0;
        }
        let next = (idx + 1).min(self.audio_samples.len() - 1);
        let frac = (pos - idx as f64) as f32;
        self.audio_samples[idx] + (self.audio_samples[next] - self.audio_samples[idx]) * frac
    }

    /// Renders stereo interleaved `f32` frames (`frames = out_interleaved.len() / 2`)
    /// from the loaded audio file through the TD-PSOLA `Shifter` + `SoftKneeCompressor`.
    pub fn render_stereo_f32(&mut self, out_interleaved: &mut [f32]) -> f32 {
        self.pull_notes();
        if self.follow_shared_bypass {
            self.bypass = self.note_feed.bypass.load(Ordering::Acquire);
        }
        let frames = out_interleaved.len() / 2;
        if frames == 0 {
            return self.meter_level;
        }

        if self.audio_samples.is_empty() || self.duration_sec <= 0.0 {
            out_interleaved.fill(0.0);
            self.meter_level = 0.0;
            return 0.0;
        }

        let dt = 1.0 / self.sample_rate as f64;
        let total_dur = self.duration_sec;
        let mut block_peak = 0.0f32;
        // The vocal heard now went into the shifter this long ago (bypass
        // plays the source directly); the backing plays that same moment.
        let heard_lag = if self.bypass {
            0.0
        } else {
            self.shifter.latency() as f64 * dt
        };

        // Allocation-free per-sample loop: notes are read by index and the
        // pitch/formant math runs only on control ticks inside `shift_sample`.
        for frame in out_interleaved.chunks_exact_mut(2) {
            let (out_l, out_r) = if !self.playing {
                match self.audition_step() {
                    Some(sample) => sample,
                    None => {
                        frame[0] = 0.0;
                        frame[1] = 0.0;
                        continue;
                    }
                }
            } else {
                self.playhead_sec += dt;
                if self.playhead_sec >= total_dur {
                    if self.loop_enabled {
                        self.playhead_sec %= total_dur;
                    } else {
                        self.playhead_sec = total_dur;
                        self.playing = false;
                        frame[0] = 0.0;
                        frame[1] = 0.0;
                        continue;
                    }
                }
                let t = self.playhead_sec as f32;

                if self.bypass {
                    // Keep the shifter's timeline running so leaving bypass
                    // doesn't replay stale grains.
                    let raw = self.sample_interpolated(self.playhead_sec);
                    let _ = self.shift_sample(raw, |e| e.neutral_control());
                    let s = clamp(raw, -0.98, 0.98);
                    (s, s)
                } else if let Some(idx) = self.note_index_at(t) {
                    let n = &self.notes[idx];
                    let local_t = ((t - n.start) / n.dur.max(0.01)).clamp(0.0, 1.0);
                    // Muted notes keep the timeline running but feed silence.
                    let muted = n.muted;
                    let (src, path) = if muted {
                        (None, None)
                    } else {
                        let src = (n.detected_start + local_t * n.detected_dur) as f64;
                        (Some(src), self.is_stretched(idx).then_some(idx))
                    };
                    self.begin_splice(src, path);
                    let dry = if muted {
                        0.0
                    } else {
                        self.note_dry(idx, self.playhead_sec)
                    };
                    let dry = self.end_splice(dry);
                    self.shift_sample(dry, |e| e.note_control(idx, local_t))
                } else {
                    let src = match self.gap_source(t) {
                        Some(at) => Some(at),
                        None if self.is_vacated_original_region(t) => None,
                        None => Some(self.playhead_sec),
                    };
                    self.begin_splice(src, None);
                    let dry = src.map_or(0.0, |at| self.sample_interpolated(at));
                    let dry = self.end_splice(dry);
                    self.shift_sample(dry, |e| e.neutral_control())
                }
            };
            frame[0] = out_l;
            frame[1] = out_r;
            // The meter shows the vocal alone.
            block_peak = block_peak.max(out_l.abs().max(out_r.abs()));
            if self.playing {
                if let Some(backing) = &self.backing {
                    let (bl, br) = backing.at(self.playhead_sec - heard_lag);
                    frame[0] = (out_l + bl).clamp(-1.0, 1.0);
                    frame[1] = (out_r + br).clamp(-1.0, 1.0);
                }
            }
        }

        self.meter_level += (block_peak - self.meter_level) * 0.45;
        self.meter_level
    }

    /// Renders into the Android front end's `i16` callback buffer.
    ///
    /// Runs on the audio callback, whose size is the device's and can grow
    /// after a route change, so it never sizes a buffer to it: a callback
    /// longer than the preallocated scratch is rendered in chunks of
    /// [`I16_RENDER_CHUNK_FRAMES`]. A callback within one chunk renders exactly
    /// as one `render_stereo_f32` call; a longer one steps the meter once per
    /// chunk.
    pub fn render_stereo_i16(&mut self, out_interleaved: &mut [i16]) -> f32 {
        let mut scratch = std::mem::take(&mut self.scratch);
        // Whole stereo frames per chunk, so no chunk splits a frame.
        let chunk_len = scratch.len() & !1;
        let mut level = self.meter_level;
        if chunk_len > 0 {
            for out_chunk in out_interleaved.chunks_mut(chunk_len) {
                let float_buf = &mut scratch[..out_chunk.len()];
                level = self.render_stereo_f32(float_buf);
                for (dst, &src) in out_chunk.iter_mut().zip(float_buf.iter()) {
                    *dst = (clamp(src, -0.99, 0.99) * i16::MAX as f32) as i16;
                }
            }
        }
        self.scratch = scratch;
        level
    }

    pub fn process_external_stereo(&mut self, input: &[f32], output: &mut [f32]) {
        let frames = (input.len() / 2).min(output.len() / 2);
        for i in 0..frames {
            let (l, r) = self
                .whitesharp
                .process_stereo(input[i * 2], input[i * 2 + 1]);
            output[i * 2] = l;
            output[i * 2 + 1] = r;
        }
    }
}

// ── Fast YIN Pitch Frame Extraction & Studio Note Segmentation ────────────────

/// Pitch analysis hop (~10 ms) and window (~42 ms), in samples.
fn pitch_hop(sample_rate: u32) -> usize {
    ((sample_rate as usize) / 100).max(128)
}

fn pitch_window(sample_rate: u32) -> usize {
    ((sample_rate as usize) * 42 / 1000).max(1024)
}

/// YIN confidence from which a frame counts as voiced.
const PITCH_VOICED: f64 = 0.42;

/// Lowest and highest pitch the analysis looks for.
const PITCH_MIN_HZ: f64 = 65.0;
const PITCH_MAX_HZ: f64 = 950.0;

/// Samples [`detect_pitch_yin_fast`] compares at each lag (from the start
/// of the window).
fn yin_span(sample_rate: u32) -> usize {
    let tau_max = (sample_rate as f64 / PITCH_MIN_HZ).round() as usize;
    pitch_window(sample_rate).saturating_sub(tau_max).min(1024)
}

fn extract_pitch_frames(
    samples: &[f32],
    sample_rate: u32,
    progress: &AnalysisProgress,
) -> Option<Vec<PitchFrame>> {
    let hop_size = pitch_hop(sample_rate);
    let frame_size = pitch_window(sample_rate);
    let min_freq = PITCH_MIN_HZ;
    let max_freq = PITCH_MAX_HZ;
    let voiced_threshold = PITCH_VOICED;

    let mut peak_rms = 0.0f64;
    let mut raw_rms = Vec::with_capacity(samples.len() / hop_size + 1);
    let mut i = 0usize;
    while i < samples.len() {
        let end = (i + frame_size).min(samples.len());
        let r = calculate_rms(&samples[i..end]);
        if r > peak_rms {
            peak_rms = r;
        }
        raw_rms.push((i, end, r));
        i += hop_size;
    }

    let rms_gate = (peak_rms * 0.045).max(0.003);
    let mut frames = Vec::with_capacity(raw_rms.len());
    let total = raw_rms.len().max(1);

    for (n, (start, end, rms)) in raw_rms.into_iter().enumerate() {
        // Report and honour cancel every ~2.5 s of audio, not per frame.
        if n % 256 == 0 {
            if progress.cancel.load(Ordering::Relaxed) {
                return None;
            }
            let permille = (n * 1000 / total) as u32;
            progress.permille.store(permille, Ordering::Relaxed);
        }
        let time_seconds = start as f64 / sample_rate as f64;
        let (frequency_hz, confidence) = if rms >= rms_gate && (end - start) >= frame_size / 2 {
            detect_pitch_yin_fast(
                &samples[start..end],
                sample_rate,
                min_freq,
                max_freq,
                voiced_threshold,
            )
        } else {
            (0.0, 0.0)
        };
        frames.push(PitchFrame {
            time_seconds,
            frequency_hz,
            confidence,
            rms,
        });
    }

    Some(frames)
}

fn detect_pitch_yin_fast(
    frame: &[f32],
    sample_rate: u32,
    min_freq: f64,
    max_freq: f64,
    voiced_threshold: f64,
) -> (f64, f64) {
    let tau_min = (sample_rate as f64 / max_freq).round() as usize;
    let tau_max = (sample_rate as f64 / min_freq).round() as usize;
    if frame.len() <= tau_max + 2 || tau_min >= tau_max {
        return (0.0, 0.0);
    }

    let w = (frame.len() - tau_max).min(1024);
    let step = if w >= 512 { 2 } else { 1 };
    let mut d = vec![0.0f64; tau_max];

    for tau in 1..tau_max {
        let mut sum = 0.0;
        let mut j = 0;
        while j < w {
            let diff = frame[j] as f64 - frame[j + tau] as f64;
            sum += diff * diff;
            j += step;
        }
        d[tau] = sum;
    }

    let mut d_prime = vec![1.0f64; tau_max];
    let mut running_sum = 0.0;
    for tau in 1..tau_max {
        running_sum += d[tau];
        if running_sum > 1e-12 {
            d_prime[tau] = d[tau] * (tau as f64) / running_sum;
        }
    }

    let threshold = 0.16;
    let mut opt_tau = None;
    for tau in tau_min..(tau_max - 1) {
        if d_prime[tau] < threshold
            && d_prime[tau] < d_prime[tau - 1]
            && d_prime[tau] <= d_prime[tau + 1]
        {
            opt_tau = Some(tau);
            break;
        }
    }

    let tau = match opt_tau {
        Some(t) => t,
        None => {
            let mut min_val = f64::MAX;
            let mut min_tau = 0;
            for (t, &val) in d_prime.iter().enumerate().take(tau_max - 1).skip(tau_min) {
                if val < min_val {
                    min_val = val;
                    min_tau = t;
                }
            }
            min_tau
        }
    };

    if tau == 0 || tau + 1 >= tau_max {
        return (0.0, 0.0);
    }

    let alpha = d_prime[tau - 1];
    let beta = d_prime[tau];
    let gamma = d_prime[tau + 1];
    let denom = alpha - 2.0 * beta + gamma;
    let mut tau_interp = tau as f64;
    if denom.abs() > 1e-6 {
        let p = 0.5 * (alpha - gamma) / denom;
        if p.abs() <= 0.5 {
            tau_interp += p;
        }
    }

    let freq = sample_rate as f64 / tau_interp;
    let confidence = (1.0 - beta.min(1.0)).clamp(0.0, 1.0);
    if freq >= min_freq && freq <= max_freq && confidence >= voiced_threshold {
        (freq, confidence)
    } else {
        (0.0, confidence)
    }
}

fn segment_canto_notes(frames: &[PitchFrame], key_pc: u8) -> Vec<CantoNote> {
    if frames.is_empty() {
        return Vec::new();
    }

    let frame_dt = if frames.len() >= 2 {
        (frames[1].time_seconds - frames[0].time_seconds).max(0.005)
    } else {
        0.010
    };

    let peak_rms = frames.iter().map(|f| f.rms).fold(0.0f64, f64::max);
    let voiced_rms_gate = (peak_rms * 0.048).max(0.0035);
    let voiced_conf_gate = 0.42;

    let mut is_voiced: Vec<bool> = frames
        .iter()
        .map(|f| {
            f.confidence >= voiced_conf_gate
                && f.rms >= voiced_rms_gate
                && f.frequency_hz >= 65.0
                && f.frequency_hz <= 950.0
        })
        .collect();

    // Bridge 1-3 frame dropouts (<= 30ms) inside voiced phrases
    for i in 1..is_voiced.len().saturating_sub(3) {
        if !is_voiced[i]
            && is_voiced[i - 1]
            && (is_voiced[i + 1] || is_voiced[i + 2] || is_voiced[i + 3])
        {
            is_voiced[i] = true;
        }
    }

    let mut voiced_rms_sum = 0.0;
    let mut voiced_rms_count = 0usize;
    for (i, f) in frames.iter().enumerate() {
        if is_voiced[i] {
            voiced_rms_sum += f.rms;
            voiced_rms_count += 1;
        }
    }
    let ref_rms = if voiced_rms_count > 0 {
        (voiced_rms_sum / voiced_rms_count as f64).max(1e-4)
    } else {
        (peak_rms * 0.35).max(1e-4)
    };

    let mut notes = Vec::new();
    let mut note_counter = 1usize;

    let mut idx = 0usize;
    while idx < frames.len() {
        if is_voiced[idx] {
            let seg_start = idx;
            while idx < frames.len() && is_voiced[idx] {
                idx += 1;
            }
            let seg_end = idx;
            let seg = &frames[seg_start..seg_end];
            let sub_notes = split_voiced_segment(seg, frame_dt, ref_rms, key_pc, &mut note_counter);
            notes.extend(sub_notes);
        } else {
            idx += 1;
        }
    }

    notes.sort_by(|a, b| a.start.total_cmp(&b.start));

    // Merge micro-fragment notes that are very close in time (< 45ms) and pitch (< 0.42 st)
    let mut merged: Vec<CantoNote> = Vec::with_capacity(notes.len());
    for note in notes {
        if let Some(prev) = merged.last_mut() {
            let gap = note.start - (prev.start + prev.dur);
            let pitch_diff = (note.detected - prev.detected).abs();
            if !prev.unpitched && !note.unpitched && gap < 0.045 && pitch_diff < 0.42 {
                let w1 = prev.dur;
                let w2 = note.dur;
                let total_w = (w1 + w2).max(0.01);
                let new_detected = (prev.detected * w1 + note.detected * w2) / total_w;
                let new_end = note.start + note.dur;
                let new_dur = (new_end - prev.start).max(0.08);
                let shift_cents = (prev.detected - new_detected) * 100.0;
                let shift_cents_2 = (note.detected - new_detected) * 100.0;

                let mut combined_contour = Vec::with_capacity(CONTOUR_POINTS);
                let mut combined_env = Vec::with_capacity(CONTOUR_POINTS);
                let split_ratio = (w1 / total_w).clamp(0.1, 0.9);
                for k in 0..CONTOUR_POINTS {
                    let u = k as f32 / (CONTOUR_POINTS - 1) as f32;
                    if u <= split_ratio {
                        let lt = (u / split_ratio).clamp(0.0, 1.0);
                        combined_contour.push(clamp(
                            sample_curve(&prev.contour, lt) + shift_cents,
                            -55.0,
                            55.0,
                        ));
                        combined_env.push(sample_curve(&prev.envelope, lt));
                    } else {
                        let lt = ((u - split_ratio) / (1.0 - split_ratio)).clamp(0.0, 1.0);
                        combined_contour.push(clamp(
                            sample_curve(&note.contour, lt) + shift_cents_2,
                            -55.0,
                            55.0,
                        ));
                        combined_env.push(sample_curve(&note.envelope, lt));
                    }
                }

                prev.detected = new_detected;
                prev.pitch = new_detected;
                prev.detected_dur = new_dur;
                prev.dur = new_dur;
                prev.lyric = solfege_for_midi(new_detected, key_pc);
                prev.contour = combined_contour;
                prev.envelope = combined_env;
                continue;
            }
        }
        merged.push(note);
    }

    // Ensure clean visual separation (min 12ms gap) between adjacent notes so blocks never overlap
    for i in 0..merged.len().saturating_sub(1) {
        let next_start = merged[i + 1].start;
        if merged[i].start + merged[i].dur > next_start - 0.012 {
            let trimmed = (next_start - 0.012 - merged[i].start).max(0.06);
            merged[i].dur = trimmed;
            merged[i].detected_dur = trimmed;
        }
    }

    for (i, n) in merged.iter_mut().enumerate() {
        n.id = format!("n{}", i + 1);
    }

    merged
}

fn split_voiced_segment(
    seg: &[PitchFrame],
    frame_dt: f64,
    ref_rms: f64,
    key_pc: u8,
    note_counter: &mut usize,
) -> Vec<CantoNote> {
    if seg.is_empty() {
        return Vec::new();
    }

    let mut raw_midi = Vec::with_capacity(seg.len());
    let mut last_valid_midi = 69.0f64;
    for f in seg {
        if f.frequency_hz > 0.0 {
            let (m, c) = hz_to_midi_cents(f.frequency_hz);
            last_valid_midi = m as f64 + c / 100.0;
        }
        raw_midi.push(last_valid_midi);
    }

    // 7-point median filter to reject octave/consonant spikes
    let mut median_midi = vec![0.0f64; raw_midi.len()];
    for i in 0..raw_midi.len() {
        let lo = i.saturating_sub(3);
        let hi = (i + 4).min(raw_midi.len());
        let mut win: Vec<f64> = raw_midi[lo..hi].to_vec();
        win.sort_by(|a, b| a.total_cmp(b));
        median_midi[i] = win[win.len() / 2];
    }

    // 5-point moving average for plateau segmentation
    let mut smooth_midi = vec![0.0f64; median_midi.len()];
    for i in 0..median_midi.len() {
        let lo = i.saturating_sub(2);
        let hi = (i + 3).min(median_midi.len());
        let sum: f64 = median_midi[lo..hi].iter().sum();
        smooth_midi[i] = sum / (hi - lo) as f64;
    }

    // Minimum ~95ms per note plateau
    let min_frames = ((0.095 / frame_dt).round() as usize).max(6);
    let mut notes = Vec::new();
    let mut note_start_idx = 0usize;
    // Fixed semitone plateau center (does not drift with slow slides!)
    let mut plateau_midi = smooth_midi[0];

    for i in 1..seg.len() {
        let diff = (smooth_midi[i] - plateau_midi).abs();

        // Check if pitch stepped to a new plateau (>= 0.65 semitones away from current plateau)
        let lookahead_end = (i + min_frames).min(seg.len());
        let candidate_center = smooth_midi[i];
        let stable_new_pitch = diff >= 0.65
            && (lookahead_end - i) >= min_frames / 2
            && smooth_midi[i..lookahead_end]
                .iter()
                .all(|&m| (m - candidate_center).abs() < 0.48);

        if stable_new_pitch && (i - note_start_idx) >= min_frames {
            if let Some(note) = build_pitched_note(
                &seg[note_start_idx..i],
                &median_midi[note_start_idx..i],
                frame_dt,
                ref_rms,
                key_pc,
                note_counter,
            ) {
                notes.push(note);
            }
            note_start_idx = i;
            plateau_midi =
                smooth_midi[i..lookahead_end].iter().sum::<f64>() / (lookahead_end - i) as f64;
        }
    }

    if seg.len() - note_start_idx >= min_frames {
        if let Some(note) = build_pitched_note(
            &seg[note_start_idx..],
            &median_midi[note_start_idx..],
            frame_dt,
            ref_rms,
            key_pc,
            note_counter,
        ) {
            notes.push(note);
        }
    }

    notes
}

fn build_pitched_note(
    frames: &[PitchFrame],
    midi_curve: &[f64],
    frame_dt: f64,
    ref_rms: f64,
    key_pc: u8,
    note_counter: &mut usize,
) -> Option<CantoNote> {
    if frames.is_empty() || midi_curve.is_empty() {
        return None;
    }

    let start = frames[0].time_seconds as f32;
    let end = (frames[frames.len() - 1].time_seconds + frame_dt) as f32;
    let dur = (end - start).max(0.08);

    // Core frames (middle 65%) for steady center pitch estimation
    let n = midi_curve.len();
    let core_lo = (n * 18) / 100;
    let core_hi = ((n * 82) / 100).max(core_lo + 1).min(n);
    let mut core_sorted: Vec<f64> = midi_curve[core_lo..core_hi].to_vec();
    core_sorted.sort_by(|a, b| a.total_cmp(b));
    let trim = core_sorted.len() / 4;
    let trimmed = &core_sorted[trim..(core_sorted.len() - trim).max(trim + 1)];
    let center_midi = (trimmed.iter().sum::<f64>() / trimmed.len() as f64) as f32;

    let mut sum_rms = 0.0f64;
    let mut max_rms = 0.0f64;
    for f in frames {
        sum_rms += f.rms;
        if f.rms > max_rms {
            max_rms = f.rms;
        }
    }
    let mean_rms = (sum_rms / frames.len() as f64).max(1e-5);
    let gain_db = clamp((20.0 * (mean_rms / ref_rms).log10()) as f32, -18.0, 6.0);

    let mut contour = Vec::with_capacity(CONTOUR_POINTS);
    let mut envelope = Vec::with_capacity(CONTOUR_POINTS);
    let norm_peak = max_rms.max(1e-4);

    for k in 0..CONTOUR_POINTS {
        let u = k as f64 / (CONTOUR_POINTS - 1) as f64;
        let pos = u * (n.saturating_sub(1)) as f64;
        let idx = pos.floor() as usize;
        let next = (idx + 1).min(n - 1);
        let frac = pos - idx as f64;

        let m = midi_curve[idx] + (midi_curve[next] - midi_curve[idx]) * frac;
        // Attenuate extreme consonant onset/release spikes at the very edges
        let edge_taper = if u < 0.12 {
            (u / 0.12) as f32
        } else if u > 0.88 {
            ((1.0 - u) / 0.12) as f32
        } else {
            1.0
        };
        let raw_cents = (m as f32 - center_midi) * 100.0;
        let cents = clamp(raw_cents * (0.35 + 0.65 * edge_taper), -55.0, 55.0);
        contour.push(cents);

        let r = frames[idx].rms + (frames[next].rms - frames[idx].rms) * frac;
        let env = ((r / norm_peak) as f32).clamp(0.28, 1.0);
        envelope.push(env);
    }

    // Two-pass 5-tap Gaussian smoothing on `contour` and `envelope` so visual curves are studio-smooth
    let smooth_contour = smooth_curve_5tap(&smooth_curve_5tap(&contour));
    let smooth_envelope = smooth_curve_5tap(&smooth_curve_5tap(&envelope));

    let scoop = (smooth_contour[0] + smooth_contour[1]) * 0.5;
    let drift = smooth_contour[CONTOUR_POINTS - 3] - smooth_contour[5];
    let mut vib_acc = 0.0f32;
    for k in 4..(CONTOUR_POINTS - 2) {
        let local_mean = (smooth_contour[k - 2] + smooth_contour[k] + smooth_contour[k + 2]) / 3.0;
        let hp = smooth_contour[k] - local_mean;
        vib_acc += hp * hp;
    }
    let vib = (vib_acc / (CONTOUR_POINTS - 6) as f32).sqrt() * 1.414;

    let id = format!("n{}", *note_counter);
    *note_counter += 1;

    Some(CantoNote {
        id,
        lyric: solfege_for_midi(center_midi, key_pc),
        unpitched: false,
        detected: center_midi,
        pitch: center_midi,
        detected_start: start,
        start,
        detected_dur: dur,
        dur,
        modulation: 1.0,
        formant: 0.0,
        gain_db,
        detected_gain: gain_db,
        pinned: false,
        muted: false,
        orn_style: ORN_NONE,
        orn_depth: 1.0,
        orn_speed: 0.5,
        accent: 0.0,
        scoop,
        drift,
        vib,
        vib_cycles: (dur * 5.2).max(1.5),
        vib_phase: 0.0,
        contour: smooth_contour,
        envelope: smooth_envelope,
    })
}

fn smooth_curve_5tap(input: &[f32]) -> Vec<f32> {
    if input.len() < 5 {
        return input.to_vec();
    }
    let len = input.len();
    let mut out = vec![0.0f32; len];
    for i in 0..len {
        let p0 = input[i.saturating_sub(2)];
        let p1 = input[i.saturating_sub(1)];
        let p2 = input[i];
        let p3 = input[(i + 1).min(len - 1)];
        let p4 = input[(i + 2).min(len - 1)];
        out[i] = 0.0625 * p0 + 0.25 * p1 + 0.375 * p2 + 0.25 * p3 + 0.0625 * p4;
    }
    out
}

/// Estimates musical Key (`0..11`) and Scale from voiced `PitchFrame`s: the
/// tonic and major / minor from Krumhansl-Kessler profiles, then the scale
/// the take actually uses (modes, harmonic / melodic minor, pentatonics,
/// blues) from [`refine_scale`].
fn estimate_key_and_mode(frames: &[PitchFrame]) -> (u8, Scale) {
    let mut chroma = [0.0f32; 12];
    let mut total_weight = 0.0f32;

    for f in frames {
        if f.frequency_hz >= 65.0 && f.confidence >= 0.42 {
            let (midi, cents) = hz_to_midi_cents(f.frequency_hz);
            let tuning_weight = (1.0 - (cents.abs() as f32 / 50.0).min(1.0)) * 0.5 + 0.5;
            let w = (f.rms as f32) * (f.confidence as f32) * tuning_weight;
            let pc = (midi as usize) % 12;
            chroma[pc] += w;
            total_weight += w;
        }
    }

    if total_weight <= 1e-6 {
        return (9, Scale::Minor);
    }

    let mut best_key = 9u8;
    let mut best_scale = Scale::Minor;
    let mut best_score = f32::MIN;

    for root in 0..12usize {
        let mut maj_score = 0.0f32;
        let mut min_score = 0.0f32;
        for deg in 0..12usize {
            let c = chroma[(root + deg) % 12];
            maj_score += c * MAJOR_PROFILE[deg];
            min_score += c * MINOR_PROFILE[deg];
        }
        if maj_score > best_score {
            best_score = maj_score;
            best_key = root as u8;
            best_scale = Scale::Major;
        }
        if min_score > best_score {
            best_score = min_score;
            best_key = root as u8;
            best_scale = Scale::Minor;
        }
    }

    (best_key, refine_scale(&chroma, best_key, best_scale))
}

/// Estimates tempo (BPM) from the RMS onset flux autocorrelation of `frames`.
fn estimate_bpm_from_frames(frames: &[PitchFrame]) -> f32 {
    if frames.len() < 120 {
        return DEFAULT_BPM;
    }
    let dt = (frames[1].time_seconds - frames[0].time_seconds).max(0.005) as f32;
    let fps = 1.0 / dt;

    let mut onset = vec![0.0f32; frames.len()];
    for i in 1..frames.len() {
        let diff = (frames[i].rms - frames[i - 1].rms).max(0.0) as f32;
        onset[i] = (1.0 + 40.0 * diff).ln();
    }

    let min_bpm = 72.0f32;
    let max_bpm = 156.0f32;
    let mut best_bpm = DEFAULT_BPM;
    let mut best_score = -1.0f32;

    let mut bpm = min_bpm;
    while bpm <= max_bpm {
        let period_frames = (60.0 / bpm) * fps;
        let lag1 = period_frames.round() as usize;
        let lag2 = (period_frames * 2.0).round() as usize;
        if lag1 > 2 && lag2 < onset.len() {
            let mut sum1 = 0.0f32;
            for i in 0..(onset.len() - lag1) {
                sum1 += onset[i] * onset[i + lag1];
            }
            let mut sum2 = 0.0f32;
            for i in 0..(onset.len() - lag2) {
                sum2 += onset[i] * onset[i + lag2];
            }
            let prior = (-0.5 * ((bpm / 110.0).log2() / 0.65).powi(2)).exp();
            let score = (sum1 + 0.5 * sum2) * prior;
            if score > best_score {
                best_score = score;
                best_bpm = bpm.round();
            }
        }
        bpm += 1.0;
    }

    best_bpm
}

#[cfg(test)]
mod backing_tests {
    use std::sync::Arc;

    use super::*;
    use crate::backing::BackingAudio;

    /// Output frame of the first sample louder than half scale.
    fn first_click(engine: &mut CantoEngine, frames: usize) -> Option<usize> {
        let mut out = vec![0.0f32; frames * 2];
        engine.render_stereo_f32(&mut out);
        out.chunks_exact(2).position(|f| f[0].abs() > 0.5)
    }

    /// A silent take and a backing track with one click at `click` frames.
    fn engine_with_click(click: usize, offset_sec: f64, bypass: bool) -> CantoEngine {
        let sr = 48_000u32;
        let mut engine = CantoEngine::new(sr as f32);
        engine
            .load_mono_samples(vec![0.0; sr as usize], sr)
            .unwrap();
        let mut samples = vec![0.0f32; sr as usize * 2];
        samples[click * 2] = 0.9;
        samples[click * 2 + 1] = 0.9;
        engine.set_backing(Some(Backing {
            audio: Arc::new(BackingAudio::from_interleaved(&samples, sr, Vec::new())),
            gain: 1.0,
            offset_sec,
        }));
        engine.bypass = bypass;
        engine.loop_enabled = false;
        engine.set_playing(true);
        engine
    }

    #[test]
    fn the_backing_is_delayed_by_the_shifter_like_the_vocal() {
        let mut engine = engine_with_click(12_000, 0.0, false);
        let latency = engine.shifter.latency() as usize;
        let at = first_click(&mut engine, 24_000).expect("the click is heard");
        // A vocal sample at take frame t is heard at t + latency (see
        // `render_offline`); the backing frame of that moment comes with it.
        assert!(
            at.abs_diff(12_000 + latency) <= 1,
            "{at} vs {}",
            12_000 + latency
        );

        let mut bypassed = engine_with_click(12_000, 0.0, true);
        let at = first_click(&mut bypassed, 24_000).expect("the click is heard");
        assert!(at.abs_diff(12_000) <= 1, "{at}");
    }

    #[test]
    fn the_offset_moves_the_take_into_the_song() {
        // The take starts 0.1 s into the track: the click at 0.25 s of the
        // track is heard 0.15 s into the take.
        let mut engine = engine_with_click(12_000, 0.1, true);
        let at = first_click(&mut engine, 24_000).expect("the click is heard");
        assert!(at.abs_diff(7_200) <= 1, "{at}");
    }

    #[test]
    fn previews_meters_and_exports_leave_the_backing_out() {
        let mut engine = engine_with_click(100, 0.0, true);
        let mut out = vec![0.0f32; 400];
        engine.render_stereo_f32(&mut out);
        assert!(out.iter().any(|s| s.abs() > 0.5));
        assert_eq!(engine.meter_level(), 0.0, "the meter shows the vocal");
        assert!(engine.render_offline().iter().all(|&s| s == 0.0));

        engine.set_playing(false);
        engine.seek(0.0);
        let mut out = vec![1.0f32; 400];
        engine.render_stereo_f32(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "stopped: no backing");
    }
}

#[cfg(test)]
mod i16_render_tests {
    use super::*;

    fn playing_sine(sr: u32) -> CantoEngine {
        let mut engine = CantoEngine::new(sr as f32);
        let samples = (0..sr as usize)
            .map(|i| 0.5 * (i as f32 * 220.0 * std::f32::consts::TAU / sr as f32).sin())
            .collect();
        engine.load_mono_samples(samples, sr).unwrap();
        engine.loop_enabled = false;
        engine.set_playing(true);
        engine
    }

    /// A callback larger than any before it (first callback, route change)
    /// must not grow the scratch on the audio thread, and rendering it in
    /// chunks must produce the same samples as one pass.
    #[test]
    fn a_long_i16_callback_renders_in_chunks_without_growing_the_scratch() {
        let sr = 48_000;
        let frames = I16_RENDER_CHUNK_FRAMES * 3 + 77;
        let mut chunked = playing_sine(sr);
        let scratch_ptr = chunked.scratch.as_ptr();
        let scratch_cap = chunked.scratch.capacity();
        let mut out = vec![0i16; frames * 2];
        chunked.render_stereo_i16(&mut out);
        assert_eq!(chunked.scratch.as_ptr(), scratch_ptr, "scratch reallocated");
        assert_eq!(chunked.scratch.capacity(), scratch_cap);

        let mut whole = playing_sine(sr);
        let mut reference = vec![0.0f32; frames * 2];
        whole.render_stereo_f32(&mut reference);
        let expected: Vec<i16> = reference
            .iter()
            .map(|&s| (clamp(s, -0.99, 0.99) * i16::MAX as f32) as i16)
            .collect();
        assert!(out.iter().any(|&s| s != 0), "the take is heard");
        assert_eq!(out, expected);
    }
}
