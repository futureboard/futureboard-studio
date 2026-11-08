//! Rodhareist — flagship guitar multi-effect (DSP core).
//!
//! Engine-agnostic like the other `BuiltinAudioPlugins` cores: it exposes a
//! realtime-safe [`StereoEffect`](builtin_dsp_core::StereoEffect) chain
//! (`Gate → Drive → Amp → Mod → Delay → Reverb → Cabinet`) plus the metadata
//! and parameter model the React editor drives. Host/bridge wiring (C entry
//! points, embedded-UI table) is layered on separately.

mod dsp;
pub mod presets;
mod state;
pub mod ui;
mod wire;

pub use dsp::{
    AmpModel, CabModel, DelayModel, DriveModel, Dsp, EqModel, IR_PARTITION_SAMPLES, IrInfo,
    IrLoadError, IrLoader, Line2Mode, MAX_IR_SECONDS, MicModel, ModModel, NAM_BLOCK_SAMPLES,
    NamCaptureInfo, NamLoadError, NamLoader, PATH_SLOTS, PLUGIN_ID, Params, PreparedIrRuntime,
    PreparedNamRuntime, Retired, ReverbModel, RowSend, StageBParams, StageChannel, StageKind,
    ToneEngineKind, WahModel, apply_to_params, default_params, descriptor, prepare_ir_runtime,
    prepare_nam_runtime, reorder_chain, telemetry, ui_values,
};
pub use presets::{FactoryPreset, factory_presets};
pub use state::{RodhareistState, SCHEMA_VERSION};
pub use wire::{UI_PARAM_IDS, ui_param_id, ui_param_index};

#[cfg(test)]
mod tests {
    use super::*;
    use builtin_dsp_core::StereoEffect;

    #[test]
    fn descriptor_is_effect_and_ids_are_unique() {
        let d = descriptor();
        assert_eq!(d.id, PLUGIN_ID);
        assert_eq!(d.category, builtin_dsp_core::PluginCategory::Effect);
        let mut ids: Vec<_> = d.params.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(unique, ids.len(), "duplicate parameter id in descriptor");
    }

    /// The descriptor is what a host shows in its own parameter list and what
    /// its "reset to default" writes back, so a default that disagrees with
    /// [`dsp::default_params`] means the DAW displays one value while the
    /// plugin loads another. Retuning the amp left five of these behind and
    /// nothing caught it.
    #[test]
    fn descriptor_defaults_match_the_plugin_defaults() {
        let defaults = dsp::ui_values(&dsp::default_params());
        for param in descriptor().params {
            let Some(&(_, actual)) = defaults.iter().find(|(id, _)| *id == param.id) else {
                continue;
            };
            assert!(
                (param.default_value - actual).abs() < 1.0e-6,
                "`{}`: descriptor says {}, default_params() says {actual}",
                param.id,
                param.default_value,
            );
            assert!(
                param.default_value >= param.min && param.default_value <= param.max,
                "`{}`: default {} is outside {}..{}",
                param.id,
                param.default_value,
                param.min,
                param.max,
            );
        }
    }

    #[test]
    fn processes_finite_at_multiple_rates() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0] {
            let mut dsp = Dsp::new(sr);
            for n in 0..1_000 {
                let x = (n as f32 * 0.02).sin() * 0.4;
                let (l, r) = dsp.process_stereo(x, x);
                assert!(l.is_finite() && r.is_finite());
            }
        }
    }

    #[test]
    fn reset_clears_tails() {
        let mut dsp = Dsp::new(48_000.0);
        for _ in 0..1_000 {
            let _ = dsp.process_stereo(0.5, -0.5);
        }
        dsp.reset();
        let (l, r) = dsp.process_stereo(0.0, 0.0);
        assert!(l.abs() < 1.0e-3 && r.abs() < 1.0e-3);
    }
}

/// Counts heap hits on the thread that armed it, and separately the frees.
/// `const` thread-locals so the counter itself does not allocate on first
/// touch. A free while armed on the audio side means a model, or something
/// else, was dropped inside the callback.
#[cfg(test)]
pub(crate) mod alloc_probe {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct Counting;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u64> = const { Cell::new(0) };
        static FREES: Cell<u64> = const { Cell::new(0) };
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if ARMED.with(Cell::get) {
                COUNT.with(|c| c.set(c.get() + 1));
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if ARMED.with(Cell::get) {
                COUNT.with(|c| c.set(c.get() + 1));
            }
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if ARMED.with(Cell::get) {
                COUNT.with(|c| c.set(c.get() + 1));
            }
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if ARMED.with(Cell::get) {
                FREES.with(|c| c.set(c.get() + 1));
            }
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: Counting = Counting;

    pub(crate) fn reset() {
        COUNT.with(|c| c.set(0));
        FREES.with(|c| c.set(0));
    }

    pub(crate) fn arm() {
        ARMED.with(|c| c.set(true));
    }

    pub(crate) fn disarm() {
        ARMED.with(|c| c.set(false));
    }

    pub(crate) fn count() -> u64 {
        COUNT.with(Cell::get)
    }

    /// Heap frees made on this thread while armed.
    pub(crate) fn frees() -> u64 {
        FREES.with(Cell::get)
    }
}
