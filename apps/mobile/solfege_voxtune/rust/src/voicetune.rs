//! Monophonic vocal pitch analysis, note segmentation, non-destructive correction model,
//! and TD-PSOLA offline rendering.
//! Borrowed from `Futureboard-release/crates/SphereVoiceTune` and upgraded with real TD-PSOLA rendering.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::dsp_core::db_to_linear;
use crate::shifter::{Control, MAX_FORMANT, Shifter};

#[derive(Debug, Error, PartialEq)]
pub enum VoiceTuneError {
    #[error("Invalid sample rate: {0}")]
    InvalidSampleRate(u32),
    #[error("Invalid analysis parameters: {0}")]
    InvalidParameters(String),
    #[error("Empty audio buffer")]
    EmptyBuffer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PitchFrame {
    pub time_seconds: f64,
    pub frequency_hz: f64,
    pub confidence: f64,
    pub rms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceNote {
    pub id: String,
    pub start_time: f64,
    pub end_time: f64,
    pub detected_pitch_hz: f64,
    pub detected_midi_note: u8,
    pub average_cents_offset: f64,
    pub confidence: f64,
    pub gain: f32,
    pub corrected_midi_note: u8,
    pub pitch_offset_cents: f64,
    pub timing_offset: f64,
    pub formant_shift: f64,
}

impl VoiceNote {
    pub fn new(
        id: String,
        start_time: f64,
        end_time: f64,
        detected_pitch_hz: f64,
        confidence: f64,
        gain: f32,
    ) -> Self {
        let (midi, cents) = hz_to_midi_cents(detected_pitch_hz);
        Self {
            id,
            start_time,
            end_time,
            detected_pitch_hz,
            detected_midi_note: midi,
            average_cents_offset: cents,
            confidence,
            gain,
            corrected_midi_note: midi,
            pitch_offset_cents: 0.0,
            timing_offset: 0.0,
            formant_shift: 0.0,
        }
    }

    pub fn reset_edits(&mut self) {
        self.corrected_midi_note = self.detected_midi_note;
        self.pitch_offset_cents = 0.0;
        self.timing_offset = 0.0;
        self.formant_shift = 0.0;
    }

    pub fn target_frequency_hz(&self) -> f64 {
        let target_midi = self.corrected_midi_note as f64 + self.pitch_offset_cents / 100.0;
        440.0 * 2.0f64.powf((target_midi - 69.0) / 12.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceTuneAnalysisConfig {
    pub min_frequency: f64,
    pub max_frequency: f64,
    pub frame_size: usize,
    pub hop_size: usize,
    pub voiced_threshold: f64,
    pub rms_threshold: f64,
}

impl Default for VoiceTuneAnalysisConfig {
    fn default() -> Self {
        Self {
            min_frequency: 60.0,
            max_frequency: 800.0,
            frame_size: 2048,
            hop_size: 512,
            voiced_threshold: 0.35,
            rms_threshold: 0.005,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceTuneDocument {
    pub sample_rate: u32,
    pub duration_seconds: f64,
    pub pitch_frames: Vec<PitchFrame>,
    pub notes: Vec<VoiceNote>,
    pub analysis_metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceTuneRenderPlan {
    pub note_edits: Vec<VoiceNote>,
    pub correction_mode: String,
    pub offline_instructions: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceCorrectionParams {
    pub pitch_center_correction: f64,
    pub pitch_drift_correction: f64,
    pub vibrato_scale: f64,
    pub timing_stretch: f64,
    pub formant_shift: f64,
    pub gain_db: f32,
}

impl Default for VoiceCorrectionParams {
    fn default() -> Self {
        Self {
            pitch_center_correction: 1.0,
            pitch_drift_correction: 0.0,
            vibrato_scale: 1.0,
            timing_stretch: 1.0,
            formant_shift: 0.0,
            gain_db: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormantConfig {
    pub shift_semitones: f64,
    pub preserve_formants: bool,
}

impl Default for FormantConfig {
    fn default() -> Self {
        Self {
            shift_semitones: 0.0,
            preserve_formants: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingCorrection {
    pub time_stretch_factor: f64,
    pub alignment_grid_seconds: f64,
}

impl Default for TimingCorrection {
    fn default() -> Self {
        Self {
            time_stretch_factor: 1.0,
            alignment_grid_seconds: 0.0,
        }
    }
}

pub fn hz_to_midi_cents(hz: f64) -> (u8, f64) {
    if hz <= 0.0 {
        return (0, 0.0);
    }
    let midi_f = 12.0 * (hz / 440.0).log2() + 69.0;
    let midi = midi_f.round();
    let midi_u8 = midi.clamp(0.0, 127.0) as u8;
    let target_hz = midi_to_hz(midi_u8);
    let cents = 1200.0 * (hz / target_hz).log2();
    (midi_u8, cents)
}

pub fn midi_to_hz(midi: u8) -> f64 {
    440.0 * 2.0f64.powf((midi as f64 - 69.0) / 12.0)
}

pub fn detect_pitch_yin(
    frame: &[f32],
    sample_rate: u32,
    min_freq: f64,
    max_freq: f64,
    voiced_threshold: f64,
) -> (f64, f64) {
    if sample_rate == 0 || frame.is_empty() || min_freq <= 0.0 || max_freq <= min_freq {
        return (0.0, 0.0);
    }

    let tau_min = (sample_rate as f64 / max_freq).round() as usize;
    let tau_max = (sample_rate as f64 / min_freq).round() as usize;

    if frame.len() <= tau_max {
        return (0.0, 0.0);
    }

    let w = frame.len() - tau_max;
    let mut d = vec![0.0f64; tau_max];
    for tau in 0..tau_max {
        let mut sum = 0.0;
        for j in 0..w {
            let diff = frame[j] as f64 - frame[j + tau] as f64;
            sum += diff * diff;
        }
        d[tau] = sum;
    }

    let mut d_prime = vec![0.0f64; tau_max];
    d_prime[0] = 1.0;
    let mut running_sum = 0.0;
    for tau in 1..tau_max {
        running_sum += d[tau];
        if running_sum > 0.0 {
            d_prime[tau] = d[tau] / (running_sum / tau as f64);
        } else {
            d_prime[tau] = 1.0;
        }
    }

    let threshold = 0.15;
    let mut opt_tau = None;
    for tau in tau_min..tau_max {
        if d_prime[tau] < threshold
            && tau + 1 < tau_max
            && d_prime[tau] < d_prime[tau - 1]
            && d_prime[tau] < d_prime[tau + 1]
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
            for (t, &value) in d_prime.iter().enumerate().take(tau_max).skip(tau_min) {
                if value < min_val {
                    min_val = value;
                    min_tau = t;
                }
            }
            min_tau
        }
    };

    if tau == 0 || tau >= tau_max {
        return (0.0, 0.0);
    }

    let mut tau_interpolated = tau as f64;
    if tau > 0 && tau + 1 < d_prime.len() {
        let alpha = d_prime[tau - 1];
        let beta = d_prime[tau];
        let gamma = d_prime[tau + 1];
        let denom = alpha - 2.0 * beta + gamma;
        if denom.abs() > 1e-5 {
            let p = 0.5 * (alpha - gamma) / denom;
            if p.abs() <= 0.5 {
                tau_interpolated = tau as f64 + p;
            }
        }
    }

    let freq = sample_rate as f64 / tau_interpolated;
    let confidence = 1.0 - d_prime[tau].min(1.0);

    if freq >= min_freq && freq <= max_freq && confidence >= voiced_threshold {
        (freq, confidence)
    } else {
        (0.0, confidence)
    }
}

pub fn calculate_rms(frame: &[f32]) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0;
    for &sample in frame {
        sum += sample as f64 * sample as f64;
    }
    (sum / frame.len() as f64).sqrt()
}

pub fn segment_notes(
    frames: &[PitchFrame],
    voiced_threshold: f64,
    rms_threshold: f64,
    min_note_duration: f64,
) -> Vec<VoiceNote> {
    let mut notes = Vec::new();
    let mut note_counter = 1;

    let mut voiced_segments: Vec<Vec<&PitchFrame>> = Vec::new();
    let mut current_segment = Vec::new();

    for frame in frames {
        let is_voiced = frame.confidence >= voiced_threshold
            && frame.rms >= rms_threshold
            && frame.frequency_hz > 0.0;
        if is_voiced {
            current_segment.push(frame);
        } else if !current_segment.is_empty() {
            voiced_segments.push(current_segment);
            current_segment = Vec::new();
        }
    }
    if !current_segment.is_empty() {
        voiced_segments.push(current_segment);
    }

    for segment in voiced_segments {
        if segment.is_empty() {
            continue;
        }

        let raw_midi: Vec<f64> = segment
            .iter()
            .map(|f| {
                let (midi, cents) = hz_to_midi_cents(f.frequency_hz);
                midi as f64 + cents / 100.0
            })
            .collect();

        let mut smoothed_midi = vec![0.0; raw_midi.len()];
        let window_size = 5;
        let half_win = window_size / 2;
        for (i, smoothed) in smoothed_midi.iter_mut().enumerate() {
            let start = i.saturating_sub(half_win);
            let end = (i + half_win + 1).min(raw_midi.len());
            let sum: f64 = raw_midi[start..end].iter().sum();
            *smoothed = sum / (end - start) as f64;
        }

        let mut current_note_frames = Vec::new();
        let mut current_target_midi = smoothed_midi[0].round() as u8;

        for (idx, frame) in segment.iter().enumerate() {
            let frame_midi = smoothed_midi[idx].round() as u8;
            if frame_midi == current_target_midi {
                current_note_frames.push(*frame);
            } else {
                if !current_note_frames.is_empty() {
                    let note = create_voice_note_from_frames(
                        &format!("note_{}", note_counter),
                        &current_note_frames,
                    );
                    if (note.end_time - note.start_time) >= min_note_duration {
                        notes.push(note);
                        note_counter += 1;
                    }
                }
                current_note_frames = vec![*frame];
                current_target_midi = frame_midi;
            }
        }

        if !current_note_frames.is_empty() {
            let note = create_voice_note_from_frames(
                &format!("note_{}", note_counter),
                &current_note_frames,
            );
            if (note.end_time - note.start_time) >= min_note_duration {
                notes.push(note);
                note_counter += 1;
            }
        }
    }

    notes
}

fn create_voice_note_from_frames(id: &str, frames: &[&PitchFrame]) -> VoiceNote {
    let start_time = frames[0].time_seconds;
    let end_time = frames[frames.len() - 1].time_seconds;

    let count = frames.len() as f64;
    let mut sum_hz = 0.0;
    let mut sum_conf = 0.0;
    let mut sum_rms = 0.0;

    for f in frames {
        sum_hz += f.frequency_hz;
        sum_conf += f.confidence;
        sum_rms += f.rms;
    }

    VoiceNote::new(
        id.to_string(),
        start_time,
        end_time,
        sum_hz / count,
        sum_conf / count,
        (sum_rms / count) as f32,
    )
}

pub fn update_note_correction(
    note: &mut VoiceNote,
    corrected_midi_note: Option<u8>,
    pitch_offset_cents: Option<f64>,
    timing_offset: Option<f64>,
    formant_shift: Option<f64>,
    gain: Option<f32>,
) {
    if let Some(midi) = corrected_midi_note {
        note.corrected_midi_note = midi;
    }
    if let Some(cents) = pitch_offset_cents {
        note.pitch_offset_cents = cents;
    }
    if let Some(timing) = timing_offset {
        note.timing_offset = timing;
    }
    if let Some(formant) = formant_shift {
        note.formant_shift = formant;
    }
    if let Some(g) = gain {
        note.gain = g;
    }
}

pub fn apply_timing_offset(notes: &mut [VoiceNote], note_id: &str, offset_seconds: f64) {
    if let Some(note) = notes.iter_mut().find(|n| n.id == note_id) {
        note.timing_offset = offset_seconds;
    }
}

pub fn generate_render_plan(notes: &[VoiceNote], correction_mode: &str) -> VoiceTuneRenderPlan {
    let mut note_edits = Vec::new();
    for note in notes {
        let has_edits = note.corrected_midi_note != note.detected_midi_note
            || note.pitch_offset_cents != 0.0
            || note.timing_offset != 0.0
            || note.formant_shift != 0.0;

        if has_edits {
            note_edits.push(note.clone());
        }
    }

    let offline_instructions = format!(
        "Offline render plan: Apply TD-PSOLA pitch/formant/time corrections to {} modified notes using mode '{}'.",
        note_edits.len(),
        correction_mode
    );

    VoiceTuneRenderPlan {
        note_edits,
        correction_mode: correction_mode.to_string(),
        offline_instructions,
    }
}

pub struct VoiceTuneAnalyzer;

impl VoiceTuneAnalyzer {
    pub fn analyze_mono(
        samples: &[f32],
        sample_rate: u32,
        config: &VoiceTuneAnalysisConfig,
    ) -> Result<VoiceTuneDocument, VoiceTuneError> {
        if sample_rate == 0 {
            return Err(VoiceTuneError::InvalidSampleRate(0));
        }
        if config.frame_size == 0 || config.hop_size == 0 {
            return Err(VoiceTuneError::InvalidParameters(
                "Frame size and hop size must be greater than zero".to_string(),
            ));
        }

        let total_samples = samples.len();
        let duration_seconds = total_samples as f64 / sample_rate as f64;
        let mut pitch_frames = Vec::new();

        let mut i = 0;
        while i < total_samples {
            let mut frame = vec![0.0f32; config.frame_size];
            let end = (i + config.frame_size).min(total_samples);
            let len = end - i;
            frame[..len].copy_from_slice(&samples[i..end]);

            let time_seconds = i as f64 / sample_rate as f64;
            let rms = calculate_rms(&frame);

            let (frequency_hz, confidence) = if rms >= config.rms_threshold {
                detect_pitch_yin(
                    &frame,
                    sample_rate,
                    config.min_frequency,
                    config.max_frequency,
                    config.voiced_threshold,
                )
            } else {
                (0.0, 0.0)
            };

            pitch_frames.push(PitchFrame {
                time_seconds,
                frequency_hz,
                confidence,
                rms,
            });

            i += config.hop_size;
        }

        let min_note_duration = 0.08;
        let notes = segment_notes(
            &pitch_frames,
            config.voiced_threshold,
            config.rms_threshold,
            min_note_duration,
        );

        let mut analysis_metadata = HashMap::new();
        analysis_metadata.insert("pitch_detection_algorithm".to_string(), "YIN".to_string());
        analysis_metadata.insert("frame_size".to_string(), config.frame_size.to_string());
        analysis_metadata.insert("hop_size".to_string(), config.hop_size.to_string());
        analysis_metadata.insert("total_frames".to_string(), pitch_frames.len().to_string());

        Ok(VoiceTuneDocument {
            sample_rate,
            duration_seconds,
            pitch_frames,
            notes,
            analysis_metadata,
        })
    }
}

pub struct OfflineVoiceRenderer;

impl OfflineVoiceRenderer {
    /// Renders edited vocal samples through the TD-PSOLA `Shifter` according to a `VoiceTuneRenderPlan`.
    pub fn render_offline(
        original_samples: &[f32],
        sample_rate: u32,
        plan: &VoiceTuneRenderPlan,
    ) -> Result<Vec<f32>, VoiceTuneError> {
        if sample_rate == 0 {
            return Err(VoiceTuneError::InvalidSampleRate(0));
        }
        if plan.note_edits.is_empty() || original_samples.is_empty() {
            return Ok(original_samples.to_vec());
        }

        let sr = sample_rate as f32;
        let mut shifter = Shifter::new(sr, sr / 55.0);
        shifter.set_longest_period(sr / 55.0, 0);
        let latency = shifter.latency() as usize;
        let total = original_samples.len();
        let mut output = Vec::with_capacity(total);
        let hop = 128usize;

        for idx in 0..(total + latency) {
            if idx % hop == 0 {
                let t = idx as f64 / sr as f64;
                let active_note = plan
                    .note_edits
                    .iter()
                    .find(|n| t >= n.start_time && t <= n.end_time);
                let control = if let Some(note) = active_note {
                    let det_hz = note.detected_pitch_hz.max(55.0) as f32;
                    let tgt_hz = note.target_frequency_hz().max(55.0) as f32;
                    let ratio = (tgt_hz / det_hz).clamp(0.25, 4.0);
                    let formant = 2.0f32
                        .powf(note.formant_shift as f32 / 12.0)
                        .clamp(1.0 / MAX_FORMANT, MAX_FORMANT);
                    Control {
                        centre: idx as u64,
                        period: Some(sr / det_hz),
                        ratio,
                        formant,
                        gain: db_to_linear(note.gain),
                    }
                } else {
                    Control {
                        centre: idx as u64,
                        period: None,
                        ratio: 1.0,
                        formant: 1.0,
                        gain: 1.0,
                    }
                };
                shifter.push_control(control);
            }

            let sample = if idx < total { original_samples[idx] } else { 0.0 };
            let (wet, _) = shifter.process(sample, sample);
            if idx >= latency {
                output.push(wet[0]);
            }
        }

        Ok(output)
    }
}
