//! Which of a channel's inserts feed its gain-reduction meter.
//!
//! The meter reads the reduction a built-in dynamics processor publishes
//! with its telemetry (`BuiltinMeterFrame::gain_reduction_db`), so it covers
//! the built-in Dynamics family. A VST3 publishes no reduction the host can
//! read, so a channel whose only compressor is one shows no reduction meter
//! rather than a meter stuck at zero.

use crate::components::timeline::timeline_state::{InsertLoadStatus, InsertSlotState};

/// The built-in Dynamics family, by catalog stem.
const DYNAMICS_STEMS: &[&str] = &[
    "compresser",
    "zcomp",
    "burnlimit",
    "deesser",
    "duckstep",
    "clipper67",
    "transient",
    "waygate",
];

/// Whether `slot` is a built-in dynamics processor.
pub(crate) fn is_dynamics(slot: &InsertSlotState) -> bool {
    slot.plugin_id
        .as_deref()
        .and_then(SpherePluginHost::resolve_builtin_stem)
        .is_some_and(|stem| DYNAMICS_STEMS.contains(&stem))
}

/// The ids of `inserts`' dynamics processors in the signal path, in chain
/// order: what the channel's reduction meter adds up. Empty when there are
/// none — the strip then has no reduction meter.
pub(crate) fn reduction_inserts(inserts: &[InsertSlotState]) -> Vec<String> {
    inserts
        .iter()
        .filter(|slot| {
            is_dynamics(slot)
                && slot.enabled
                && !slot.bypassed
                && !matches!(slot.load_status, InsertLoadStatus::Disabled)
        })
        .map(|slot| slot.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(id: &str, plugin: &str) -> InsertSlotState {
        let mut slot = InsertSlotState::empty(id);
        slot.plugin_id = Some(plugin.to_string());
        slot.load_status = InsertLoadStatus::Ready;
        slot
    }

    #[test]
    fn only_built_in_dynamics_in_the_path_feed_the_meter() {
        let mut bypassed = slot("comp-off", "compresser");
        bypassed.bypassed = true;
        let inserts = vec![
            slot("eq", "equz8"),
            slot("comp", "compresser"),
            slot("lim", "builtin:burnlimit"),
            bypassed,
            slot("vst", "ABCDEF0123456789ABCDEF0123456789"),
        ];
        assert_eq!(reduction_inserts(&inserts), vec!["comp", "lim"]);
        assert!(reduction_inserts(&[slot("eq", "equzx")]).is_empty());
    }
}
