//! MIDI FX on the control thread: rebuild hand-over, settings synced without
//! a rebuild, and live parameter / bypass edits.
//!
//! The mirror graph (`self.runtime`) and the graph the callback runs share
//! each track's [`RuntimeMidiFx`] (an `Arc`), so changing it here changes what
//! the audio thread plays at its next block. No command is needed.

use super::*;
use crate::runtime::RuntimeMidiFx;

impl EngineInner {
    /// Give each track of the rebuilt `runtime` the chain its track is
    /// already running, brought to the new snapshot. Called before the new
    /// graph is mirrored and queued.
    pub(super) fn hand_over_midi_fx(
        &self,
        runtime: &mut RuntimeProject,
        snapshot: &EngineProjectSnapshot,
        sample_rate: u32,
    ) {
        let mut handed = false;
        {
            let mirror = self.runtime.lock();
            for track in runtime.tracks.iter_mut() {
                let Some(running) = mirror
                    .tracks
                    .iter()
                    .find(|t| t.id == track.id)
                    .and_then(|t| t.midi_fx.clone())
                else {
                    continue;
                };
                let specs = snapshot
                    .tracks
                    .iter()
                    .find(|t| t.id == track.id)
                    .map(|t| t.midi_fx.as_slice())
                    .unwrap_or(&[]);
                running.reconcile(specs, sample_rate);
                track.midi_fx = (!running.is_spent()).then_some(running);
                handed = true;
            }
        }
        if handed {
            // A chain kept for its last note-offs lands on a track built
            // without MIDI FX: give its lists the chain's capacity.
            runtime.reserve_midi_block_events();
        }
    }

    /// Bring every running chain to `snapshot`'s settings (bypass, parameter
    /// values) when the graph is not rebuilt.
    pub(super) fn apply_midi_fx_changes(&self, snapshot: &EngineProjectSnapshot, sample_rate: u32) {
        let mirror = self.runtime.lock();
        for track in &mirror.tracks {
            let Some(running) = track.midi_fx.as_ref() else {
                continue;
            };
            if let Some(spec) = snapshot.tracks.iter().find(|t| t.id == track.id) {
                running.reconcile(&spec.midi_fx, sample_rate);
            }
        }
    }

    fn running_midi_fx(&self, track_id: &str) -> Option<Arc<RuntimeMidiFx>> {
        self.runtime
            .lock()
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.midi_fx.clone())
    }

    /// Set a MIDI FX parameter live (an editor knob). Returns `false` when
    /// the track has no running chain with that slot and parameter — the next
    /// project sync then carries the value.
    pub fn set_midi_fx_param(
        &self,
        track_id: &str,
        slot_id: &str,
        param_id: &str,
        value: f32,
    ) -> bool {
        self.running_midi_fx(track_id)
            .is_some_and(|fx| fx.set_param(slot_id, param_id, value))
    }

    /// Bypass or re-enable a MIDI FX slot live.
    pub fn set_midi_fx_enabled(&self, track_id: &str, slot_id: &str, enabled: bool) -> bool {
        self.running_midi_fx(track_id)
            .is_some_and(|fx| fx.set_enabled(slot_id, enabled))
    }
}
