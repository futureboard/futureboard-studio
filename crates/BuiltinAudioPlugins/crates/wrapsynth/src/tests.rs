//! The synth as a whole: notes, voices, MIDI timing, and what it sounds like
//! measured — aliasing, clicks, pitch.

use super::*;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

const RATE: f32 = 48_000.0;

/// One oscillator, no unison, no effects, the filter wide open: what the
/// oscillator itself plays.
fn bare(wave: Wavetable) -> Params {
    Params {
        osc_a_wave: wave,
        osc_a_position: 0.0,
        osc_a_level: 1.0,
        osc_b_level: 0.0,
        sub_level: 0.0,
        noise_level: 0.0,
        unison: 1,
        filter_type: FilterType::LowPass12,
        cutoff_hz: CUTOFF_MAX_HZ,
        resonance: 0.0,
        filter_drive: 0.0,
        filter_key_track: 0.0,
        filter_env_amount: 0.0,
        attack_ms: 0.5,
        sustain: 1.0,
        velocity_amount: 0.0,
        analog: 0.0,
        phase_random: 0.0,
        master_db: 0.0,
        ..default_params()
    }
}

fn synth(params: Params) -> Dsp {
    let mut dsp = Dsp::new(RATE);
    dsp.set_params(params);
    dsp
}

fn render(dsp: &mut Dsp, frames: usize) -> Vec<(f32, f32)> {
    let mut out = vec![0.0; frames * 2];
    dsp.process_block(&mut out, frames);
    out.chunks_exact(2)
        .map(|frame| (frame[0], frame[1]))
        .collect()
}

fn left(frames: &[(f32, f32)]) -> Vec<f32> {
    frames.iter().map(|frame| frame.0).collect()
}

fn biggest_step(samples: &[f32]) -> f32 {
    samples
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn a_note_sounds_and_stays_finite_and_under_full_scale() {
    let mut dsp = Dsp::new(RATE);
    dsp.note_on(60, 110);
    let frames = render(&mut dsp, 8_000);
    let peak = frames
        .iter()
        .inspect(|(l, r)| assert!(l.is_finite() && r.is_finite()))
        .map(|(l, r)| l.abs().max(r.abs()))
        .fold(0.0, f32::max);
    assert!(peak > 0.02 && peak < 0.9, "peak {peak}");
}

#[test]
fn a_released_note_reaches_silence_and_frees_its_voice() {
    let mut dsp = synth(Params {
        release_ms: 20.0,
        ..default_params()
    });
    dsp.note_on(64, 100);
    render(&mut dsp, 2_000);
    dsp.note_off(64);
    render(&mut dsp, 6_000);
    assert_eq!(dsp.active_voices(), 0);
    let tail = render(&mut dsp, 64);
    assert!(
        tail.iter()
            .all(|(l, r)| l.abs() < 1.0e-4 && r.abs() < 1.0e-4)
    );
}

/// The saw that used to be drawn sample by sample folded its harmonics back
/// below Nyquist as inharmonic noise. Measured now: nearly every bit of a
/// high saw's energy sits on its own harmonics.
#[test]
fn a_high_saw_does_not_alias() {
    let mut dsp = synth(bare(Wavetable::ClassicSaw));
    let note = 100u8; // 2.64 kHz
    dsp.note_on(note, 127);
    render(&mut dsp, 4_096);
    const N: usize = 8_192;
    let samples = left(&render(&mut dsp, N));
    let mut bins: Vec<Complex<f32>> = samples
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let window = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos();
            Complex::new(x * window, 0.0)
        })
        .collect();
    FftPlanner::new().plan_fft_forward(N).process(&mut bins);
    let fundamental = hz_for(f32::from(note));
    let bin_hz = RATE / N as f32;
    let (mut harmonic, mut other) = (0.0f64, 0.0f64);
    for (k, bin) in bins.iter().enumerate().take(N / 2).skip(4) {
        let hz = k as f32 * bin_hz;
        let nearest = (hz / fundamental).round().max(1.0) * fundamental;
        let power = f64::from(bin.norm_sqr());
        if (hz - nearest).abs() <= 4.0 * bin_hz {
            harmonic += power;
        } else {
            other += power;
        }
    }
    let ratio_db = 10.0 * (other / harmonic).log10();
    assert!(ratio_db < -45.0, "off-harmonic energy {ratio_db:.1} dB");
}

#[test]
fn pitch_bend_moves_the_note_by_its_range() {
    let mut dsp = synth(Params {
        pitch_bend_range: 2.0,
        ..bare(Wavetable::ClassicSine)
    });
    dsp.midi(0xe0, 0x7f, 0x7f); // full up
    dsp.note_on(69, 127);
    render(&mut dsp, 4_800);
    let samples = left(&render(&mut dsp, RATE as usize));
    let crossings = samples
        .windows(2)
        .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
        .count() as f32;
    // A whole tone above A4, give or take the bend's last step.
    let expected = hz_for(71.0);
    assert!(
        (crossings - expected).abs() < 2.0,
        "{crossings} Hz, expected {expected}"
    );
}

#[test]
fn a_note_starts_on_its_sample() {
    let mut dsp = synth(bare(Wavetable::ClassicSaw));
    dsp.queue_midi(100, 0x90, 60, 127);
    let samples = left(&render(&mut dsp, 256));
    assert!(samples[..100].iter().all(|x| *x == 0.0), "silent before");
    assert!(
        samples[100..140].iter().any(|x| x.abs() > 1.0e-3),
        "sounding after"
    );
}

#[test]
fn retriggering_a_sounding_note_does_not_click() {
    let mut dsp = synth(Params {
        release_ms: 400.0,
        ..bare(Wavetable::ClassicSine)
    });
    dsp.note_on(57, 120);
    let before = left(&render(&mut dsp, 4_800));
    dsp.note_off(57);
    render(&mut dsp, 480);
    dsp.note_on(57, 120);
    let after = left(&render(&mut dsp, 480));
    let steady = biggest_step(&before[2_400..]);
    assert!(
        biggest_step(&after) <= steady * 1.5,
        "retrigger step {} vs steady {steady}",
        biggest_step(&after)
    );
    assert_eq!(dsp.active_voices(), 1, "the same voice plays it again");
}

#[test]
fn stealing_a_voice_fades_it_instead_of_cutting_it() {
    let mut dsp = synth(Params {
        sustain: 1.0,
        release_ms: 2_000.0,
        ..bare(Wavetable::ClassicSine)
    });
    for note in 40..(40 + MAX_VOICES as u8) {
        dsp.note_on(note, 100);
    }
    let held = left(&render(&mut dsp, 4_800));
    // Below every held note: its own slope cannot pass for a click.
    dsp.note_on(39, 100);
    let stolen = left(&render(&mut dsp, 960));
    let steady = biggest_step(&held[2_400..]);
    assert!(
        biggest_step(&stolen) <= steady * 1.5,
        "steal step {} vs steady {steady}",
        biggest_step(&stolen)
    );
    assert_eq!(dsp.active_voices(), MAX_VOICES);
    assert!(
        dsp.voices
            .iter()
            .any(|voice| voice.active && voice.note == 39)
    );
}

#[test]
fn voice_stealing_stays_bounded() {
    let mut dsp = Dsp::new(RATE);
    for note in 24..100 {
        dsp.note_on(note, 90);
    }
    assert_eq!(dsp.active_voices(), MAX_VOICES);
    render(&mut dsp, 2_000);
    assert_eq!(dsp.active_voices(), MAX_VOICES);
}

/// Every voice busy (the last chord still releasing), then a whole chord in
/// one block: every note of it sounds, instead of each new note stealing
/// the voice the previous one had just started.
#[test]
fn a_chord_on_a_full_synth_keeps_every_note() {
    let mut dsp = Dsp::new(RATE);
    for note in 40..(40 + MAX_VOICES as u8) {
        dsp.note_on(note, 90);
    }
    render(&mut dsp, 200);
    for note in 40..48 {
        dsp.note_off(note);
    }
    let chord = [60u8, 64, 67, 71, 74];
    for &note in &chord {
        dsp.note_on(note, 100);
    }
    // Past the steal fade: every chord note has its voice.
    render(&mut dsp, 480);
    for note in chord {
        assert!(
            dsp.voices
                .iter()
                .any(|voice| voice.active && !voice.fading && voice.note == note),
            "note {note} of the chord was lost"
        );
    }
}

#[test]
fn legato_glides_on_one_voice() {
    let mut dsp = synth(Params {
        voice_mode: VoiceMode::Legato,
        glide_ms: 100.0,
        ..bare(Wavetable::ClassicSaw)
    });
    dsp.note_on(60, 100);
    render(&mut dsp, 2_400);
    let attack_before = dsp.voices[0].amp.stage;
    dsp.note_on(72, 100);
    render(&mut dsp, 480);
    let voice = dsp.voices[0];
    assert_eq!(dsp.active_voices(), 1);
    assert!(voice.pitch > 60.0 && voice.pitch < 72.0, "{}", voice.pitch);
    assert_eq!(voice.amp.stage, attack_before, "legato does not retrigger");
    render(&mut dsp, 24_000);
    assert!((dsp.voices[0].pitch - 72.0).abs() < 0.01);
    // Letting go of the top key goes back to the one still held.
    dsp.note_off(72);
    render(&mut dsp, 24_000);
    assert!((dsp.voices[0].pitch - 60.0).abs() < 0.01);
    assert!(dsp.voices[0].gate);
}

#[test]
fn the_sustain_pedal_holds_released_notes() {
    let mut dsp = synth(Params {
        release_ms: 20.0,
        ..default_params()
    });
    dsp.midi(0xb0, 64, 127);
    dsp.note_on(60, 100);
    render(&mut dsp, 480);
    dsp.note_off(60);
    render(&mut dsp, 9_600);
    assert_eq!(dsp.active_voices(), 1, "held by the pedal");
    dsp.midi(0xb0, 64, 0);
    render(&mut dsp, 9_600);
    assert_eq!(dsp.active_voices(), 0);
}

#[test]
fn switching_off_fades_rather_than_cuts() {
    let mut dsp = synth(bare(Wavetable::ClassicSine));
    dsp.note_on(57, 127);
    let before = left(&render(&mut dsp, 4_800));
    let mut off = *dsp.params();
    off.power = false;
    dsp.set_params(off);
    let fading = left(&render(&mut dsp, 960));
    assert!(biggest_step(&fading) <= biggest_step(&before[2_400..]) * 1.5);
    assert!(fading[900..].iter().all(|x| *x == 0.0));
    assert_eq!(dsp.active_voices(), 0);
}

#[test]
fn a_loud_hot_patch_never_passes_full_scale() {
    let mut dsp = synth(Params {
        unison: 7,
        osc_b_level: 1.0,
        sub_level: 1.0,
        filter_drive: 1.0,
        resonance: 0.95,
        master_db: 3.0,
        velocity_amount: 0.0,
        ..default_params()
    });
    for note in 36..(36 + MAX_VOICES as u8) {
        dsp.note_on(note, 127);
    }
    for (l, r) in render(&mut dsp, 24_000) {
        assert!(l.abs() <= 1.0 && r.abs() <= 1.0);
    }
}

#[test]
fn every_factory_preset_plays_cleanly() {
    for preset in factory_presets() {
        let mut dsp = synth(preset.params);
        dsp.note_on(48, 100);
        dsp.note_on(55, 100);
        dsp.note_on(64, 100);
        let frames = render(&mut dsp, 24_000);
        let peak = frames
            .iter()
            .map(|(l, r)| {
                assert!(l.is_finite() && r.is_finite(), "{}", preset.name);
                l.abs().max(r.abs())
            })
            .fold(0.0, f32::max);
        assert!(peak > 1.0e-3, "{} is silent", preset.name);
        assert!(peak <= 1.0, "{} clips", preset.name);
    }
}

#[test]
fn a_first_version_project_still_plays() {
    let json = r#"{"version":1,"params":{"power":true,"oscAWave":"saw","oscAPosition":0.18,
        "oscALevel":0.78,"oscBWave":"square","oscBPosition":0.42,"oscBLevel":0.38,
        "oscBSemitones":0.0,"oscBDetuneCents":7.0,"unison":3,"unisonDetuneCents":14.0,
        "stereoWidth":0.72,"subLevel":0.16,"noiseLevel":0.025,"cutoffHz":6400.0,
        "resonance":0.18,"filterDrive":0.12,"attackMs":8.0,"decayMs":220.0,"sustain":0.72,
        "releaseMs":420.0,"masterDb":-8.0}}"#;
    let state = ipc::WrapSynthState::from_json(json).unwrap();
    let mut dsp = synth(state.params);
    dsp.note_on(60, 100);
    let peak = render(&mut dsp, 9_600)
        .iter()
        .map(|(l, r)| l.abs().max(r.abs()))
        .fold(0.0, f32::max);
    assert!(peak > 0.02 && peak <= 1.0, "{peak}");
}

#[test]
fn an_lfo_on_the_cutoff_moves_the_sound() {
    let plain = bare(Wavetable::ClassicSaw);
    let mut wobble = plain;
    wobble.cutoff_hz = 600.0;
    wobble.lfo1_rate_hz = 4.0;
    wobble.mod_slots[0] = ModSlot {
        source: ModSource::Lfo1,
        dest: ModDest::FilterCutoff,
        amount: 0.4,
    };
    let mut dsp = synth(wobble);
    dsp.note_on(48, 127);
    let samples = left(&render(&mut dsp, RATE as usize));
    // RMS per 10 ms window rises and falls with the LFO.
    let windows: Vec<f32> = samples
        .chunks(480)
        .map(|chunk| (chunk.iter().map(|x| x * x).sum::<f32>() / chunk.len() as f32).sqrt())
        .collect();
    let (low, high) = windows[10..]
        .iter()
        .fold((f32::MAX, 0.0f32), |(lo, hi), &w| (lo.min(w), hi.max(w)));
    assert!(high > low * 1.5, "{low} .. {high}");
}
