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
//! - `backing`: the backing track played under the vocal for reference
//! - `dsp_core`: denormal-safe filters, delay rings, allpass and soft-knee compressor
//! - `detect`: decimated + full-rate parabolic-refined YIN pitch detector
//! - `shifter`: stereo TD-PSOLA pitch and formant shifter
//! - `splice`: zero-lookahead live pitch-synchronous splicer
//! - `scale`: scale bitmasks and hysteresis-stabilized target resolution
//! - `whitesharp_dsp`: real-time pitch correction / vibrato / formant processor
//! - `voicetune`: offline YIN analysis, note segmentation and TD-PSOLA render plan
//! - `engine`: note analysis, editing state and the TD-PSOLA phrase renderer
//! - `edit`: note edit operations (cut, join, macros) shared by the editors
//! - `session`: the edited document, undo history and engine sync
//! - `load_job`: background decode + analysis with progress and cancel
//! - `store`: per-file disk cache of analyses and edits
//! - `shared`: the engine shared between a UI thread and an audio callback

pub mod audio_io;
pub mod backing;
pub mod detect;
pub mod dsp_core;
pub mod edit;
pub mod engine;
pub mod load_job;
pub mod scale;
pub mod session;
pub mod shared;
pub mod shifter;
pub mod splice;
pub mod store;
pub mod stretch;
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
        assert!(
            cents.abs() < 1.5,
            "expected ~220 Hz, got {found_hz} Hz ({cents} c)"
        );
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
            if let Some(est) = detector.push(sample, 0.2)
                && n > out.len() / 2
            {
                periods.extend(est.period);
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

        update_note_correction(
            &mut doc.notes[0],
            Some(72),
            Some(0.0),
            None,
            Some(2.0),
            Some(0.0),
        );
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
        let max_step = left
            .windows(2)
            .fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
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
        engine.update_note(
            0,
            n0.pitch + 3.0,
            n0.start,
            n0.dur,
            1.0,
            0.0,
            n0.gain_db,
            true,
            false,
        );
        let edited = engine.render_offline();
        let diff = out
            .iter()
            .zip(&edited)
            .take(40_000)
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(diff > 0.05, "edit not audible in export ({diff})");
    }

    fn median_hz(signal: &[f32]) -> f32 {
        let mut detector = Detector::new(48_000.0, 30.0);
        detector.set_range(50.0, 1_500.0);
        let mut periods: Vec<f32> = signal
            .iter()
            .filter_map(|&s| detector.push(s, 0.2))
            .filter_map(|e| e.period)
            .collect();
        periods.sort_by(f32::total_cmp);
        48_000.0 / periods[periods.len() / 2]
    }

    #[test]
    fn a_lengthened_or_shortened_note_keeps_its_pitch() {
        let (mut engine, _) = two_note_engine();
        let n0 = engine.notes[0].clone();
        let n1 = engine.notes[1].clone();
        // Note 0 (220 Hz) squeezed to 60 %; note 1 (C4) drawn out to fill
        // the gap before it and the rest of the file.
        engine.update_note(
            0,
            n0.pitch,
            n0.start,
            n0.dur * 0.6,
            1.0,
            0.0,
            n0.gain_db,
            false,
            false,
        );
        let start = n0.start + n0.dur * 0.6 + 0.05;
        engine.update_note(
            1,
            n1.pitch,
            start,
            1.95 - start,
            1.0,
            0.0,
            n1.gain_db,
            false,
            false,
        );
        let out = engine.render_offline();
        let span = |a: f32, b: f32| &out[(a * 48_000.0) as usize..(b * 48_000.0) as usize];
        let cents = |hz: f32, want: f32| 1_200.0 * (hz / want).log2();
        let first = median_hz(span(n0.start + 0.05, n0.start + n0.dur * 0.55));
        assert!(
            cents(first, 220.0).abs() < 15.0,
            "squeezed note at {first} Hz"
        );
        let second = median_hz(span(start + 0.1, 1.85));
        assert!(
            cents(second, 261.63).abs() < 15.0,
            "stretched note at {second} Hz"
        );
        // Still tunable: +2 st on the stretched note lands on D4.
        engine.update_note(
            1,
            n1.pitch + 2.0,
            start,
            1.95 - start,
            1.0,
            0.0,
            n1.gain_db,
            true,
            false,
        );
        let out = engine.render_offline();
        let tuned =
            median_hz(&out[((start + 0.1) * 48_000.0) as usize..(1.85 * 48_000.0) as usize]);
        assert!(
            cents(tuned, 293.66).abs() < 20.0,
            "tuned stretched note at {tuned} Hz"
        );
    }

    /// A legato phrase: G3, A3, B3 sung without breaks (gliding between).
    fn legato_engine() -> CantoEngine {
        let sr = 48_000u32;
        let mut mono = vec![0.0f32; sr as usize * 3];
        let mut phase = 0.0f32;
        for (i, s) in mono.iter_mut().enumerate() {
            let t = i as f32 / sr as f32;
            if !(0.2..2.6).contains(&t) {
                continue;
            }
            let step = |a: f32| ((t - a) / 0.04).clamp(0.0, 1.0);
            let midi = 55.0
                + 2.0 * step(1.0)
                + 2.0 * step(1.8)
                + 0.35 * (std::f32::consts::TAU * 5.3 * t).sin();
            phase = (phase + 440.0 * 2f32.powf((midi - 69.0) / 12.0) / sr as f32).fract();
            let env = ((t - 0.2) / 0.03).min((2.6 - t) / 0.05).clamp(0.0, 1.0);
            *s = env
                * (1..12)
                    .map(|k| (std::f32::consts::TAU * k as f32 * phase).sin() / k as f32)
                    .sum::<f32>()
                * 0.25;
        }
        let mut engine = CantoEngine::new(sr as f32);
        engine
            .load_audio_bytes(&encode_mono_wav_i16(&mono, sr), Some("wav"))
            .expect("analyze");
        engine
    }

    /// Largest one-sample jump in `x` relative to the local level, and the
    /// longest silent run (ms) between `a` and `b` seconds.
    fn joins(x: &[f32], a: f32, b: f32) -> (f32, f32) {
        let (a, b) = ((a * 48_000.0) as usize, (b * 48_000.0) as usize);
        let mut worst = 0.0f32;
        let (mut run, mut longest) = (0usize, 0usize);
        for i in a.max(241)..b {
            let rms = (x[i - 240..i].iter().map(|s| s * s).sum::<f32>() / 240.0).sqrt();
            let d2 = (x[i] - 2.0 * x[i - 1] + x[i - 2]).abs();
            worst = worst.max(d2 / rms.max(0.02));
            if x[i].abs() < 1e-3 {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        (worst, longest as f32 / 48.0)
    }

    #[test]
    fn resized_legato_notes_join_without_a_catch() {
        let mut engine = legato_engine();
        assert_eq!(engine.notes.len(), 3);
        let (sung, _) = joins(&engine.render_offline(), 0.3, 2.5);
        // Squeeze the middle note on its own: a hole opens, but it fades
        // out and back in instead of cutting.
        let n = engine.notes[1].clone();
        engine.update_note(
            1,
            n.pitch,
            n.start,
            n.dur * 0.7,
            1.0,
            0.0,
            n.gain_db,
            false,
            false,
        );
        let (cut, hole_ms) = joins(&engine.render_offline(), 0.3, 2.5);
        assert!(hole_ms > 100.0);
        assert!(cut < 0.2, "hard cut into the hole: {cut}");
        // As the editor does for legato notes, the next note takes the time:
        // no hole, and the joins are as smooth as sung.
        let m = engine.notes[2].clone();
        let gap = m.start - (n.start + n.dur);
        let start = n.start + n.dur * 0.7 + gap;
        let end = m.start + m.dur;
        engine.update_note(
            2,
            m.pitch,
            start,
            end - start,
            1.0,
            0.0,
            m.gain_db,
            false,
            false,
        );
        let (linked, silent_ms) = joins(&engine.render_offline(), 0.3, 2.5);
        assert!(silent_ms < 1.0, "{silent_ms} ms of silence");
        assert!(linked < sung * 1.5, "join {linked} vs sung {sung}");
    }

    #[test]
    fn a_stretched_note_previews_at_its_new_length() {
        let (mut engine, _) = two_note_engine();
        let n0 = engine.notes[0].clone();
        engine.update_note(
            0,
            n0.pitch,
            n0.start,
            n0.dur * 1.2,
            1.0,
            0.0,
            n0.gain_db,
            false,
            false,
        );
        engine.trigger_note_audition(0);
        engine.release_audition();
        // Past the old length it is still playing, at the old pitch.
        let frames = (n0.dur * 1.1 * 48_000.0) as usize;
        let heard = render(&mut engine, frames);
        assert!(engine.is_auditioning());
        let left: Vec<f32> = heard.iter().step_by(2).copied().collect();
        let hz = median_hz(&left[frames / 4..]);
        assert!(
            (1_200.0 * (hz / 220.0).log2()).abs() < 15.0,
            "preview at {hz} Hz"
        );
    }

    #[test]
    fn muted_note_renders_silence_over_its_span() {
        let (mut engine, _) = two_note_engine();
        let n0 = engine.notes[0].clone();
        engine.update_note(
            0, n0.pitch, n0.start, n0.dur, 1.0, 0.0, n0.gain_db, false, true,
        );
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
            if let Some(period) = detector.push(x, 0.15).and_then(|e| e.period)
                && (from..to).contains(&t)
            {
                track.push(69.0 + 12.0 * (sr / period / 440.0).log2());
            }
        }
        track
    }

    fn spread(track: &[f32]) -> f32 {
        let lo = track.iter().cloned().fold(f32::MAX, f32::min);
        let hi = track.iter().cloned().fold(f32::MIN, f32::max);
        hi - lo
    }

    /// A sung-like tone: ten harmonics, a scoop into the note, vibrato of
    /// ±`vib_cents` at 5.5 Hz, and a fall off the end. `shift_cents` moves
    /// the whole pitch curve (the ideal output of a pitch shift).
    fn sung_tone(shift_cents: f32, vib_cents: f32, bends: bool) -> Vec<f32> {
        let sr = 48_000.0f32;
        let (start, end) = (0.2f32, 2.6f32);
        let mut out = vec![0.0f32; (2.8 * sr) as usize];
        let mut phase = 0.0f32;
        for (i, s) in out.iter_mut().enumerate() {
            let t = i as f32 / sr;
            if !(start..end).contains(&t) {
                continue;
            }
            let into = t - start;
            let left = end - t;
            let mut cents = shift_cents;
            if bends && into < 0.15 {
                cents -= 120.0 * (1.0 - into / 0.15);
            }
            if bends && left < 0.15 {
                cents -= 80.0 * (1.0 - left / 0.15);
            }
            if into > 0.35 {
                let depth = vib_cents * ((into - 0.35) / 0.25).min(1.0);
                cents += depth * (std::f32::consts::TAU * 5.5 * into).sin();
            }
            let hz = 220.0 * 2f32.powf(cents / 1_200.0);
            phase += std::f32::consts::TAU * hz / sr;
            let amp = (into / 0.03).min(1.0) * (left / 0.05).min(1.0) * 0.3;
            let mut v = 0.0;
            for k in 1..=10 {
                v += (phase * k as f32).sin() / k as f32;
            }
            *s = amp * v;
        }
        out
    }

    /// The true pitch (MIDI) of `sung_tone` at `t`.
    fn sung_truth(t: f32, shift_cents: f32, vib_cents: f32, bends: bool) -> f32 {
        let into = t - 0.2;
        let left = 2.6 - t;
        let mut c = shift_cents;
        if bends && into < 0.15 {
            c -= 120.0 * (1.0 - into / 0.15);
        }
        if bends && left < 0.15 {
            c -= 80.0 * (1.0 - left / 0.15);
        }
        if into > 0.35 {
            let depth = vib_cents * ((into - 0.35) / 0.25).min(1.0);
            c += depth * (std::f32::consts::TAU * 5.5 * into).sin();
        }
        57.0 + c / 100.0
    }

    /// Pitch of `x` at `t` (MIDI): autocorrelation peak within ±12 % of
    /// the expected period over a 20 ms window, parabola-refined.
    fn acf_pitch(x: &[f32], t: f32, expect_midi: f32) -> f32 {
        let sr = 48_000.0f32;
        let p = sr / (440.0 * 2f32.powf((expect_midi - 69.0) / 12.0));
        let c = (t * sr) as isize;
        let half = (0.010 * sr) as isize;
        let (lo, hi) = ((p * 0.88) as isize, (p * 1.12) as isize + 1);
        let score = |lag: isize| {
            let (mut d, mut a, mut b) = (0.0f32, 0.0f32, 0.0f32);
            for k in -half..half {
                let u = x[(c + k - lag / 2) as usize];
                let v = x[(c + k + lag - lag / 2) as usize];
                d += u * v;
                a += u * u;
                b += v * v;
            }
            d / (a * b).sqrt().max(1e-9)
        };
        let mut best = (lo, f32::MIN);
        for lag in lo..=hi {
            let s = score(lag);
            if s > best.1 {
                best = (lag, s);
            }
        }
        let (l, m, r) = (score(best.0 - 1), best.1, score(best.0 + 1));
        let den = l - 2.0 * m + r;
        let off = if den.abs() > 1e-9 {
            (0.5 * (l - r) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let period = best.0 as f32 + off;
        69.0 + 12.0 * (sr / period / 440.0).log2()
    }

    /// Renders the vibrato tone with every note set to `pitch(note)` and
    /// Drift `modulation`, and returns the output's pitch error against
    /// `truth(t)` (cents: median, p95) through the held part of the note.
    fn tuning_error(
        pitch: impl Fn(&CantoNote) -> f32,
        modulation: f32,
        truth: impl Fn(f32) -> f32,
    ) -> (f32, f32) {
        let sr = 48_000u32;
        let mut engine = CantoEngine::new(sr as f32);
        engine
            .load_audio_bytes(
                &encode_mono_wav_i16(&sung_tone(0.0, 70.0, true), sr),
                Some("wav"),
            )
            .expect("analyze");
        for i in 0..engine.notes.len() {
            let n = engine.notes[i].clone();
            engine.update_note(
                i,
                pitch(&n),
                n.start,
                n.dur,
                modulation,
                0.0,
                n.gain_db,
                true,
                false,
            );
        }
        let out = engine.render_offline();
        let mut errors = Vec::new();
        let mut t = 0.6f32;
        while t < 2.3 {
            let want = truth(t);
            errors.push(((acf_pitch(&out, t, want) - want) * 100.0).abs());
            t += 0.005;
        }
        errors.sort_by(f32::total_cmp);
        (errors[errors.len() / 2], errors[errors.len() * 95 / 100])
    }

    #[test]
    fn tuning_follows_the_pitch_actually_sung() {
        // Vibrato of ±70 cents is wider than a note's contour can hold; the
        // render follows the sung pitch track, so a flattened note is flat…
        let (median, p95) = tuning_error(|_| 57.0, 0.0, |_| 57.0);
        assert!(
            median < 6.0 && p95 < 12.0,
            "flattened: median {median} c, p95 {p95} c"
        );
        // …and a retuned one keeps the singer's own movement, shifted.
        let (median, p95) =
            tuning_error(|n| n.pitch + 3.0, 1.0, |t| sung_truth(t, 300.0, 70.0, true));
        assert!(
            median < 4.0 && p95 < 8.0,
            "+3 st: median {median} c, p95 {p95} c"
        );
    }

    #[test]
    fn a_cached_analysis_keeps_the_sung_pitch() {
        let sr = 48_000u32;
        let mut engine = CantoEngine::new(sr as f32);
        let res = engine
            .load_audio_bytes(
                &encode_mono_wav_i16(&sung_tone(0.0, 70.0, true), sr),
                Some("wav"),
            )
            .expect("analyze");
        let at = |t: f32| res.pitch.at(t).expect("voiced");
        assert!((at(1.0) - sung_truth(1.0, 0.0, 70.0, true)).abs() < 0.1);
        assert!(res.pitch.at(0.05).is_none(), "silence has no pitch");
        let json = serde_json::to_string(&res).unwrap();
        let back: CantoAnalysisResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pitch, res.pitch);
        // An entry cached before the track existed still loads.
        let old = json.replace(
            &format!(",\"pitch\":{}", serde_json::to_string(&res.pitch).unwrap()),
            "",
        );
        let back: CantoAnalysisResult = serde_json::from_str(&old).unwrap();
        assert!(back.pitch.midi.is_empty());
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
        for (i, sample) in mono
            .iter_mut()
            .enumerate()
            .take(sr as usize * 9 / 10)
            .skip(sr as usize / 10)
        {
            let t = i as f32 / sr as f32;
            *sample = 0.45 * (std::f32::consts::TAU * 220.0 * t).sin()
                + 0.15 * (std::f32::consts::TAU * 440.0 * t).sin();
        }
        for (i, sample) in mono
            .iter_mut()
            .enumerate()
            .take(sr as usize * 19 / 10)
            .skip(sr as usize * 11 / 10)
        {
            let t = i as f32 / sr as f32;
            *sample = 0.45 * (std::f32::consts::TAU * 261.63 * t).sin()
                + 0.15 * (std::f32::consts::TAU * 523.25 * t).sin();
        }
        let wav_bytes = encode_mono_wav_i16(&mono, sr);

        let mut engine = CantoEngine::new(sr as f32);
        let res = engine
            .load_audio_bytes(&wav_bytes, Some("wav"))
            .expect("decode and analyze WAV");
        assert!(res.ok);
        assert_eq!(res.notes.len(), 2, "expected 2 analyzed vocal notes");
        assert!(
            (res.notes[0].detected - 57.0).abs() < 0.35,
            "expected ~A3 (57), got {}",
            res.notes[0].detected
        );
        assert!(
            (res.notes[1].detected - 60.0).abs() < 0.35,
            "expected ~C4 (60), got {}",
            res.notes[1].detected
        );

        // Shift first note up by 3 semitones and render
        engine.update_note(
            0,
            60.0,
            res.notes[0].start,
            res.notes[0].dur,
            1.0,
            0.0,
            0.0,
            true,
            false,
        );
        engine.playing = true;
        engine.seek(0.25);
        let mut buf = vec![0.0f32; 1024 * 2];
        let mut max_level = 0.0f32;
        for _ in 0..8 {
            let lvl = engine.render_stereo_f32(&mut buf);
            max_level = max_level.max(lvl);
        }
        assert!(
            max_level > 0.05,
            "expected audible TD-PSOLA vocal output, got {max_level}"
        );
        assert!(buf.iter().any(|&s| s.abs() > 0.001));
    }
}
