//! WhiteSharp's factory presets.
//!
//! A preset is a correction *style*: speed, humanize, flex and vibrato. It
//! never carries the key, the scale, the removed or bypassed notes or the
//! input type — those belong to the song and the singer, and an editor
//! loading a preset keeps them.

use crate::{Params, VibratoShape, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    let base = default_params();
    let style =
        |name: &'static str, retune_ms: f32, humanize: f32, flex_tune: f32, vibrato_db: f32| {
            FactoryPreset {
                name,
                params: Params {
                    retune_ms,
                    humanize,
                    flex_tune,
                    vibrato_db,
                    ..base.clone()
                },
            }
        };
    vec![
        FactoryPreset {
            name: "Default",
            params: base.clone(),
        },
        style("Hard Tune", 0.0, 0.0, 0.0, 0.0),
        style("Tight Pop", 10.0, 15.0, 0.0, 0.0),
        style("Natural Lead", 45.0, 40.0, 30.0, 0.0),
        style("Transparent", 80.0, 60.0, 60.0, 0.0),
        style("Ballad", 120.0, 70.0, 45.0, 2.0),
        style("Flat Vibrato", 25.0, 20.0, 0.0, -12.0),
        style("Wide Vibrato", 40.0, 30.0, 20.0, 6.0),
        FactoryPreset {
            name: "Classic Hard",
            params: Params {
                classic: true,
                retune_ms: 0.0,
                ..base.clone()
            },
        },
        FactoryPreset {
            name: "Vibrato Lead",
            params: Params {
                retune_ms: 30.0,
                humanize: 30.0,
                vibrato_db: -6.0,
                vibrato_shape: VibratoShape::Sine,
                vibrato_rate_hz: 5.5,
                vibrato_delay_ms: 400.0,
                vibrato_onset_ms: 350.0,
                vibrato_pitch: 28.0,
                vibrato_amp: 10.0,
                vibrato_variation: 25.0,
                ..base.clone()
            },
        },
        FactoryPreset {
            name: "Chipmunk",
            params: Params {
                retune_ms: 15.0,
                transpose: 12.0,
                formant: false,
                ..base.clone()
            },
        },
        FactoryPreset {
            name: "Deep Voice",
            params: Params {
                retune_ms: 30.0,
                transpose: -7.0,
                throat: 125.0,
                ..base
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;

    #[test]
    fn every_preset_is_in_range_named_once_and_song_neutral() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Default");
        let mut names: Vec<_> = bank.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), bank.len());
        let base = default_params();
        for preset in &bank {
            let mut clamped = preset.params.clone();
            ipc::sanitize_params(&mut clamped);
            assert_eq!(clamped, preset.params, "{} is out of range", preset.name);
            assert_eq!(preset.params.key, base.key);
            assert_eq!(preset.params.scale, base.scale);
            assert_eq!(preset.params.input_type, base.input_type);
            assert_eq!(preset.params.remove_mask, 0);
            assert_eq!(preset.params.bypass_mask, 0);
        }
    }
}
