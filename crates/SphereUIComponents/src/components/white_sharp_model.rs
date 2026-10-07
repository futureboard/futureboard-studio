//! What the native WhiteSharp editor edits: the plug-in's params, their wire
//! edits, its presets and its knob table. No GPUI here.
//!
//! Every value goes back to the plug-in as the wire edits [`wire_diff`]
//! computes, through the DSP crate's own `ipc` table — so an edit lands in
//! the editor's copy exactly as it lands in Studio's state mirror and in the
//! host.

use std::sync::{Arc, OnceLock};

use whitesharp::Params;

use crate::components::fx_model::{spec, KnobSpec, Taper, Unit};

/// Every param as `(wire index, value)`, in wire order.
pub fn wire_values(params: &Params) -> Vec<(u32, f32)> {
    whitesharp::ipc::ui_values(params)
        .into_iter()
        .filter_map(|(id, value)| whitesharp::ui_param_index(id).map(|i| (i, value)))
        .collect()
}

/// The wire edits that turn `before` into `after`.
pub fn wire_diff(before: &Params, after: &Params) -> Vec<(u32, f32)> {
    let old = wire_values(before);
    wire_values(after)
        .into_iter()
        .zip(old)
        .filter(|((_, new), (_, old))| new != old)
        .map(|(edit, _)| edit)
        .collect()
}

/// `params` with one edit applied the way the DSP will take it.
pub fn with(params: &Params, id: &str, value: f32) -> Params {
    let mut next = params.clone();
    whitesharp::ipc::apply_ui_param(&mut next, id, value);
    next
}

/// A param's value by id.
pub fn value(params: &Params, id: &str) -> f32 {
    whitesharp::ipc::ui_values(params)
        .into_iter()
        .find(|(candidate, _)| *candidate == id)
        .map_or(0.0, |(_, value)| value)
}

// ── Presets ─────────────────────────────────────────────────────────────────

/// What a preset sets: the correction *style*. The key, the scale, the
/// removed and bypassed notes, the input type, tuning, levels and latency
/// mode belong to the song, the singer and the rig, and stay as they are.
pub const STYLE_IDS: [&str; 15] = [
    "retuneMs",
    "humanize",
    "flexTune",
    "vibratoDb",
    "formant",
    "throat",
    "transpose",
    "classic",
    "vibratoShape",
    "vibratoRateHz",
    "vibratoDelayMs",
    "vibratoOnsetMs",
    "vibratoPitch",
    "vibratoAmp",
    "vibratoVariation",
];

pub struct Preset {
    pub name: &'static str,
    pub params: Params,
}

pub fn presets() -> Arc<Vec<Preset>> {
    static BANK: OnceLock<Arc<Vec<Preset>>> = OnceLock::new();
    BANK.get_or_init(|| {
        Arc::new(
            whitesharp::factory_presets()
                .into_iter()
                .map(|p| Preset {
                    name: p.name,
                    params: p.params,
                })
                .collect(),
        )
    })
    .clone()
}

/// `preset`'s style over `current`'s song.
pub fn preset_applied(current: &Params, preset: &Params) -> Params {
    STYLE_IDS.iter().fold(current.clone(), |params, id| {
        with(&params, id, value(preset, id))
    })
}

/// The preset whose style the params carry, if any.
pub fn matching_preset(params: &Params) -> Option<usize> {
    presets().iter().position(|preset| {
        STYLE_IDS
            .iter()
            .all(|id| (value(params, id) - value(&preset.params, id)).abs() < 0.05)
    })
}

// ── Knobs ───────────────────────────────────────────────────────────────────

/// The knob for param `id`, ranges from the DSP crate's descriptor.
pub fn knob(id: &str) -> Option<KnobSpec> {
    let range = whitesharp::descriptor()
        .params
        .iter()
        .find(|p| p.id == id)?;
    let (label, taper, unit, centre) = match id {
        // Fine at the fast end, where the character changes most.
        "retuneMs" => ("Retune Speed", Taper::Square, Unit::Ms, None),
        "humanize" => ("Humanize", Taper::Linear, Unit::Percent, None),
        "flexTune" => ("Flex-Tune", Taper::Linear, Unit::Percent, None),
        // Antares calls it Natural Vibrato; one word keeps the cell's
        // readout on the row's line.
        "vibratoDb" => ("Vibrato", Taper::Linear, Unit::Db, Some(0.0)),
        "throat" => ("Throat", Taper::Linear, Unit::Percent, Some(100.0)),
        "transpose" => ("Transpose", Taper::Linear, Unit::Semitones, Some(0.0)),
        "detune" => ("Detune", Taper::Linear, Unit::Cents, Some(0.0)),
        "tracking" => ("Tracking", Taper::Linear, Unit::Percent, None),
        "mix" => ("Mix", Taper::Linear, Unit::Percent, None),
        "vibratoRateHz" => ("Rate", Taper::Log, Unit::Hz, None),
        "vibratoDelayMs" => ("Delay", Taper::Square, Unit::Ms, None),
        "vibratoOnsetMs" => ("Onset", Taper::Square, Unit::Ms, None),
        "vibratoPitch" => ("Pitch", Taper::Linear, Unit::CentsDepth, None),
        "vibratoAmp" => ("Amplitude", Taper::Linear, Unit::Percent, None),
        "vibratoVariation" => ("Variation", Taper::Linear, Unit::Percent, None),
        "outputDb" => ("Output", Taper::Linear, Unit::Db, Some(0.0)),
        _ => return None,
    };
    let mut knob = spec(range.id, label, range.min, range.max, taper, unit);
    if let Some(centre) = centre {
        knob.bipolar = true;
        knob.centre = centre;
    }
    Some(knob)
}

/// A param's factory default, by id.
pub fn default_value(id: &str) -> f32 {
    value(&whitesharp::default_params(), id)
}

/// `note` (MIDI) as a name and octave, `A4` for 69.
pub fn note_name(note: i32) -> String {
    format!(
        "{}{}",
        whitesharp::NOTE_NAMES[note.rem_euclid(12) as usize],
        note.div_euclid(12) - 1
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_brings_its_style_and_leaves_the_song() {
        let mut song = whitesharp::default_params();
        song.key = 7;
        song.scale = whitesharp::Scale::Minor;
        song.remove_mask = 0b101;
        song.input_type = whitesharp::InputType::Soprano;
        song.latency = whitesharp::LatencyMode::Live;
        let hard = presets()
            .iter()
            .position(|p| p.name == "Hard Tune")
            .unwrap();
        let applied = preset_applied(&song, &presets()[hard].params);
        assert_eq!(applied.retune_ms, 0.0);
        assert_eq!(applied.key, 7);
        assert_eq!(applied.scale, whitesharp::Scale::Minor);
        assert_eq!(applied.remove_mask, 0b101);
        assert_eq!(applied.input_type, whitesharp::InputType::Soprano);
        assert_eq!(applied.latency, whitesharp::LatencyMode::Live);
        assert_eq!(matching_preset(&applied), Some(hard));
        // Changing the song keeps the preset recognised; the style does not.
        assert_eq!(matching_preset(&with(&applied, "key", 2.0)), Some(hard));
        assert_eq!(matching_preset(&with(&applied, "retuneMs", 90.0)), None);
    }

    #[test]
    fn a_diff_lands_exactly_through_the_wire() {
        let before = whitesharp::default_params();
        let after = with(&with(&before, "scale", 2.0), "bypassMask", 6.0);
        let mut landed = before.clone();
        for (index, value) in wire_diff(&before, &after) {
            whitesharp::ipc::apply_wire_param(&mut landed, index, value);
        }
        assert_eq!(landed, after);
        assert_eq!(wire_diff(&after, &after), Vec::new());
    }

    #[test]
    fn every_knob_round_trips_its_default() {
        for id in [
            "retuneMs",
            "humanize",
            "flexTune",
            "vibratoDb",
            "throat",
            "transpose",
            "detune",
            "tracking",
            "mix",
            "outputDb",
            "vibratoRateHz",
            "vibratoDelayMs",
            "vibratoOnsetMs",
            "vibratoPitch",
            "vibratoAmp",
            "vibratoVariation",
        ] {
            let knob = knob(id).unwrap_or_else(|| panic!("no knob for {id}"));
            let default = default_value(id);
            assert!(
                (knob.from_knob(knob.to_knob(default)) - default).abs()
                    < 0.011 * default.abs().max(1.0),
                "{id}"
            );
        }
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(-1), "B-2");
    }
}
