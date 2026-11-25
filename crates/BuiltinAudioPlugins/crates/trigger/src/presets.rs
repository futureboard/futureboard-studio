//! Trigger's factory bank: whole `Params` with power on, which the editor's
//! preset bar loads as ordinary wire edits. A preset sets how hits are found
//! and played; it never changes which sample plays.

use crate::{LENGTH_FULL_MS, Mode, Params, default_params};

pub struct FactoryPreset {
    pub name: &'static str,
    pub params: Params,
}

fn preset(name: &'static str, edit: impl FnOnce(&mut Params)) -> FactoryPreset {
    let mut params = default_params();
    edit(&mut params);
    FactoryPreset { name, params }
}

/// The bank, `Default` first.
pub fn factory_presets() -> Vec<FactoryPreset> {
    vec![
        FactoryPreset {
            name: "Default",
            params: default_params(),
        },
        preset("Kick Replace", |p| {
            p.hpf_hz = 30.0;
            p.lpf_hz = 180.0;
            p.threshold_db = -24.0;
            p.retrigger_ms = 90.0;
            p.dynamics = 60.0;
        }),
        preset("Kick Layer", |p| {
            p.hpf_hz = 30.0;
            p.lpf_hz = 180.0;
            p.threshold_db = -24.0;
            p.retrigger_ms = 90.0;
            p.dynamics = 70.0;
            p.mode = Mode::Blend;
            p.sample_db = -6.0;
        }),
        preset("Snare Replace", |p| {
            p.hpf_hz = 150.0;
            p.lpf_hz = 8_000.0;
            p.threshold_db = -22.0;
            p.retrigger_ms = 70.0;
            p.dynamics = 80.0;
        }),
        preset("Snare Layer", |p| {
            p.hpf_hz = 150.0;
            p.lpf_hz = 8_000.0;
            p.threshold_db = -22.0;
            p.retrigger_ms = 70.0;
            p.dynamics = 85.0;
            p.mode = Mode::Blend;
            p.sample_db = -8.0;
        }),
        preset("Tom Replace", |p| {
            p.hpf_hz = 60.0;
            p.lpf_hz = 1_500.0;
            p.threshold_db = -26.0;
            p.retrigger_ms = 120.0;
            p.dynamics = 90.0;
        }),
        preset("Tight Hits", |p| {
            p.threshold_db = -20.0;
            p.retrigger_ms = 40.0;
            p.length_ms = 220.0;
            p.fade_ms = 40.0;
            p.voices = 1;
        }),
        preset("Ghost Notes", |p| {
            p.hpf_hz = 150.0;
            p.lpf_hz = 8_000.0;
            p.threshold_db = -38.0;
            p.retrigger_ms = 45.0;
            p.dynamics = 100.0;
        }),
        preset("Fixed Level", |p| {
            p.threshold_db = -20.0;
            p.dynamics = 0.0;
            p.length_ms = LENGTH_FULL_MS;
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bank_opens_on_the_defaults_and_names_are_unique() {
        let bank = factory_presets();
        assert_eq!(bank[0].name, "Default");
        assert_eq!(bank[0].params, default_params());
        for (i, a) in bank.iter().enumerate() {
            assert!(a.params.power);
            assert!(a.params.sample_name.is_none(), "{} names no sample", a.name);
            let mut copy = a.params.clone();
            crate::ipc::sanitize_params(&mut copy);
            assert_eq!(copy, a.params, "{} survives a sanitize", a.name);
            for b in &bank[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }
}
