//! BuiltinAudioPlugins — umbrella for Futureboard stock DSP cores.
//!
//! These processors are intentionally **engine-agnostic**. Do not depend on
//! `SphereDirectAudioEngine` here; integrate later through the host / insert
//! façade (`SphereAudioPlugins` or a dedicated bridge).
//!
//! ## Phase map
//! - Phase 1 (easy): `equz8`, `compresser`, `fa2a`
//! - Phase 2 (medium): `echospace`, `fa76`
//! - Phase 3 (hard): `c1073`, `meowsyn`

/// BuildInHelper: embedded UI asset infrastructure shared by every Built-in
/// Plugin dynamic library.
///
/// Re-exported from its own dependency-free crate so plugin `build.rs` files can
/// depend on the generator without dragging the DSP crates (and their `cdylib`
/// outputs) into a second build unit — see [`builtin_ui_embed`] for why.
pub use builtin_ui_embed as ui;

pub use builtin_dsp_core as core;

pub use burnlimit;
pub use c1073;
pub use clipper67;
pub use compresser;
pub use drumsampler;
pub use echospace;
pub use equz8;
pub use equzx;
pub use fa2a;
pub use fa76;
pub use meowsyn;
pub use mixstation;
pub use transient;
pub use wrapsynth;
pub use zcomp;

use builtin_dsp_core::PluginDescriptor;

/// Descriptors for the implemented focus set (integration can register these later).
pub fn focus_descriptors() -> Vec<PluginDescriptor> {
    vec![
        equz8::descriptor(),
        equzx::descriptor(),
        compresser::descriptor(),
        fa2a::descriptor(),
        echospace::descriptor(),
        fa76::descriptor(),
        burnlimit::descriptor(),
        clipper67::descriptor(),
        transient::descriptor(),
        c1073::descriptor(),
        meowsyn::descriptor(),
        wrapsynth::descriptor(),
        drumsampler::descriptor(),
        zcomp::descriptor(),
        mixstation::descriptor(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use builtin_dsp_core::{Instrument, StereoEffect};

    #[test]
    fn focus_set_is_complete() {
        let ids: Vec<_> = focus_descriptors().into_iter().map(|d| d.id).collect();
        assert!(ids.contains(&equz8::PLUGIN_ID));
        assert!(ids.contains(&equzx::PLUGIN_ID));
        assert!(ids.contains(&compresser::PLUGIN_ID));
        assert!(ids.contains(&fa2a::PLUGIN_ID));
        assert!(ids.contains(&echospace::PLUGIN_ID));
        assert!(ids.contains(&fa76::PLUGIN_ID));
        assert!(ids.contains(&burnlimit::PLUGIN_ID));
        assert!(ids.contains(&clipper67::PLUGIN_ID));
        assert!(ids.contains(&transient::PLUGIN_ID));
        assert!(ids.contains(&c1073::PLUGIN_ID));
        assert!(ids.contains(&meowsyn::PLUGIN_ID));
        assert!(ids.contains(&wrapsynth::PLUGIN_ID));
        assert!(ids.contains(&drumsampler::PLUGIN_ID));
        assert!(ids.contains(&zcomp::PLUGIN_ID));
        assert!(ids.contains(&mixstation::PLUGIN_ID));
    }

    #[test]
    fn effects_process_finite() {
        let mut eq = equz8::Dsp::new(48_000.0);
        let mut comp = compresser::Dsp::new(48_000.0);
        let mut optical = fa2a::Dsp::new(48_000.0);
        let mut delay = echospace::Dsp::new(48_000.0);
        let mut fet = fa76::Dsp::new(48_000.0);
        let mut channel = c1073::Dsp::new(48_000.0);
        let mut ultimate = zcomp::Dsp::new(48_000.0);
        let mut strip = mixstation::Dsp::new(48_000.0);

        for dsp in [
            &mut eq as &mut dyn StereoEffect,
            &mut comp,
            &mut optical,
            &mut delay,
            &mut fet,
            &mut channel,
            &mut ultimate,
            &mut strip,
        ] {
            let (l, r) = dsp.process_stereo(0.2, -0.1);
            assert!(l.is_finite() && r.is_finite());
        }
    }

    #[test]
    fn meowsyn_instrument_smoke() {
        let mut syn = meowsyn::Dsp::new(48_000.0);
        syn.note_on(60, 100);
        let (l, r) = syn.process_stereo();
        assert!(l.is_finite() && r.is_finite());
    }

    #[test]
    fn wrapsynth_instrument_smoke() {
        let mut synth = wrapsynth::Dsp::new(48_000.0);
        synth.note_on(60, 100);
        let (left, right) = synth.process_stereo();
        assert!(left.is_finite() && right.is_finite());
    }

    #[test]
    fn drumsampler_instrument_smoke() {
        let mut sampler = drumsampler::Dsp::new(48_000.0);
        sampler.note_on(36, 100);
        let (left, right) = sampler.process_stereo();
        assert!(left.is_finite() && right.is_finite());
    }
}
