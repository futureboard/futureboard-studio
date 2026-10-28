//! Futureboard Studio Mobile — Standalone Rust Audio, Arrangement, MIDI & Built-In Plugin Engine.

pub mod audio_io;
pub mod engine;
pub mod ffi;
pub mod plugin_host;

pub use engine::{
    AudioClip, AutomationLane, AutomationPoint, InsertSlotSnapshot, MidiClip, MidiNote,
    ProjectSnapshot, SendSlot, StudioMobileEngine, TelemetrySnapshot, TrackKind, TrackMeterFrame,
    TrackSnapshot,
};
pub use plugin_host::{
    plugin_catalog, BuiltinPluginInstance, PluginCatalogEntry, PluginKind, PluginMeterSnapshot,
    PluginParamSpec,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_catalog_plugins_instantiate_and_process_audio() {
        let catalog = plugin_catalog();
        assert!(catalog.len() >= 25, "Expected at least 25 built-in plugins");
        for entry in &catalog {
            let mut inst = BuiltinPluginInstance::new(&entry.id, 48_000.0)
                .unwrap_or_else(|| panic!("Failed to instantiate plugin {}", entry.id));
            if entry.kind == PluginKind::Instrument {
                inst.note_on(60, 0.9);
            }
            let mut left = vec![0.15f32; 256];
            let mut right = vec![-0.15f32; 256];
            inst.process_stereo_block(&mut left, &mut right);
            assert!(
                left.iter().all(|v| v.is_finite()) && right.iter().all(|v| v.is_finite()),
                "Plugin {} produced non-finite samples",
                entry.id
            );
            inst.all_notes_off();
        }
    }

    #[test]
    fn studio_engine_renders_demo_project_with_nonzero_signal_and_telemetry() {
        let mut engine = StudioMobileEngine::new(48_000);
        let snap = engine.snapshot();
        assert_eq!(snap.tracks.len(), 6);
        assert_eq!(snap.master_track.inserts.len(), 3);

        engine.set_playing(true);
        let mut out = vec![0.0f32; 1024 * 2];
        let mut max_abs = 0.0f32;
        for _ in 0..8 {
            engine.render_interleaved_stereo(&mut out, 1024);
            for &s in &out {
                max_abs = max_abs.max(s.abs());
            }
        }
        assert!(
            max_abs > 1.0e-4,
            "Expected non-zero audio output from demo arrangement, got max_abs={max_abs}"
        );

        let tel = engine.telemetry();
        assert!(tel.playhead_beats > 0.0);
        assert_eq!(tel.spectrum_db.len(), 128);
        assert_eq!(tel.track_meters.len(), 6);
    }

    #[test]
    fn track_mutations_and_undo_redo_work() {
        let mut engine = StudioMobileEngine::new(48_000);
        let initial_count = engine.snapshot().tracks.len();
        let new_id = engine.add_track("Lead Pluck", TrackKind::Instrument, Some("wrapsynth"));
        assert_eq!(engine.snapshot().tracks.len(), initial_count + 1);

        assert!(engine.undo());
        assert_eq!(engine.snapshot().tracks.len(), initial_count);

        assert!(engine.redo());
        assert_eq!(engine.snapshot().tracks.len(), initial_count + 1);
        assert!(engine.delete_track(&new_id));
        assert_eq!(engine.snapshot().tracks.len(), initial_count);
    }
}
