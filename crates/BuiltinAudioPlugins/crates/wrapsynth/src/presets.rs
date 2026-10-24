//! WrapSynth's factory presets: one of each kind of sound the engine is built
//! for, every one starting from the init patch so a preset only says what
//! makes it itself.

use crate::{
    FilterType, LfoShape, ModDest, ModSlot, ModSource, Params, VoiceMode, Wavetable, default_params,
};

#[derive(Debug, Clone, Copy)]
pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn route(source: ModSource, dest: ModDest, amount: f32) -> ModSlot {
    ModSlot {
        source,
        dest,
        amount,
    }
}

fn with_routes(mut params: Params, routes: &[ModSlot]) -> Params {
    for (slot, route) in params.mod_slots.iter_mut().zip(routes) {
        *slot = *route;
    }
    params
}

/// The bank, `Init` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    let init = default_params();
    vec![
        FactoryPreset {
            name: "Init",
            params: init,
        },
        FactoryPreset {
            name: "Supersaw Lead",
            params: with_routes(
                Params {
                    unison: 7,
                    unison_detune_cents: 24.0,
                    unison_blend: 0.62,
                    stereo_width: 0.95,
                    osc_b_level: 0.55,
                    osc_b_semitones: 12.0,
                    osc_b_detune_cents: 4.0,
                    cutoff_hz: 7_800.0,
                    resonance: 0.12,
                    filter_env_amount: 0.12,
                    attack_ms: 2.0,
                    sustain: 0.9,
                    release_ms: 320.0,
                    glide_ms: 40.0,
                    voice_mode: VoiceMode::Legato,
                    lfo1_rate_hz: 5.5,
                    delay_on: true,
                    delay_mix: 0.18,
                    reverb_on: true,
                    reverb_mix: 0.16,
                    master_db: -2.5,
                    ..init
                },
                // A light vibrato, a tenth of a semitone.
                &[
                    route(ModSource::Lfo1, ModDest::OscAPitch, 0.004),
                    route(ModSource::Lfo1, ModDest::OscBPitch, 0.004),
                ],
            ),
        },
        FactoryPreset {
            name: "Warm Pad",
            params: with_routes(
                Params {
                    osc_a_wave: Wavetable::BasicShapes,
                    osc_a_position: 0.55,
                    osc_b_wave: Wavetable::ClassicTriangle,
                    osc_b_level: 0.5,
                    osc_b_semitones: -12.0,
                    unison: 5,
                    unison_detune_cents: 16.0,
                    stereo_width: 0.9,
                    cutoff_hz: 2_400.0,
                    resonance: 0.2,
                    filter_key_track: 0.35,
                    filter_env_amount: 0.15,
                    filter_attack_ms: 900.0,
                    filter_decay_ms: 2_000.0,
                    filter_sustain: 0.5,
                    attack_ms: 650.0,
                    decay_ms: 1_200.0,
                    sustain: 0.85,
                    release_ms: 1_800.0,
                    analog: 0.3,
                    lfo2_shape: LfoShape::Triangle,
                    lfo2_rate_hz: 0.18,
                    chorus_on: true,
                    chorus_mix: 0.35,
                    reverb_on: true,
                    reverb_size: 0.8,
                    reverb_decay_sec: 4.5,
                    reverb_mix: 0.32,
                    master_db: -4.0,
                    ..init
                },
                &[
                    route(ModSource::Lfo2, ModDest::OscAPosition, 0.25),
                    route(ModSource::Lfo2, ModDest::FilterCutoff, 0.06),
                ],
            ),
        },
        FactoryPreset {
            name: "Glass Pluck",
            params: Params {
                osc_a_wave: Wavetable::Harmonics,
                osc_a_position: 0.4,
                osc_b_wave: Wavetable::ClassicSine,
                osc_b_level: 0.4,
                osc_b_semitones: 19.0,
                unison: 2,
                unison_detune_cents: 6.0,
                cutoff_hz: 900.0,
                resonance: 0.3,
                filter_key_track: 0.7,
                filter_env_amount: 0.42,
                filter_attack_ms: 0.5,
                filter_decay_ms: 260.0,
                filter_sustain: 0.0,
                attack_ms: 0.5,
                decay_ms: 650.0,
                sustain: 0.0,
                release_ms: 500.0,
                delay_on: true,
                delay_feedback: 0.42,
                delay_mix: 0.24,
                reverb_on: true,
                reverb_mix: 0.2,
                master_db: -4.0,
                ..init
            },
        },
        FactoryPreset {
            name: "Sub Bass",
            params: Params {
                osc_a_wave: Wavetable::ClassicSine,
                osc_a_level: 0.9,
                osc_b_wave: Wavetable::ClassicTriangle,
                osc_b_level: 0.25,
                osc_b_detune_cents: 0.0,
                unison: 1,
                sub_level: 0.35,
                cutoff_hz: 600.0,
                resonance: 0.0,
                filter_key_track: 0.0,
                filter_env_amount: 0.1,
                filter_decay_ms: 180.0,
                attack_ms: 1.0,
                decay_ms: 400.0,
                sustain: 1.0,
                release_ms: 90.0,
                voice_mode: VoiceMode::Mono,
                glide_ms: 25.0,
                velocity_amount: 0.3,
                analog: 0.0,
                phase_random: 0.0,
                master_db: 0.0,
                ..init
            },
        },
        FactoryPreset {
            name: "Reese Bass",
            params: with_routes(
                Params {
                    osc_b_detune_cents: 22.0,
                    osc_b_level: 0.8,
                    osc_a_level: 0.8,
                    unison: 2,
                    unison_detune_cents: 18.0,
                    stereo_width: 0.35,
                    sub_level: 0.3,
                    filter_type: FilterType::LadderLowPass24,
                    cutoff_hz: 520.0,
                    resonance: 0.25,
                    filter_drive: 0.35,
                    filter_key_track: 0.2,
                    filter_env_amount: 0.12,
                    attack_ms: 2.0,
                    sustain: 1.0,
                    release_ms: 140.0,
                    voice_mode: VoiceMode::Legato,
                    glide_ms: 60.0,
                    lfo1_shape: LfoShape::Sine,
                    lfo1_rate_hz: 0.25,
                    lfo1_retrigger: false,
                    master_db: 3.0,
                    ..init
                },
                &[route(ModSource::Lfo1, ModDest::FilterCutoff, 0.08)],
            ),
        },
        FactoryPreset {
            name: "Wobble Bass",
            params: with_routes(
                Params {
                    osc_a_wave: Wavetable::HardSync,
                    osc_a_position: 0.3,
                    osc_b_wave: Wavetable::ClassicSquare,
                    osc_b_level: 0.5,
                    osc_b_semitones: -12.0,
                    unison: 3,
                    unison_detune_cents: 10.0,
                    sub_level: 0.4,
                    filter_type: FilterType::LadderLowPass24,
                    cutoff_hz: 380.0,
                    resonance: 0.55,
                    filter_drive: 0.45,
                    filter_key_track: 0.3,
                    filter_env_amount: 0.0,
                    attack_ms: 1.0,
                    sustain: 1.0,
                    release_ms: 120.0,
                    voice_mode: VoiceMode::Mono,
                    lfo1_shape: LfoShape::Sine,
                    lfo1_sync: true,
                    // A straight eighth.
                    lfo1_division: 7,
                    lfo1_retrigger: true,
                    dist_on: true,
                    dist_drive: 0.25,
                    dist_mix: 0.5,
                    master_db: -2.0,
                    ..init
                },
                &[
                    route(ModSource::Lfo1, ModDest::FilterCutoff, 0.42),
                    route(ModSource::Lfo1, ModDest::OscAPosition, 0.3),
                ],
            ),
        },
        FactoryPreset {
            name: "FM Bell",
            params: Params {
                osc_a_wave: Wavetable::Fm,
                osc_a_position: 0.45,
                osc_b_wave: Wavetable::ClassicSine,
                osc_b_level: 0.3,
                osc_b_semitones: 24.0,
                unison: 1,
                cutoff_hz: 12_000.0,
                filter_env_amount: 0.0,
                attack_ms: 0.5,
                decay_ms: 2_400.0,
                sustain: 0.0,
                release_ms: 2_200.0,
                velocity_amount: 0.85,
                reverb_on: true,
                reverb_size: 0.7,
                reverb_decay_sec: 3.5,
                reverb_mix: 0.3,
                master_db: -8.0,
                ..init
            },
        },
        FactoryPreset {
            name: "Vowel Choir",
            params: with_routes(
                Params {
                    osc_a_wave: Wavetable::Formant,
                    osc_a_position: 0.2,
                    osc_b_wave: Wavetable::ClassicSaw,
                    osc_b_level: 0.3,
                    unison: 5,
                    unison_detune_cents: 14.0,
                    stereo_width: 1.0,
                    filter_type: FilterType::FormantA,
                    cutoff_hz: 1_000.0,
                    resonance: 0.55,
                    filter_key_track: 0.0,
                    filter_env_amount: 0.0,
                    attack_ms: 420.0,
                    sustain: 0.9,
                    release_ms: 1_400.0,
                    analog: 0.35,
                    lfo1_rate_hz: 0.4,
                    lfo1_retrigger: false,
                    chorus_on: true,
                    reverb_on: true,
                    reverb_mix: 0.35,
                    master_db: 3.0,
                    ..init
                },
                &[route(ModSource::Lfo1, ModDest::OscAPosition, 0.35)],
            ),
        },
        FactoryPreset {
            name: "Brass Stab",
            params: Params {
                osc_b_semitones: 0.0,
                osc_b_detune_cents: 6.0,
                unison: 3,
                unison_detune_cents: 8.0,
                cutoff_hz: 900.0,
                resonance: 0.18,
                filter_key_track: 0.6,
                filter_env_amount: 0.38,
                filter_attack_ms: 40.0,
                filter_decay_ms: 380.0,
                filter_sustain: 0.45,
                attack_ms: 25.0,
                decay_ms: 300.0,
                sustain: 0.8,
                release_ms: 220.0,
                velocity_amount: 0.8,
                reverb_on: true,
                reverb_mix: 0.14,
                master_db: -4.0,
                ..init
            },
        },
        FactoryPreset {
            name: "Pulse Keys",
            params: with_routes(
                Params {
                    osc_a_wave: Wavetable::Pulse,
                    osc_a_position: 0.35,
                    osc_b_wave: Wavetable::Pulse,
                    osc_b_position: 0.6,
                    osc_b_level: 0.4,
                    osc_b_semitones: 12.0,
                    unison: 2,
                    unison_detune_cents: 7.0,
                    cutoff_hz: 3_200.0,
                    filter_env_amount: 0.15,
                    filter_decay_ms: 600.0,
                    attack_ms: 2.0,
                    decay_ms: 1_400.0,
                    sustain: 0.35,
                    release_ms: 400.0,
                    lfo1_rate_hz: 0.8,
                    lfo1_retrigger: false,
                    chorus_on: true,
                    chorus_mix: 0.3,
                    master_db: -7.0,
                    ..init
                },
                &[route(ModSource::Lfo1, ModDest::OscAPosition, 0.18)],
            ),
        },
        FactoryPreset {
            name: "Noise Sweep",
            params: with_routes(
                Params {
                    osc_a_level: 0.0,
                    osc_b_level: 0.0,
                    noise_level: 0.9,
                    filter_type: FilterType::BandPass12,
                    cutoff_hz: 300.0,
                    resonance: 0.7,
                    filter_key_track: 0.0,
                    filter_env_amount: 0.0,
                    attack_ms: 1_500.0,
                    sustain: 1.0,
                    release_ms: 2_500.0,
                    mod_attack_ms: 3_000.0,
                    mod_decay_ms: 4_000.0,
                    mod_sustain: 0.0,
                    reverb_on: true,
                    reverb_size: 0.9,
                    reverb_decay_sec: 6.0,
                    reverb_mix: 0.4,
                    master_db: 1.5,
                    ..init
                },
                &[route(ModSource::Env3, ModDest::FilterCutoff, 0.6)],
            ),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bank_starts_with_init_and_every_preset_is_in_range() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Init");
        assert!(bank.len() >= 10);
        for preset in &bank {
            let mut sanitized = preset.params;
            crate::ipc::sanitize_params(&mut sanitized);
            assert_eq!(sanitized, preset.params, "{} is out of range", preset.name);
        }
        let mut names: Vec<_> = bank.iter().map(|preset| preset.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), bank.len(), "names are unique");
    }
}
