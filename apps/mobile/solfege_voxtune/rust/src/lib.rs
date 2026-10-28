//! `canto_dsp` — Rust Audio Engine & Vocal Pitch/Formant DSP for CANTO (Solfege VoxTune).
//!
//! Combines DSP modules borrowed from `Futureboard-release/crates`:
//! - `audio_io`: Multi-format audio file decoder (WAV/MP3/FLAC/OGG/M4A/AAC/AIFF) & cubic resampler (`SphereDirectAudioEngine`)
//! - `dsp_core`: Denormal-safe filters, delay rings, Schroeder allpass, and soft-knee compressor (`builtin_dsp_core`)
//! - `detect`: Decimated + full-rate parabolic-refined YIN pitch detector (`whitesharp::detect`)
//! - `shifter`: Stereo TD-PSOLA pitch & formant shifter (`whitesharp::shifter`)
//! - `splice`: Zero-lookahead live pitch-synchronous splicer (`whitesharp::splice`)
//! - `scale`: Scale bitmasks & hysteresis-stabilized target resolution (`whitesharp::scale`)
//! - `whitesharp_dsp`: Real-time Auto-Tune / vibrato / throat / formant processor (`whitesharp`)
//! - `voicetune`: Offline YIN analysis, note segmentation & TD-PSOLA render plan (`SphereVoiceTune`)
//! - `engine`: CANTO real-time audio file analyzer & TD-PSOLA vocal phrase processor (`CantoEngine`)
//! - `ffi`: C-ABI FFI surface for Flutter (`dart:ffi`) and Android native audio

pub mod audio_io;
pub mod detect;
pub mod dsp_core;
pub mod engine;
pub mod ffi;
pub mod scale;
pub mod shifter;
pub mod splice;
pub mod voicetune;
pub mod whitesharp_dsp;

pub use audio_io::{
    DecodedAudio, compute_waveform_peaks, decode_audio_bytes_to_mono, decode_audio_file_to_mono,
    encode_mono_wav_i16, resample_mono_cubic,
};
pub use detect::{Detector, Estimate};
pub use dsp_core::{
    Allpass, DelayRing, Smoothed, SoftKneeCompressor, StereoEffect, clamp, db_to_linear,
    flush_denormal, linear_to_db, make_eq_biquad, make_eq_coefficients, mix, smoothing_step,
};
pub use engine::{CantoAnalysisResult, CantoEngine, CantoNote, midi_to_hz};
pub use scale::{ALL_NOTES, NOTE_NAMES, Scale, nearest_target, pitch_class, scale_mask};
pub use shifter::{Control, MAX_FORMANT, Shifter};
pub use splice::{LATENCY, Splicer};
pub use voicetune::{
    FormantConfig, OfflineVoiceRenderer, PitchFrame, TimingCorrection, VoiceCorrectionParams,
    VoiceNote, VoiceTuneAnalysisConfig, VoiceTuneAnalyzer, VoiceTuneDocument, VoiceTuneError,
    VoiceTuneRenderPlan, calculate_rms, detect_pitch_yin, generate_render_plan, hz_to_midi_cents,
    segment_notes, update_note_correction,
};
pub use whitesharp_dsp::{
    InputType, LatencyMode, Params, VibratoShape, WhiteSharpDsp, default_params,
    latency_samples_for, midi_of,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_tracks_vocal_fundamentals_accurately() {
        let sr = 48_000.0f32;
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(60.0, 1_200.0);
        let target_hz = 220.0f32;
        let mut last = None;
        for n in 0..(sr as usize / 2) {
            let t = n as f32 / sr;
            let x = (1..6)
                .map(|k| (std::f32::consts::TAU * target_hz * k as f32 * t).sin() / k as f32)
                .sum::<f32>()
                * 0.3;
            if let Some(est) = detector.push(x, 0.15) {
                last = Some(est);
            }
        }
        let period = last.and_then(|e| e.period).expect("voiced period");
        let found_hz = sr / period;
        let cents = 1_200.0 * (found_hz / target_hz).log2();
        assert!(cents.abs() < 1.5, "expected ~220 Hz, got {found_hz} Hz ({cents} c)");
    }

    #[test]
    fn td_psola_shifter_shifts_pitch_by_ratio() {
        let sr = 48_000.0f32;
        let hz = 220.0f32;
        let ratio = 1.25f32;
        let period = sr / hz;
        let mut shifter = Shifter::new(sr, sr / 60.0);
        shifter.set_longest_period(sr / 60.0, 0);
        let mut out = Vec::with_capacity(sr as usize);
        for n in 0..(sr as usize) {
            if n % 192 == 0 {
                shifter.push_control(Control {
                    centre: n as u64,
                    period: Some(period),
                    ratio,
                    formant: 1.0,
                    gain: 1.0,
                });
            }
            let t = n as f32 / sr;
            let x = (1..5)
                .map(|k| (std::f32::consts::TAU * hz * k as f32 * t).sin() / k as f32)
                .sum::<f32>()
                * 0.3;
            let (wet, _) = shifter.process(x, x);
            out.push(wet[0]);
        }

        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(50.0, 1_500.0);
        let mut periods = Vec::new();
        for (n, &sample) in out.iter().enumerate() {
            if let Some(est) = detector.push(sample, 0.2) {
                if n > out.len() / 2 {
                    periods.extend(est.period);
                }
            }
        }
        periods.sort_by(f32::total_cmp);
        let shifted_hz = sr / periods[periods.len() / 2];
        let cents = 1_200.0 * (shifted_hz / (hz * ratio)).log2();
        assert!(cents.abs() < 15.0, "shifted_hz={shifted_hz}, cents={cents}");
    }

    #[test]
    fn voicetune_analyzes_and_renders_psola_offline() {
        let sr = 44_100u32;
        let mut samples = vec![0.0f32; sr as usize];
        // Generate 0.6s of A4 (440 Hz) in the middle
        for (i, sample) in samples.iter_mut().enumerate().take(35_000).skip(5_000) {
            let t = i as f32 / sr as f32;
            *sample = 0.5 * (std::f32::consts::TAU * 440.0 * t).sin();
        }
        let config = VoiceTuneAnalysisConfig::default();
        let mut doc = VoiceTuneAnalyzer::analyze_mono(&samples, sr, &config).expect("analyze");
        assert!(!doc.notes.is_empty());
        assert_eq!(doc.notes[0].detected_midi_note, 69);

        update_note_correction(&mut doc.notes[0], Some(72), Some(0.0), None, Some(2.0), Some(0.0));
        let plan = generate_render_plan(&doc.notes, "psola");
        assert_eq!(plan.note_edits.len(), 1);
        let rendered = OfflineVoiceRenderer::render_offline(&samples, sr, &plan).expect("render");
        assert_eq!(rendered.len(), samples.len());
    }

    #[test]
    fn canto_engine_loads_wav_analyzes_notes_and_renders_psola() {
        let sr = 48_000u32;
        // Build a 2.0-second WAV buffer with two sung notes: A3 (220 Hz) and C4 (261.63 Hz)
        let mut mono = vec![0.0f32; sr as usize * 2];
        for i in (sr as usize / 10)..(sr as usize * 9 / 10) {
            let t = i as f32 / sr as f32;
            mono[i] = 0.45 * (std::f32::consts::TAU * 220.0 * t).sin()
                + 0.15 * (std::f32::consts::TAU * 440.0 * t).sin();
        }
        for i in (sr as usize * 11 / 10)..(sr as usize * 19 / 10) {
            let t = i as f32 / sr as f32;
            mono[i] = 0.45 * (std::f32::consts::TAU * 261.63 * t).sin()
                + 0.15 * (std::f32::consts::TAU * 523.25 * t).sin();
        }
        let wav_bytes = encode_mono_wav_i16(&mono, sr);

        let mut engine = CantoEngine::new(sr as f32);
        let res = engine
            .load_audio_bytes(&wav_bytes, Some("wav"))
            .expect("decode and analyze WAV");
        assert!(res.ok);
        assert_eq!(res.notes.len(), 2, "expected 2 analyzed vocal notes");
        assert!((res.notes[0].detected - 57.0).abs() < 0.35, "expected ~A3 (57), got {}", res.notes[0].detected);
        assert!((res.notes[1].detected - 60.0).abs() < 0.35, "expected ~C4 (60), got {}", res.notes[1].detected);

        // Shift first note up by 3 semitones and render
        engine.update_note(0, 60.0, res.notes[0].start, res.notes[0].dur, 1.0, 0.0, 0.0, true);
        engine.playing = true;
        engine.seek(0.25);
        let mut buf = vec![0.0f32; 1024 * 2];
        let mut max_level = 0.0f32;
        for _ in 0..8 {
            let lvl = engine.render_stereo_f32(&mut buf);
            max_level = max_level.max(lvl);
        }
        assert!(max_level > 0.05, "expected audible TD-PSOLA vocal output, got {max_level}");
        assert!(buf.iter().any(|&s| s.abs() > 0.001));
    }
}
