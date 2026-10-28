//! CANTO Vocal Audio Engine, Real-Time TD-PSOLA Processor & Audio Analyzer.
//!
//! Decodes real audio files (`audio_io`), analyzes pitch contours, note segments,
//! musical key/scale, and BPM (`voicetune` + Krumhansl-Kessler key profiles), and
//! streams real-time TD-PSOLA pitch/formant/time/level shifted audio via `Shifter`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::audio_io::{decode_audio_bytes_to_mono, decode_audio_file_to_mono};
use crate::dsp_core::{
    SoftKneeCompressor, StereoEffect, clamp, db_to_linear,
};
use crate::scale::{ALL_NOTES, Scale, nearest_target, scale_mask};
use crate::shifter::{Control, MAX_FORMANT, Shifter};
use crate::voicetune::{PitchFrame, calculate_rms, hz_to_midi_cents};
use crate::whitesharp_dsp::WhiteSharpDsp;

pub const DEFAULT_BPM: f32 = 120.0;
pub const BEATS_PER_BAR: f32 = 4.0;
pub const CONTOUR_POINTS: usize = 32;

const KEY_NAMES: [&str; 12] = [
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
    note.pitch + contour_cents(note, t, note.modulation) / 100.0
}

pub fn detected_pitch_at(note: &CantoNote, t: f32) -> f32 {
    note.detected + contour_cents(note, t, 1.0) / 100.0
}

pub fn solfege_for_midi(midi: f32, key_pc: u8) -> String {
    let pc = (midi.round() as i32).rem_euclid(12) as usize;
    let rel = (pc + 12 - (key_pc as usize % 12)) % 12;
    SOLFEGE_SYLLABLES[rel].to_string()
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
    pub playhead_sec: f64,
    pub correct_amount: f32,
    /// Decoded mono audio samples of the loaded vocal file (at `sample_rate`).
    pub audio_samples: Vec<f32>,
    /// Total duration of the loaded audio in seconds.
    pub duration_sec: f64,
    /// Downsampled peak envelope for the overview waveform.
    pub waveform_peaks: Vec<f32>,
    /// Analyzed and user-edited vocal notes.
    pub notes: Vec<CantoNote>,
    /// Real-time TD-PSOLA pitch & formant shifter.
    shifter: Shifter,
    /// Studio vocal leveling compressor.
    compressor: SoftKneeCompressor,
    /// Embedded WhiteSharp live audio processor for external input buffers.
    pub whitesharp: WhiteSharpDsp,
    control_counter: usize,
    meter_level: f32,
    /// Optional note index currently being auditioned while dragging in the editor.
    audition_note_index: Option<usize>,
    audition_cursor_sec: f64,
    audition_remaining_frames: usize,
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

        Self {
            sample_rate: sr,
            bpm: DEFAULT_BPM,
            key: 9, // A
            scale: Scale::Minor,
            playing: false,
            loop_enabled: true,
            bypass: false,
            playhead_sec: 0.0,
            correct_amount: 0.0,
            audio_samples: Vec::new(),
            duration_sec: 0.0,
            waveform_peaks: Vec::new(),
            notes: Vec::new(),
            shifter,
            compressor,
            whitesharp: WhiteSharpDsp::new(sr),
            control_counter: 0,
            meter_level: 0.0,
            audition_note_index: None,
            audition_cursor_sec: 0.0,
            audition_remaining_frames: 0,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn meter_level(&self) -> f32 {
        self.meter_level
    }

    pub fn has_audio(&self) -> bool {
        !self.audio_samples.is_empty()
    }

    /// Loads an audio file from `path`, decodes + resamples it to `self.sample_rate`,
    /// and immediately runs full vocal pitch/note/key/BPM analysis.
    pub fn load_audio_file(&mut self, path: &Path) -> Result<CantoAnalysisResult, String> {
        let decoded = decode_audio_file_to_mono(path, self.sample_rate as u32)?;
        self.playing = false;
        self.playhead_sec = 0.0;
        self.correct_amount = 0.0;
        self.audio_samples = decoded.samples;
        self.duration_sec = decoded.duration_sec;
        self.waveform_peaks = decoded.waveform_peaks;
        self.shifter.reset();
        self.compressor.reset();
        self.analyze_loaded_audio()
    }

    /// Loads audio from raw file bytes in memory, decodes + resamples it to `self.sample_rate`,
    /// and immediately runs full vocal pitch/note/key/BPM analysis.
    pub fn load_audio_bytes(
        &mut self,
        bytes: &[u8],
        ext_hint: Option<&str>,
    ) -> Result<CantoAnalysisResult, String> {
        let decoded = decode_audio_bytes_to_mono(bytes, ext_hint, self.sample_rate as u32)?;
        self.playing = false;
        self.playhead_sec = 0.0;
        self.correct_amount = 0.0;
        self.audio_samples = decoded.samples;
        self.duration_sec = decoded.duration_sec;
        self.waveform_peaks = decoded.waveform_peaks;
        self.shifter.reset();
        self.compressor.reset();
        self.analyze_loaded_audio()
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
            crate::audio_io::resample_mono_cubic(&samples, sample_rate, target_sr)
        };
        self.playing = false;
        self.playhead_sec = 0.0;
        self.correct_amount = 0.0;
        self.duration_sec = mono.len() as f64 / self.sample_rate as f64;
        self.waveform_peaks = crate::audio_io::compute_waveform_peaks(&mono, 240);
        self.audio_samples = mono;
        self.shifter.reset();
        self.compressor.reset();
        self.analyze_loaded_audio()
    }

    /// Analyzes `self.audio_samples` to extract vocal notes, real pitch contours,
    /// amplitude envelopes, musical key/scale, and tempo (BPM).
    pub fn analyze_loaded_audio(&mut self) -> Result<CantoAnalysisResult, String> {
        if self.audio_samples.is_empty() {
            return Err("No audio loaded to analyze".to_string());
        }

        let sr = self.sample_rate as u32;
        let frames = extract_pitch_frames(&self.audio_samples, sr);

        let (key_pc, scale_mode) = estimate_key_and_mode(&frames);
        self.key = key_pc;
        self.scale = scale_mode;
        self.bpm = estimate_bpm_from_frames(&frames).clamp(68.0, 172.0);
        self.correct_amount = 0.0;

        self.notes = segment_canto_notes(&frames, self.key);

        let mode_str = match self.scale {
            Scale::Major => "major",
            Scale::MinorPentatonic => "penta",
            _ => "minor",
        };

        Ok(CantoAnalysisResult {
            ok: true,
            error: None,
            sample_rate: sr,
            duration_sec: self.duration_sec,
            bpm: self.bpm,
            key: KEY_NAMES[(self.key as usize) % 12].to_string(),
            mode: mode_str.to_string(),
            notes: self.notes.clone(),
            waveform_peaks: self.waveform_peaks.clone(),
        })
    }

    pub fn set_key_and_scale(&mut self, key: u8, scale: Scale) {
        self.key = key % 12;
        self.scale = scale;
        for note in &mut self.notes {
            if !note.unpitched {
                note.lyric = solfege_for_midi(note.pitch, self.key);
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
        for note in &mut self.notes {
            if note.unpitched || note.pinned {
                continue;
            }
            if let Some(target) = nearest_target(note.detected, allowed, None) {
                note.pitch = note.detected + (target as f32 - note.detected) * self.correct_amount;
            }
        }
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
    ) {
        if let Some(note) = self.notes.get_mut(index) {
            note.pitch = pitch;
            note.start = start;
            note.dur = dur.max(0.05);
            note.modulation = clamp(modulation, 0.0, 1.45);
            note.formant = clamp(formant, -12.0, 12.0);
            note.gain_db = clamp(gain_db, -18.0, 6.0);
            note.pinned = pinned;
            if !note.unpitched {
                note.lyric = solfege_for_midi(note.pitch, self.key);
            }
        }
    }

    /// Triggers short real-audio audition of a note when dragging it while paused.
    pub fn trigger_note_audition(&mut self, note_index: usize) {
        if note_index < self.notes.len() && !self.audio_samples.is_empty() {
            if self.audition_note_index != Some(note_index) {
                let n = &self.notes[note_index];
                self.audition_cursor_sec = n.detected_start as f64;
                self.audition_note_index = Some(note_index);
            }
            // Keep audition alive for ~140ms after the last pointer move update
            self.audition_remaining_frames = (self.sample_rate * 0.14) as usize;
        }
    }

    pub fn stop_audition(&mut self) {
        self.audition_note_index = None;
        self.audition_remaining_frames = 0;
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

    fn note_at(&self, time_sec: f32) -> Option<CantoNote> {
        for note in self.notes.iter().rev() {
            let shown = note.present(self.bypass);
            if time_sec >= shown.start && time_sec <= shown.start + shown.dur {
                return Some(shown);
            }
        }
        None
    }

    fn is_vacated_original_region(&self, time_sec: f32) -> bool {
        if self.bypass {
            return false;
        }
        for note in &self.notes {
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
        let frames = out_interleaved.len() / 2;
        if frames == 0 {
            return self.meter_level;
        }

        if self.audio_samples.is_empty() || self.duration_sec <= 0.0 {
            out_interleaved.fill(0.0);
            self.meter_level = 0.0;
            return 0.0;
        }

        let sr = self.sample_rate;
        let dt = 1.0 / sr as f64;
        let total_dur = self.duration_sec;
        let mut block_peak = 0.0f32;

        for i in 0..frames {
            if !self.playing {
                if let Some(note_idx) = self.audition_note_index {
                    if self.audition_remaining_frames > 0 && note_idx < self.notes.len() {
                        self.audition_remaining_frames -= 1;
                        let note = self.notes[note_idx].clone();
                        let det_start = note.detected_start as f64;
                        let det_end = (note.detected_start + note.detected_dur.max(0.05)) as f64;
                        self.audition_cursor_sec += dt;
                        if self.audition_cursor_sec >= det_end
                            || self.audition_cursor_sec < det_start
                        {
                            self.audition_cursor_sec = det_start;
                        }
                        let local_t = ((self.audition_cursor_sec - det_start)
                            / (det_end - det_start).max(0.01))
                        .clamp(0.0, 1.0) as f32;
                        let dry = self.sample_interpolated(self.audition_cursor_sec);

                        let src_midi = detected_pitch_at(&note, local_t);
                        let dst_midi = pitch_at(&note, local_t);
                        let src_hz = midi_to_hz(src_midi).clamp(55.0, 1400.0);
                        let dst_hz = midi_to_hz(dst_midi).clamp(55.0, 1400.0);
                        let ratio = (dst_hz / src_hz).clamp(0.25, 4.0);
                        let formant = 2.0f32
                            .powf(note.formant / 12.0)
                            .clamp(1.0 / MAX_FORMANT, MAX_FORMANT);
                        let gain_lin =
                            db_to_linear(note.gain_db - note.detected_gain).clamp(0.05, 4.0);

                        if self.control_counter == 0 {
                            self.shifter.push_control(Control {
                                centre: self.shifter.now(),
                                period: Some(sr / src_hz),
                                ratio,
                                formant,
                                gain: gain_lin,
                            });
                        }
                        self.control_counter = (self.control_counter + 1) & 63;

                        let (wet, _) = self.shifter.process(dry, dry);
                        let (l, r) = self.compressor.process_stereo_linked(wet[0], wet[1]);
                        let out_l = clamp(l, -0.98, 0.98);
                        let out_r = clamp(r, -0.98, 0.98);
                        out_interleaved[i * 2] = out_l;
                        out_interleaved[i * 2 + 1] = out_r;
                        block_peak = block_peak.max(out_l.abs().max(out_r.abs()));
                        continue;
                    } else {
                        self.audition_note_index = None;
                    }
                }

                out_interleaved[i * 2] = 0.0;
                out_interleaved[i * 2 + 1] = 0.0;
                continue;
            }

            self.playhead_sec += dt;
            if self.playhead_sec >= total_dur {
                if self.loop_enabled {
                    self.playhead_sec %= total_dur;
                } else {
                    self.playhead_sec = total_dur;
                    self.playing = false;
                    out_interleaved[i * 2] = 0.0;
                    out_interleaved[i * 2 + 1] = 0.0;
                    continue;
                }
            }

            if self.bypass {
                let raw = self.sample_interpolated(self.playhead_sec);
                if self.control_counter == 0 {
                    self.shifter.push_control(Control {
                        centre: self.shifter.now(),
                        period: None,
                        ratio: 1.0,
                        formant: 1.0,
                        gain: 1.0,
                    });
                }
                self.control_counter = (self.control_counter + 1) & 63;
                let _ = self.shifter.process(raw, raw);
                let s = clamp(raw, -0.98, 0.98);
                out_interleaved[i * 2] = s;
                out_interleaved[i * 2 + 1] = s;
                block_peak = block_peak.max(s.abs());
                continue;
            }

            let active = self.note_at(self.playhead_sec as f32);

            let mut target_ratio = 1.0f32;
            let mut target_formant = 1.0f32;
            let mut period_opt = None;
            let mut psola_gain = 1.0f32;
            let dry_sample: f32;

            if let Some(note) = &active {
                let local_t = ((self.playhead_sec as f32 - note.start) / note.dur.max(0.01))
                    .clamp(0.0, 1.0);
                let src_time_sec =
                    (note.detected_start + local_t * note.detected_dur) as f64;
                dry_sample = self.sample_interpolated(src_time_sec);
                psola_gain = db_to_linear(note.gain_db - note.detected_gain).clamp(0.05, 4.0);

                if !note.unpitched {
                    let src_midi = detected_pitch_at(note, local_t);
                    let dst_midi = pitch_at(note, local_t);
                    let src_hz = midi_to_hz(src_midi).clamp(55.0, 1400.0);
                    let dst_hz = midi_to_hz(dst_midi).clamp(55.0, 1400.0);

                    period_opt = Some(sr / src_hz);
                    target_ratio = (dst_hz / src_hz).clamp(0.25, 4.0);
                    target_formant = 2.0f32
                        .powf(note.formant / 12.0)
                        .clamp(1.0 / MAX_FORMANT, MAX_FORMANT);
                }
            } else if self.is_vacated_original_region(self.playhead_sec as f32) {
                dry_sample = 0.0;
            } else {
                dry_sample = self.sample_interpolated(self.playhead_sec);
            }

            if self.control_counter == 0 {
                self.shifter.push_control(Control {
                    centre: self.shifter.now(),
                    period: period_opt,
                    ratio: target_ratio,
                    formant: target_formant,
                    gain: psola_gain,
                });
            }
            self.control_counter = (self.control_counter + 1) & 63;

            let (wet, _) = self.shifter.process(dry_sample, dry_sample);
            let (l, r) = self.compressor.process_stereo_linked(wet[0], wet[1]);

            let out_l = clamp(l, -0.98, 0.98);
            let out_r = clamp(r, -0.98, 0.98);
            out_interleaved[i * 2] = out_l;
            out_interleaved[i * 2 + 1] = out_r;
            block_peak = block_peak.max(out_l.abs().max(out_r.abs()));
        }

        self.meter_level += (block_peak - self.meter_level) * 0.45;
        self.meter_level
    }

    pub fn render_stereo_i16(&mut self, out_interleaved: &mut [i16]) -> f32 {
        let mut float_buf = vec![0.0f32; out_interleaved.len()];
        let level = self.render_stereo_f32(&mut float_buf);
        for (dst, &src) in out_interleaved.iter_mut().zip(float_buf.iter()) {
            *dst = (clamp(src, -0.99, 0.99) * i16::MAX as f32) as i16;
        }
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

fn extract_pitch_frames(samples: &[f32], sample_rate: u32) -> Vec<PitchFrame> {
    let hop_size = ((sample_rate as usize) / 100).max(128); // ~10 ms hop
    let frame_size = ((sample_rate as usize) * 42 / 1000).max(1024); // ~42 ms window
    let min_freq = 65.0;
    let max_freq = 950.0;
    let voiced_threshold = 0.42;

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

    for (start, end, rms) in raw_rms {
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

    frames
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
                        combined_contour
                            .push(clamp(sample_curve(&prev.contour, lt) + shift_cents, -55.0, 55.0));
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
            plateau_midi = smooth_midi[i..lookahead_end].iter().sum::<f64>()
                / (lookahead_end - i) as f64;
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
        let local_mean =
            (smooth_contour[k - 2] + smooth_contour[k] + smooth_contour[k + 2]) / 3.0;
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

/// Estimates musical Key (`0..11`) and Scale (`Scale::Minor` or `Scale::Major`)
/// from voiced `PitchFrame`s using Krumhansl-Kessler profiles.
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

    (best_key, best_scale)
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
