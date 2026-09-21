//! Cross-process wire indices for the editor's UI parameters.
//!
//! The shared-memory param ring ([`SharedParamEvent`] in
//! `SpherePluginHost::audio_bridge`) carries a `u32` id; the DSP's
//! [`Dsp::apply_ui_param`](crate::Dsp::apply_ui_param) takes the editor's
//! string ids. This table is the single source of truth for the mapping —
//! both the main app (string → index, UI thread) and the plugin-host binary
//! (index → string, audio producer thread) link this same crate.
//!
//! **APPEND-ONLY.** The index is a cross-process ABI: reordering or inserting
//! mid-table silently retargets every knob behind it. Add new ids at the end
//! and extend the pinned-index tests below.

/// Wire index → `apply_ui_param` id. Index in this slice *is* the wire id.
pub const UI_PARAM_IDS: &[&str] = &[
    "power",             // 0
    "input_trim",        // 1
    "output_trim",       // 2
    "gate_on",           // 3
    "drive_on",          // 4
    "amp_on",            // 5
    "mod_on",            // 6
    "delay_on",          // 7
    "reverb_on",         // 8
    "cab_on",            // 9
    "drive_model",       // 10
    "amp_model",         // 11
    "cab_model",         // 12
    "tone_engine",       // 13
    "path_slot_0",       // 14
    "path_slot_1",       // 15
    "path_slot_2",       // 16
    "path_slot_3",       // 17
    "path_slot_4",       // 18
    "path_slot_5",       // 19
    "path_slot_6",       // 20
    "gate_thresh",       // 21
    "drive_gain",        // 22
    "drive_tone",        // 23
    "drive_level",       // 24
    "amp_gain",          // 25
    "amp_bass",          // 26
    "amp_middle",        // 27
    "amp_treble",        // 28
    "amp_presence",      // 29
    "amp_master",        // 30
    "chorus_rate",       // 31
    "chorus_depth",      // 32
    "chorus_mix",        // 33
    "delay_time",        // 34
    "delay_fb",          // 35
    "delay_mix",         // 36
    "reverb_decay",      // 37
    "reverb_mix",        // 38
    "cab_mic",           // 39
    "cab_dist",          // 40
    "nam_input_trim",    // 41
    "nam_output_trim",   // 42
    "nam_mix",           // 43
    "nam_loudness_norm", // 44
    "comp_on",           // 45
    "eq_on",             // 46
    "path_slot_7",       // 47
    "path_slot_8",       // 48
    "comp_thresh",       // 49
    "comp_ratio",        // 50
    "comp_attack",       // 51
    "comp_release",      // 52
    "comp_makeup",       // 53
    "eq_low_gain",       // 54
    "eq_mid1_freq",      // 55
    "eq_mid1_gain",      // 56
    "eq_mid2_freq",      // 57
    "eq_mid2_gain",      // 58
    "eq_high_gain",      // 59
    "clear_clip",        // 60 (action, not a param: resets sticky clip lights)
    "mod_model",         // 61
    "wah_on",            // 62
    "wah_model",         // 63
    "path_slot_9",       // 64
    "wah_pos",           // 65
    "wah_res",           // 66
    "wah_sens",          // 67
    "cab_mic_type",      // 68
    "reverb_model",      // 69
    "reverb_shimmer",    // 70
    "delay_model",       // 71
    "delay_tone",        // 72
    // Second instances of the doublable stages (`StageKind::Drive2` …), plus
    // the five path slots they need. Appended, never interleaved: an index in
    // this table is an ABI a running host may already hold.
    "path_slot_10",  // 73
    "path_slot_11",  // 74
    "path_slot_12",  // 75
    "path_slot_13",  // 76
    "path_slot_14",  // 77
    "drive2_on",     // 78
    "drive2_model",  // 79
    "drive2_gain",   // 80
    "drive2_tone",   // 81
    "drive2_level",  // 82
    "mod2_on",       // 83
    "mod2_model",    // 84
    "chorus2_rate",  // 85
    "chorus2_depth", // 86
    "chorus2_mix",   // 87
    "delay2_on",     // 88
    "delay2_model",  // 89
    "delay2_time",   // 90
    "delay2_fb",     // 91
    "delay2_mix",    // 92
    "delay2_tone",   // 93
    "eq2_on",        // 94
    "eq2_low_gain",  // 95
    "eq2_mid1_freq", // 96
    "eq2_mid1_gain", // 97
    "eq2_mid2_freq", // 98
    "eq2_mid2_gain", // 99
    "eq2_high_gain", // 100
    "comp2_on",      // 101
    "comp2_thresh",  // 102
    "comp2_ratio",   // 103
    "comp2_attack",  // 104
    "comp2_release", // 105
    "comp2_makeup",  // 106
    "eq_model",      // 107
    "eq2_model",     // 108
    "nam_slim_size", // 109
];

/// String id → wire index. Linear scan over a small table — control/UI
/// thread only; never call on the audio path.
pub fn ui_param_index(id: &str) -> Option<u32> {
    UI_PARAM_IDS.iter().position(|&p| p == id).map(|i| i as u32)
}

/// Wire index → string id. O(1) slice index; safe on the host's audio
/// producer thread (no allocation, no locking).
pub fn ui_param_id(index: u32) -> Option<&'static str> {
    UI_PARAM_IDS.get(index as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{
        AmpModel, CabModel, DelayModel, DriveModel, Dsp, EqModel, ModModel, ReverbModel, StageKind,
        ToneEngineKind, WahModel, apply_to_params, default_params, ui_values,
    };

    #[test]
    fn round_trips_and_is_unique() {
        for (i, &id) in UI_PARAM_IDS.iter().enumerate() {
            assert_eq!(ui_param_index(id), Some(i as u32), "id `{id}`");
            assert_eq!(ui_param_id(i as u32), Some(id));
        }
        let mut ids: Vec<_> = UI_PARAM_IDS.to_vec();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate id in UI_PARAM_IDS");
        assert_eq!(ui_param_id(UI_PARAM_IDS.len() as u32), None);
        assert_eq!(ui_param_index("does_not_exist"), None);
    }

    /// Every table entry must be routed by `apply_ui_param` — a table id the
    /// DSP does not accept would be a dead knob over the wire.
    #[test]
    fn every_entry_is_accepted_by_apply_ui_param() {
        let mut dsp = Dsp::new(48_000.0);
        for &id in UI_PARAM_IDS {
            assert!(dsp.apply_ui_param(id, 1.0), "id `{id}` was not routed");
        }
    }

    /// Pin the ABI: these literal indices travel between processes. An
    /// accidental reorder/insert must fail here, loudly.
    #[test]
    fn wire_indices_are_pinned() {
        assert_eq!(UI_PARAM_IDS.len(), 110);
        assert_eq!(ui_param_index("power"), Some(0));
        assert_eq!(ui_param_index("gate_on"), Some(3));
        assert_eq!(ui_param_index("drive_model"), Some(10));
        assert_eq!(ui_param_index("tone_engine"), Some(13));
        assert_eq!(ui_param_index("path_slot_0"), Some(14));
        assert_eq!(ui_param_index("path_slot_6"), Some(20));
        assert_eq!(ui_param_index("gate_thresh"), Some(21));
        assert_eq!(ui_param_index("drive_gain"), Some(22));
        assert_eq!(ui_param_index("amp_gain"), Some(25));
        assert_eq!(ui_param_index("chorus_rate"), Some(31));
        assert_eq!(ui_param_index("delay_time"), Some(34));
        assert_eq!(ui_param_index("reverb_decay"), Some(37));
        assert_eq!(ui_param_index("cab_mic"), Some(39));
        assert_eq!(ui_param_index("nam_loudness_norm"), Some(44));
        assert_eq!(ui_param_index("comp_on"), Some(45));
        assert_eq!(ui_param_index("path_slot_7"), Some(47));
        assert_eq!(ui_param_index("path_slot_8"), Some(48));
        assert_eq!(ui_param_index("comp_thresh"), Some(49));
        assert_eq!(ui_param_index("eq_low_gain"), Some(54));
        assert_eq!(ui_param_index("eq_high_gain"), Some(59));
        assert_eq!(ui_param_index("clear_clip"), Some(60));
        assert_eq!(ui_param_index("mod_model"), Some(61));
        assert_eq!(ui_param_index("wah_on"), Some(62));
        assert_eq!(ui_param_index("wah_model"), Some(63));
        assert_eq!(ui_param_index("path_slot_9"), Some(64));
        assert_eq!(ui_param_index("wah_pos"), Some(65));
        assert_eq!(ui_param_index("wah_res"), Some(66));
        assert_eq!(ui_param_index("wah_sens"), Some(67));
        assert_eq!(ui_param_index("cab_mic_type"), Some(68));
        assert_eq!(ui_param_index("reverb_model"), Some(69));
        assert_eq!(ui_param_index("reverb_shimmer"), Some(70));
        assert_eq!(ui_param_index("delay_model"), Some(71));
        assert_eq!(ui_param_index("delay_tone"), Some(72));
        assert_eq!(ui_param_index("path_slot_10"), Some(73));
        assert_eq!(ui_param_index("path_slot_14"), Some(77));
        assert_eq!(ui_param_index("drive2_on"), Some(78));
        assert_eq!(ui_param_index("drive2_gain"), Some(80));
        assert_eq!(ui_param_index("mod2_on"), Some(83));
        assert_eq!(ui_param_index("chorus2_rate"), Some(85));
        assert_eq!(ui_param_index("delay2_on"), Some(88));
        assert_eq!(ui_param_index("eq2_on"), Some(94));
        assert_eq!(ui_param_index("comp2_on"), Some(101));
        assert_eq!(ui_param_index("comp2_makeup"), Some(106));
        assert_eq!(ui_param_index("eq_model"), Some(107));
        assert_eq!(ui_param_index("eq2_model"), Some(108));
        assert_eq!(ui_param_index("nam_slim_size"), Some(109));
    }

    /// `ui_values` must cover every wire id except `clear_clip` (an action,
    /// not state) exactly once — anything missed would silently drop out of
    /// state replays; anything duplicated would double-apply.
    #[test]
    fn ui_values_covers_every_wire_id_except_clear_clip() {
        let values = ui_values(&default_params());
        let mut emitted: Vec<&str> = values.iter().map(|(id, _)| *id).collect();
        emitted.sort_unstable();
        let before = emitted.len();
        emitted.dedup();
        assert_eq!(before, emitted.len(), "duplicate id in ui_values");
        for &id in UI_PARAM_IDS {
            if id == "clear_clip" {
                assert!(!emitted.contains(&id), "clear_clip must not be emitted");
            } else {
                assert!(emitted.contains(&id), "ui_values missing `{id}`");
            }
        }
        assert_eq!(values.len(), UI_PARAM_IDS.len() - 1);
    }

    /// Replaying `ui_values(src)` in order into fresh params reproduces the
    /// source — the property state restore relies on (incl. the amp_model /
    /// tone_engine ordering).
    #[test]
    fn ui_values_replay_reproduces_the_source_params() {
        let mut src = default_params();
        // Non-default everything that has interactions.
        src.power = false;
        src.tone_engine = ToneEngineKind::NamCapture;
        src.amp_model = AmpModel::Recto;
        src.drive_model = DriveModel::Fuzz;
        src.cab_model = CabModel::Tweed1x12;
        src.reverb_model = ReverbModel::Shimmer;
        src.reverb_shimmer = 37.0;
        src.delay_model = DelayModel::PingPong;
        src.delay_tone = 8.25;
        src.mod_model = ModModel::Phaser;
        src.wah_model = WahModel::TouchWah;
        src.wah_on = false;
        src.wah_pos = 7.25;
        src.wah_res = 8.0;
        src.wah_sens = 2.5;
        src.stage_order = [None; crate::PATH_SLOTS];
        src.stage_order[0] = Some(StageKind::Eq);
        src.stage_order[1] = Some(StageKind::Amp);
        src.stage_order[9] = Some(StageKind::Wah);
        src.delay_time_ms = 777.0;
        src.comp_ratio = 8.0;
        src.eq_mid2_gain_db = -9.0;
        src.nam_loudness_norm = false;
        src.nam_slim_size = 40.0;

        let mut restored = default_params();
        for (id, value) in ui_values(&src) {
            assert!(apply_to_params(&mut restored, id, value), "id `{id}`");
        }
        assert_eq!(restored, src);

        // And through a live Dsp (values pass set_params clamping unchanged
        // because they originate from legal Params).
        let mut dsp = Dsp::new(48_000.0);
        for (id, value) in ui_values(&src) {
            assert!(dsp.apply_ui_param(id, value));
        }
        assert_eq!(*dsp.params(), src);
    }

    /// Pin the numeric model-select values the editor's `postModel` maps send
    /// (`editorui/src/bridge.ts`): index into each enum's `ALL` order, reached
    /// through the same `from_model_id` ids the editor uses.
    #[test]
    fn model_select_wire_values_match_editor_ids() {
        let amp = [
            "mandarin",
            "plexi",
            "twin",
            "topboost",
            "recto",
            "jcm",
            "slate",
            "bassman",
            "boutique",
            "invader",
            "tweed_combo",
        ];
        for (i, id) in amp.iter().enumerate() {
            assert_eq!(
                AmpModel::from_model_id(id),
                Some(AmpModel::ALL[i]),
                "amp `{id}`"
            );
        }
        let drive = [
            "screamer",
            "minotaur",
            "rat",
            "breaker",
            "fuzz",
            "centurion",
            "ds_one",
            "super_drive",
            "metal_core",
            "tight_rift",
            "amber_crunch",
            "copper_fuzz",
        ];
        for (i, id) in drive.iter().enumerate() {
            assert_eq!(
                DriveModel::from_model_id(id),
                Some(DriveModel::ALL[i]),
                "drive `{id}`"
            );
        }
        let cab = [
            "vintage_cab",
            "american_2x12",
            "tweed_1x12",
            "modern_412",
            "open_back",
            "vintage_212",
            "oversized_412",
            "bass_cabinet",
            "brit_412",
            "uber_412",
            "slo_412",
            // Not a modeled voicing — the convolution engine (`dsp::ir`).
            "ir",
            "modern_212",
            "american_1x12",
        ];
        for (i, id) in cab.iter().enumerate() {
            assert_eq!(
                CabModel::from_model_id(id),
                Some(CabModel::ALL[i]),
                "cab `{id}`"
            );
        }
        let reverb = ["plate", "room", "hall", "shimmer"];
        for (i, id) in reverb.iter().enumerate() {
            assert_eq!(
                ReverbModel::from_model_id(id),
                Some(ReverbModel::ALL[i]),
                "reverb `{id}`"
            );
        }
        // Append-only: the first four are what saved projects already carry on
        // the `mod_model` wire.
        let mod_models = [
            "chorus",
            "phaser",
            "flanger",
            "tremolo",
            "molam_swirl",
            "phin_vibe",
            "khaen_swirl",
            "bi_lam",
            "isan_jet",
            "soft_phase",
            "wide_vibe",
        ];
        for (i, id) in mod_models.iter().enumerate() {
            assert_eq!(
                ModModel::from_model_id(id),
                Some(ModModel::ALL[i]),
                "mod `{id}`"
            );
        }
        let delay = ["tape", "digital", "analog", "ping_pong", "dual"];
        for (i, id) in delay.iter().enumerate() {
            assert_eq!(
                DelayModel::from_model_id(id),
                Some(DelayModel::ALL[i]),
                "delay `{id}`"
            );
        }
        let wah = ["cry_wah", "touch_wah"];
        for (i, id) in wah.iter().enumerate() {
            assert_eq!(
                WahModel::from_model_id(id),
                Some(WahModel::ALL[i]),
                "wah `{id}`"
            );
        }
        let eq = ["parametric", "vintage_eq", "modern_eq"];
        for (i, id) in eq.iter().enumerate() {
            assert_eq!(
                EqModel::from_model_id(id),
                Some(EqModel::ALL[i]),
                "eq `{id}`"
            );
        }
        // bridge.ts sends tone_engine=1 for nam_capture, 2 for bypass.
        assert_eq!(ToneEngineKind::NamCapture.index(), 1);
        assert_eq!(ToneEngineKind::Bypass.index(), 2);
        assert_eq!(ToneEngineKind::Classic.index(), 0);
    }
}
