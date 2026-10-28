//! `sphere_voxtune` — the Solfege Voxtune vocal engine, shared by every
//! front end: the Android app (`apps/mobile/solfege_voxtune`, through its
//! `canto_dsp` FFI crate), Futureboard Studio's built-in tuner panel, the
//! standalone desktop app and the VST3/ARA plug-in.
//!
//! Platform-neutral by design: no audio device, no UI, no FFI. Hosts own the
//! output stream and call [`shared::Shared::render_realtime`] (or render the
//! engine directly) from their audio callback.
//!
//! - `audio_io`: multi-format decoder (WAV/MP3/FLAC/OGG/M4A/AAC/AIFF) and cubic resampler
//! - `dsp_core`: denormal-safe filters, delay rings, allpass and soft-knee compressor
//! - `detect`: decimated + full-rate parabolic-refined YIN pitch detector
//! - `shifter`: stereo TD-PSOLA pitch and formant shifter
//! - `splice`: zero-lookahead live pitch-synchronous splicer
//! - `scale`: scale bitmasks and hysteresis-stabilized target resolution
//! - `whitesharp_dsp`: real-time pitch correction / vibrato / formant processor
//! - `voicetune`: offline YIN analysis, note segmentation and TD-PSOLA render plan
//! - `engine`: note analysis, editing state and the TD-PSOLA phrase renderer
//! - `load_job`: background decode + analysis with progress and cancel
//! - `shared`: the engine shared between a UI thread and an audio callback

pub mod audio_io;
pub mod detect;
pub mod dsp_core;
pub mod engine;
pub mod load_job;
pub mod scale;
pub mod shared;
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

    fn two_note_engine() -> (CantoEngine, CantoAnalysisResult) {
        let sr = 48_000u32;
        let mut mono = vec![0.0f32; sr as usize * 2];
        for (i, s) in mono.iter_mut().enumerate() {
            let t = i as f32 / sr as f32;
            let hz = if (0.1..0.9).contains(&t) {
                220.0
            } else if (1.1..1.9).contains(&t) {
                261.63
            } else {
                continue;
            };
            *s = 0.45 * (std::f32::consts::TAU * hz * t).sin();
        }
        let mut engine = CantoEngine::new(sr as f32);
        let res = engine
            .load_audio_bytes(&encode_mono_wav_i16(&mono, sr), Some("wav"))
            .expect("analyze");
        (engine, res)
    }

    fn render(engine: &mut CantoEngine, frames: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; frames * 2];
        engine.render_stereo_f32(&mut buf);
        buf
    }

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn note_preview_sounds_immediately_and_loops_while_held() {
        let (mut engine, res) = two_note_engine();
        engine.trigger_note_audition(0);
        // The shifter is pre-rolled: the note is heard within 1024 frames,
        // well inside its ~2200-frame latency that used to be dead air.
        // (The detected start sits ~10 ms before the tone in this fixture.)
        assert!(peak(&render(&mut engine, 1_024)) > 0.05);
        // Held past the note's length: it keeps sounding (loops).
        let note_frames = (res.notes[0].detected_dur * 48_000.0) as usize;
        render(&mut engine, note_frames + 4_800);
        assert!(engine.is_auditioning());
        assert!(peak(&render(&mut engine, 4_800)) > 0.05);
    }

    #[test]
    fn released_preview_finishes_the_note_then_stops() {
        let (mut engine, res) = two_note_engine();
        engine.trigger_note_audition(0);
        render(&mut engine, 2_400);
        engine.release_audition();
        let note_frames = (res.notes[0].detected_dur * 48_000.0) as usize;
        render(&mut engine, note_frames + 9_600);
        assert!(!engine.is_auditioning());
        assert_eq!(peak(&render(&mut engine, 512)), 0.0);
    }

    #[test]
    fn stopping_a_preview_fades_without_a_click() {
        let (mut engine, _) = two_note_engine();
        engine.trigger_note_audition(0);
        render(&mut engine, 9_600);
        engine.stop_audition();
        let tail = render(&mut engine, 1_024);
        let left: Vec<f32> = tail.iter().step_by(2).copied().collect();
        let max_step = left.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
        // A hard cut from a 0.45 sine would jump by ~0.45 in one sample.
        assert!(max_step < 0.1, "discontinuity {max_step}");
        assert!(!engine.is_auditioning());
        assert_eq!(peak(&left[600..]), 0.0);
    }

    #[test]
    fn offline_export_matches_length_and_aligns_with_source() {
        let (mut engine, _) = two_note_engine();
        let out = engine.render_offline();
        // Same length as the source (2 s at 48 kHz) despite shifter latency.
        assert_eq!(out.len(), 96_000);
        // Latency is compensated: the first note's onset (0.1 s) is silent
        // before ~0.09 s and audible right after 0.1 s.
        assert!(peak(&out[..4_000]) < 0.01);
        assert!(peak(&out[5_000..9_000]) > 0.1);
        // Rendering doesn't disturb the live engine.
        assert!(!engine.playing);
        assert_eq!(engine.playhead_sec, 0.0);
        // Edits are rendered: shifting note 0 up changes the export.
        let n0 = engine.notes[0].clone();
        engine.update_note(0, n0.pitch + 3.0, n0.start, n0.dur, 1.0, 0.0, n0.gain_db, true, false);
        let edited = engine.render_offline();
        let diff = out
            .iter()
            .zip(&edited)
            .take(40_000)
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(diff > 0.05, "edit not audible in export ({diff})");
    }

    #[test]
    fn muted_note_renders_silence_over_its_span() {
        let (mut engine, _) = two_note_engine();
        let n0 = engine.notes[0].clone();
        engine.update_note(0, n0.pitch, n0.start, n0.dur, 1.0, 0.0, n0.gain_db, false, true);
        let out = engine.render_offline();
        let span = |a: f32, b: f32| &out[(a * 48_000.0) as usize..(b * 48_000.0) as usize];
        // Inside the muted first note: silence. Second note still sounds.
        assert!(peak(span(0.2, 0.8)) < 1e-3);
        assert!(peak(span(1.2, 1.8)) > 0.1);
    }

    #[test]
    fn note_list_can_be_replaced_after_a_cut() {
        let (mut engine, _) = two_note_engine();
        let original = engine.render_offline();
        // Cut note 0 in half (as the UI does): halves map to their own part
        // of the source, so the audio is unchanged.
        let n = engine.notes[0].clone();
        let half = n.dur / 2.0;
        let mut a = n.clone();
        a.id = "n1a".into();
        a.dur = half;
        a.detected_dur = half;
        let mut b = n.clone();
        b.id = "n1b".into();
        b.start = n.start + half;
        b.detected_start = n.detected_start + half;
        b.dur = half;
        b.detected_dur = half;
        let rest = engine.notes[1].clone();
        let json = serde_json::to_string(&vec![a, b, rest]).unwrap();
        engine.set_notes(serde_json::from_str(&json).unwrap());
        assert_eq!(engine.notes.len(), 3);
        let cut = engine.render_offline();
        let diff = original
            .iter()
            .zip(&cut)
            .fold(0.0f32, |m, (x, y)| m.max((x - y).abs()));
        assert!(diff < 0.05, "cutting changed the audio by {diff}");
    }

    #[test]
    fn cached_analysis_restores_the_same_engine_state_and_renders_identically() {
        let (fresh, res) = two_note_engine();
        let json = serde_json::to_string(&res).unwrap();
        let decoded = decode_audio_bytes_to_mono(
            &encode_mono_wav_i16(&fresh.audio_samples, 48_000),
            Some("wav"),
            48_000,
        )
        .unwrap();
        let mut cached = CantoEngine::new(48_000.0);
        let back = cached
            .install_decoded_with_analysis(decoded.clone(), serde_json::from_str(&json).unwrap())
            .expect("adopt cached analysis");
        assert_eq!(back.notes, res.notes);
        assert_eq!(
            (cached.key, cached.scale, cached.bpm),
            (fresh.key, fresh.scale, fresh.bpm)
        );
        assert_eq!(fresh.render_offline(), cached.render_offline());

        // A cache entry for audio of another length is refused.
        let mut stale: CantoAnalysisResult = serde_json::from_str(&json).unwrap();
        stale.duration_sec += 1.0;
        assert!(
            cached
                .install_decoded_with_analysis(decoded, stale)
                .is_err()
        );
    }

    /// Detected pitch (MIDI) of `out` between `from` and `to` seconds.
    fn midi_track(out: &[f32], from: f32, to: f32) -> Vec<f32> {
        let sr = 48_000.0f32;
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(60.0, 1_200.0);
        let mut track = Vec::new();
        for (i, &x) in out.iter().enumerate() {
            let t = i as f32 / sr;
            if let Some(period) = detector.push(x, 0.15).and_then(|e| e.period) {
                if (from..to).contains(&t) {
                    track.push(69.0 + 12.0 * (sr / period / 440.0).log2());
                }
            }
        }
        track
    }

    fn spread(track: &[f32]) -> f32 {
        let lo = track.iter().cloned().fold(f32::MAX, f32::min);
        let hi = track.iter().cloned().fold(f32::MIN, f32::max);
        hi - lo
    }

    #[test]
    fn ornament_shapes_follow_their_style() {
        let (_, res) = two_note_engine();
        let mut n = res.notes[0].clone();
        n.orn_depth = 2.0;
        n.orn_speed = 0.5;
        let curve = |n: &CantoNote| {
            (0..=400)
                .map(|k| engine::ornament_semis(n, k as f32 / 400.0))
                .collect::<Vec<_>>()
        };
        n.orn_style = engine::ORN_TURN_UP;
        let up = curve(&n);
        assert!((up.iter().cloned().fold(0.0, f32::max) - 2.0).abs() < 0.05);
        assert_eq!(*up.last().unwrap(), 0.0, "back on the note");
        n.orn_style = engine::ORN_TURN_DOWN;
        assert!((curve(&n).iter().cloned().fold(0.0, f32::min) + 2.0).abs() < 0.05);
        n.orn_style = engine::ORN_FALL;
        let fall = curve(&n);
        assert_eq!(fall[80], 0.0);
        assert!((fall[400] + 2.0).abs() < 1e-3, "ends a full depth below");
        n.orn_style = engine::ORN_SHAKE;
        let shake = curve(&n);
        let peaks = shake
            .windows(3)
            .filter(|w| w[1] > w[0] && w[1] >= w[2] && w[1] > 1.0)
            .count();
        assert!(peaks >= 3, "a shake alternates several times, got {peaks}");
        // Bypass hears the original: no ornament, no accent.
        n.accent = 1.0;
        let shown = n.present(true);
        assert_eq!((shown.orn_style, shown.accent), (engine::ORN_NONE, 0.0));
    }

    #[test]
    fn shake_moves_the_rendered_pitch_and_accent_lifts_the_attack() {
        let (mut engine, res) = two_note_engine();
        let plain = engine.render_offline();
        assert!(spread(&midi_track(&plain, 0.45, 0.8)) < 0.5);

        engine.set_expression(0, engine::ORN_SHAKE, 2.0, 0.5, 0.0);
        let shaken = engine.render_offline();
        let track = midi_track(&shaken, 0.45, 0.8);
        assert!(spread(&track) > 1.2, "shake spread {}", spread(&track));

        let rms = |out: &[f32], from: f32| {
            let a = (from * 48_000.0) as usize;
            let seg = &out[a..a + 2_400];
            (seg.iter().map(|x| x * x).sum::<f32>() / seg.len() as f32).sqrt()
        };
        let onset = res.notes[0].start + 0.01;
        engine.set_expression(0, engine::ORN_NONE, 1.0, 0.5, 1.0);
        let accented = engine.render_offline();
        let lift = rms(&accented, onset) / rms(&plain, onset);
        assert!(lift > 1.15, "accent lift {lift}");
        // Past the attack the level is back to normal.
        let later = rms(&accented, 0.6) / rms(&plain, 0.6);
        assert!((later - 1.0).abs() < 0.05, "after attack {later}");
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
        engine.update_note(0, 60.0, res.notes[0].start, res.notes[0].dur, 1.0, 0.0, 0.0, true, false);
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
