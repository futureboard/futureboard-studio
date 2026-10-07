//! EchoSpace's factory presets, ported from the retired Svelte editor and
//! extended with the rebuilt engine's wow, ducking and diffusion.
//!
//! Each is a whole `Params` (power on, freeze off); an editor loads one by
//! sending the wire values that differ, `link` first — while it is on the
//! DSP mirrors one side's edit onto the other, so a preset that turns it off
//! has to say so before its two sides arrive.

use crate::{DelayMode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    let base = default_params();
    let preset = |name: &'static str, params: Params| FactoryPreset { name, params };
    vec![
        preset("Default", base.clone()),
        preset(
            "Slapback",
            Params {
                mode: DelayMode::Mono,
                time_ms_l: 96.0,
                time_ms_r: 96.0,
                feedback: 8.0,
                cross_feedback: 0.0,
                low_cut_hz: 220.0,
                high_cut_hz: 7_000.0,
                saturation: 18.0,
                mix: 24.0,
                ..base.clone()
            },
        ),
        // The two note-named presets ride the project tempo: a "Quarter Ping"
        // pinned to 500 ms is only a quarter note at 120 BPM. Their free
        // times still hold the same figure at 120, so switching Sync off
        // lands where the name says.
        preset(
            "Quarter Ping",
            Params {
                mode: DelayMode::PingPong,
                time_ms_l: 500.0,
                time_ms_r: 500.0,
                feedback: 38.0,
                cross_feedback: 0.0,
                low_cut_hz: 160.0,
                high_cut_hz: 10_000.0,
                saturation: 6.0,
                mix: 26.0,
                sync: true,
                division_l: 10,
                division_r: 10,
                link: true,
                ..base.clone()
            },
        ),
        preset(
            "Dotted Eighth",
            Params {
                mode: DelayMode::PingPong,
                time_ms_l: 375.0,
                time_ms_r: 250.0,
                feedback: 32.0,
                cross_feedback: 0.0,
                low_cut_hz: 200.0,
                high_cut_hz: 11_000.0,
                saturation: 4.0,
                mix: 22.0,
                sync: true,
                division_l: 9,
                division_r: 7,
                ..base.clone()
            },
        ),
        preset(
            "Tape Echo",
            Params {
                mode: DelayMode::Stereo,
                time_ms_l: 320.0,
                time_ms_r: 340.0,
                feedback: 52.0,
                cross_feedback: 25.0,
                low_cut_hz: 260.0,
                high_cut_hz: 4_200.0,
                saturation: 62.0,
                mix: 30.0,
                output_db: -1.5,
                mod_depth: 22.0,
                mod_rate_hz: 0.6,
                ..base.clone()
            },
        ),
        preset(
            "Dub Space",
            Params {
                mode: DelayMode::PingPong,
                time_ms_l: 620.0,
                time_ms_r: 930.0,
                feedback: 74.0,
                cross_feedback: 35.0,
                low_cut_hz: 320.0,
                high_cut_hz: 3_200.0,
                saturation: 48.0,
                mix: 38.0,
                mod_depth: 12.0,
                mod_rate_hz: 0.35,
                diffusion: 20.0,
                ..base.clone()
            },
        ),
        preset(
            "Wide Doubler",
            Params {
                mode: DelayMode::Stereo,
                time_ms_l: 28.0,
                time_ms_r: 41.0,
                feedback: 0.0,
                cross_feedback: 0.0,
                low_cut_hz: 140.0,
                high_cut_hz: 14_000.0,
                saturation: 0.0,
                mix: 42.0,
                mod_depth: 18.0,
                mod_rate_hz: 0.9,
                width: 150.0,
                ..base.clone()
            },
        ),
        preset(
            "Vocal Throw",
            Params {
                mode: DelayMode::PingPong,
                time_ms_l: 375.0,
                time_ms_r: 375.0,
                feedback: 42.0,
                cross_feedback: 0.0,
                low_cut_hz: 300.0,
                high_cut_hz: 6_500.0,
                saturation: 10.0,
                mix: 32.0,
                sync: true,
                division_l: 9,
                division_r: 9,
                link: true,
                duck: 65.0,
                ..base.clone()
            },
        ),
        preset(
            "Ambient Wash",
            Params {
                mode: DelayMode::PingPong,
                time_ms_l: 1_200.0,
                time_ms_r: 1_800.0,
                feedback: 82.0,
                cross_feedback: 60.0,
                low_cut_hz: 240.0,
                high_cut_hz: 5_200.0,
                saturation: 22.0,
                mix: 45.0,
                output_db: -2.0,
                mod_depth: 30.0,
                mod_rate_hz: 0.25,
                diffusion: 75.0,
                width: 140.0,
                ..base
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;

    #[test]
    fn every_preset_is_already_in_range_and_named_once() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Default");
        let mut names: Vec<_> = bank.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), bank.len());
        for preset in &bank {
            let mut clamped = preset.params.clone();
            ipc::sanitize_params(&mut clamped);
            assert_eq!(clamped, preset.params, "{} is out of range", preset.name);
            assert!(preset.params.power && !preset.params.freeze);
        }
    }
}
