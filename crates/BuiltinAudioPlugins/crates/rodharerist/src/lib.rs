//! Rodhareist — flagship guitar multi-effect (DSP core).
//!
//! Engine-agnostic like the other `BuiltinAudioPlugins` cores: it exposes a
//! realtime-safe [`StereoEffect`](builtin_dsp_core::StereoEffect) chain
//! (`Gate → Drive → Amp → Mod → Delay → Reverb → Cabinet`) plus the metadata
//! and parameter model the React editor drives. Host/bridge wiring (C entry
//! points, embedded-UI table) is layered on separately.

mod dsp;
mod state;
pub mod ui;
mod wire;

pub use dsp::{
    AmpModel, CabModel, DelayModel, DriveModel, Dsp, EqModel, IR_PARTITION_SAMPLES, IrInfo,
    IrLoadError, IrLoader, MAX_IR_SECONDS, MicModel, ModModel, NAM_BLOCK_SAMPLES, NamCaptureInfo,
    NamLoadError, NamLoader, PATH_SLOTS, PLUGIN_ID, Params, PreparedIrRuntime, PreparedNamRuntime,
    ReverbModel, StageBParams, StageKind, ToneEngineKind, WahModel, apply_to_params,
    default_params, descriptor, prepare_ir_runtime, prepare_nam_runtime, ui_values,
};
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
