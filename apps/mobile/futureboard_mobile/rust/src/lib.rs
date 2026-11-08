//! Futureboard Studio Mobile — Standalone Rust Audio, Arrangement, MIDI & Built-In Plugin Engine.

pub mod audio_io;
pub mod engine;
pub mod ffi;
pub mod plugin_host;

pub use engine::{
    AudioClip, AutomationLane, AutomationPoint, InsertSlotSnapshot, MicInputRing, MidiClip,
    MidiNote, ProjectSnapshot, SendSlot, StudioMobileEngine, TelemetrySnapshot, TrackKind,
    TrackMeterFrame, TrackSnapshot,
};
pub use plugin_host::{
    plugin_catalog, BuiltinPluginInstance, PluginCatalogEntry, PluginKind, PluginMeterSnapshot,
    PluginParamSpec,
};

#[cfg(test)]
mod alloc_counter {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    pub struct CountingAllocator;

    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ENABLED.try_with(|enabled| {
                if enabled.get() {
                    let _ = ALLOC_COUNT.try_with(|c| c.set(c.get() + 1));
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = ENABLED.try_with(|enabled| {
                if enabled.get() {
                    let _ = ALLOC_COUNT.try_with(|c| c.set(c.get() + 1));
                }
            });
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    pub fn measure_allocs<F: FnOnce()>(f: F) -> usize {
        ENABLED.with(|e| e.set(false));
        ALLOC_COUNT.with(|c| c.set(0));
        ENABLED.with(|e| e.set(true));
        f();
        ENABLED.with(|e| e.set(false));
        ALLOC_COUNT.with(|c| c.get())
    }
}

#[cfg(test)]
#[global_allocator]
static GLOBAL_ALLOC: alloc_counter::CountingAllocator = alloc_counter::CountingAllocator;

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

    #[test]
    fn audio_callbacks_never_block_when_control_thread_holds_engine_lock() {
        let handle = ffi::fb_engine_new(48_000);
        assert!(!handle.is_null());

        unsafe {
            ffi::fb_engine_set_playing(handle, true);
            ffi::fb_engine_set_recording(handle, true);
        }

        let mut out = vec![1.0f32; 512 * 2];
        let mic_block = vec![0.25f32; 512 * 2];

        // Control thread holds the engine mutex while both audio callbacks fire.
        let control_guard = unsafe { (*handle).lock() };
        let t0 = std::time::Instant::now();
        unsafe {
            ffi::fb_engine_render_f32(handle, out.as_mut_ptr(), 512);
            ffi::fb_engine_push_mic_input(handle, mic_block.as_ptr(), mic_block.len() as u32, 2);
            ffi::fb_engine_push_mic_input(handle, mic_block.as_ptr(), mic_block.len() as u32, 2);
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(10),
            "Audio callbacks must not block on the control mutex"
        );
        assert!(
            out.iter().all(|&s| s == 0.0),
            "Locked render callback must output silence"
        );
        assert_eq!(unsafe { (*handle).lock_misses() }, 1);
        drop(control_guard);

        // Finishing recording commits the mic samples pushed while the control lock was held.
        unsafe {
            ffi::fb_engine_set_recording(handle, false);
        }
        let snap = unsafe { (*handle).lock().snapshot() };
        let audio_track = snap
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio)
            .expect("audio track present");
        assert!(
            audio_track
                .audio_clips
                .iter()
                .any(|c| c.source_path == "recorded://mic_take.wav"),
            "Expected recorded clip from lock-free mic ring"
        );

        unsafe {
            ffi::fb_engine_free(handle);
        }
    }

    #[test]
    fn mic_recording_input_callback_is_allocation_free_and_bounded() {
        let mut engine = StudioMobileEngine::new(48_000);
        engine.set_recording(true);

        let block = vec![0.35f32; 256 * 2];
        // Push many blocks across the realtime input entry point; must never allocate on this thread.
        let allocs = alloc_counter::measure_allocs(|| {
            for _ in 0..120 {
                engine.push_mic_input_interleaved(&block, 2);
            }
        });
        assert_eq!(
            allocs, 0,
            "push_mic_input_interleaved allocated {allocs} times on the input thread"
        );

        engine.set_recording(false);
        let snap = engine.snapshot();
        let rec_clip = snap
            .tracks
            .iter()
            .flat_map(|t| &t.audio_clips)
            .find(|c| c.source_path == "recorded://mic_take.wav")
            .expect("recorded clip created off the input thread");
        let pool_buf = engine
            .audio_pool
            .get(&rec_clip.source_id)
            .expect("recorded buffer in pool");
        assert_eq!(pool_buf.frames, 120 * 256);

        // Verify the SPSC ring itself is strictly bounded when not drained.
        let ring = MicInputRing::new(1024);
        ring.set_active(true);
        let accepted = ring.push_interleaved(&vec![0.5f32; 1500 * 2], 2);
        assert_eq!(accepted, 1024);
        assert_eq!(ring.dropped_frames(), 476);
    }

    #[test]
    fn render_interleaved_stereo_performs_zero_heap_allocations_with_buses_midi_and_binaural() {
        let mut engine = StudioMobileEngine::new(48_000);
        let bus2_id = engine.add_track("Aux Bus 2", TrackKind::Bus, None);
        engine.set_track_send("track-1", &bus2_id, -6.0, true);
        engine.set_track_spatial("track-2", -0.4, 0.8, 0.2, true);
        engine.set_track_spatial("bus-1", 0.3, 0.7, 0.3, true);
        engine.master_binaural_enabled = true;
        engine.metronome_enabled = true;
        engine.set_playing(true);

        let mut out = vec![0.0f32; 512 * 2];
        // Warm up one block
        engine.trigger_live_midi("track-2", 60, 100, true);
        engine.render_interleaved_stereo(&mut out, 512);

        // Queue live MIDI events before measuring render allocations
        engine.trigger_live_midi("track-2", 64, 110, true);
        engine.trigger_live_midi("track-2", 60, 0, false);

        let allocs = alloc_counter::measure_allocs(|| {
            for _ in 0..8 {
                engine.render_interleaved_stereo(&mut out, 512);
            }
        });
        assert_eq!(
            allocs, 0,
            "Expected 0 heap allocations during steady-state render_interleaved_stereo, got {allocs}"
        );
    }
}
