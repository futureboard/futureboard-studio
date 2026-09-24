//! Serialized per-insert state: what the shared built-in editor's
//! `SelectInstanceMsg.state` carries (see `builtin_plugin_editor_window.rs`
//! in `SphereUIComponents`), and what the project file persists per insert.
//!
//! `Params` already mirrors the DSP one-to-one (`dsp::Params`); this module
//! only adds a schema version so an older saved project doesn't silently
//! misparse against a `Params` shape that has grown fields since.

use crate::Params;

/// Bump when a field is added, removed, or changes meaning in a way that
/// would misparse against an older save. Purely additive changes (a new
/// `Option<T>` defaulting via `#[serde(default)]`) don't require a bump.
///
/// v2: `stage_order` grew from 7 to 9 slots (Comp/Eq stages) — a fixed-size
/// array change that misparses v1 blobs. New comp/eq scalar fields use
/// `#[serde(default)]` and would not have required a bump on their own.
///
/// v3: `stage_order` grew from 9 to 10 slots (Wah stage). The new
/// `mod_model`/`wah_*` fields use `#[serde(default)]` and would not have
/// required a bump on their own.
///
/// v4: `stage_order` grew from 10 to 15 slots — every doublable stage
/// (`StageKind::Drive2` …) can now be in the path at the same time as the one
/// it doubles. The new `stage_b` block uses `#[serde(default)]` and would not
/// have required a bump on its own.
///
/// Unlike v2 and v3, this growth does *not* cost an older project its path:
/// `stage_order` now deserializes through `dsp::deserialize_stage_order`,
/// which pads a shorter saved array with empty slots. A v1/v2/v3 blob loads
/// with its stages in the order it saved them and both new blocks out of the
/// path, so the rig sounds exactly as it did. The version still moves, because
/// an *older* build cannot read a 15-slot array and needs to know why.
pub const SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RodhareistState {
    pub schema_version: u32,
    pub params: Params,
}

impl RodhareistState {
    pub fn new(params: Params) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            params,
        }
    }

    /// Serialize for project persistence / the bridge snapshot. `Params`
    /// contains only plain numbers/bools/small enums — this never allocates
    /// more than a few hundred bytes and is only ever called from the
    /// control/UI thread, never the audio callback.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse a project-file or bridge-delivered blob. `None` schema_version
    /// mismatches are not rejected here — the caller (project load) decides
    /// whether to fall back to defaults; this only reports the parse itself.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CabModel, MicModel, default_params};

    #[test]
    fn round_trips_through_json() {
        let state = RodhareistState::new(default_params());
        let json = state.to_json().expect("serialize");
        let restored = RodhareistState::from_json(&json).expect("deserialize");
        assert_eq!(restored.schema_version, SCHEMA_VERSION);
        assert_eq!(restored.params.amp_gain, state.params.amp_gain);
        assert_eq!(restored.params.drive_model, state.params.drive_model);
        assert_eq!(restored.params.stage_order, state.params.stage_order);
    }

    #[test]
    fn malformed_json_is_a_clean_error_not_a_panic() {
        assert!(RodhareistState::from_json("not json").is_err());
        assert!(RodhareistState::from_json("{}").is_err());
    }

    #[test]
    fn amp_cab_and_microphone_state_round_trips() {
        let mut params = default_params();
        params.amp_model = crate::AmpModel::Slate;
        params.amp_gain = 8.75;
        params.amp_master = 3.25;
        params.cab_model = CabModel::Oversized4x12;
        params.mic_model = MicModel::Condenser;
        params.cab_mic = 73.0;
        params.cab_dist = 61.0;
        let restored =
            RodhareistState::from_json(&RodhareistState::new(params.clone()).to_json().unwrap())
                .unwrap();
        assert_eq!(restored.params, params);
    }

    #[test]
    fn legacy_state_without_microphone_type_defaults_to_dynamic() {
        let state = RodhareistState::new(default_params());
        let mut value = serde_json::to_value(state).unwrap();
        value["params"].as_object_mut().unwrap().remove("mic_model");
        let restored: RodhareistState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.params.mic_model, MicModel::Dynamic);
    }

    #[test]
    fn legacy_state_without_nam_slim_size_defaults_to_full_quality() {
        let state = RodhareistState::new(default_params());
        let mut value = serde_json::to_value(state).unwrap();
        value["params"]
            .as_object_mut()
            .unwrap()
            .remove("nam_slim_size");
        let restored: RodhareistState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.params.nam_slim_size, 100.0);
    }

    /// The Delay slot grew models and a Tone knob after v3. Both are additive
    /// with serde defaults, so a v3 blob written before they existed must
    /// still load — as the tape echo it was saved as.
    #[test]
    fn legacy_state_without_delay_model_loads_as_the_tape_echo() {
        let state = RodhareistState::new(default_params());
        let mut value = serde_json::to_value(state).unwrap();
        let params = value["params"].as_object_mut().unwrap();
        params.remove("delay_model");
        params.remove("delay_tone");
        let restored: RodhareistState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.params.delay_model, crate::DelayModel::Tape);
        assert_eq!(restored.params.delay_tone, 5.0);
    }

    #[test]
    fn delay_model_and_tone_round_trip() {
        let mut params = default_params();
        params.delay_model = crate::DelayModel::PingPong;
        params.delay_tone = 2.75;
        let restored =
            RodhareistState::from_json(&RodhareistState::new(params.clone()).to_json().unwrap())
                .unwrap();
        assert_eq!(restored.params, params);
    }

    /// A v3 project predates the second instances entirely. It must load with
    /// a full, legal B block that no path slot references — the saved rig
    /// sounds exactly as it did, and the new blocks sit in the rack.
    #[test]
    fn legacy_state_without_second_instances_loads_with_them_out_of_the_path() {
        let state = RodhareistState::new(default_params());
        let mut value = serde_json::to_value(state).unwrap();
        let params = value["params"].as_object_mut().unwrap();
        params.remove("stage_b");
        // v3 wrote ten path slots, not fifteen.
        let order = params["stage_order"].as_array_mut().unwrap();
        order.truncate(10);

        let restored: RodhareistState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.params.stage_b, crate::StageBParams::default());
        assert_eq!(restored.params.stage_order.len(), crate::PATH_SLOTS);
        assert!(
            restored
                .params
                .stage_order
                .iter()
                .flatten()
                .all(|s| s.doubles().is_none()),
            "a v3 rig must not gain a doubled block on load"
        );
        // The stages it did save are still there, in order.
        assert_eq!(
            restored.params.stage_order[..10],
            default_params().stage_order[..10]
        );
    }

    #[test]
    fn second_instance_params_round_trip() {
        let mut params = default_params();
        params.stage_b.drive_model = crate::DriveModel::Rat;
        params.stage_b.drive_gain = 9.25;
        params.stage_b.mod_model = crate::ModModel::Flanger;
        params.stage_b.delay_time_ms = 96.0;
        params.stage_b.comp_on = false;
        params.stage_order[9] = Some(crate::StageKind::Drive2);
        params.stage_order[10] = Some(crate::StageKind::Delay2);
        let restored =
            RodhareistState::from_json(&RodhareistState::new(params.clone()).to_json().unwrap())
                .unwrap();
        assert_eq!(restored.params, params);
    }

    #[test]
    fn legacy_state_without_shimmer_amount_keeps_original_voicing() {
        let state = RodhareistState::new(default_params());
        let mut value = serde_json::to_value(state).unwrap();
        value["params"]
            .as_object_mut()
            .unwrap()
            .remove("reverb_shimmer");
        let restored: RodhareistState = serde_json::from_value(value).unwrap();
        assert_eq!(restored.params.reverb_shimmer, 62.0);
    }
}
