//! The Futureboard built-in effects, run inside the engine.
//!
//! Studio runs these in its plug-in host process; LiveStage runs them on its
//! own audio thread — no bridge latency, and the only effects a headless
//! server has. They are addressed the same way Studio addresses them: by
//! catalog stem, with parameters as wire values (`apply_wire_param`), so a
//! built-in editor that drives Studio drives LiveStage unchanged.

use builtin_dsp_core::StereoEffect;

use crate::telemetry::{IMAGE_BANDS, LevelFrame, PITCH_SLOTS, REDUCTION_BANDS, SCOPE_SAMPLES};

/// One built-in effect LiveStage can load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinEffectInfo {
    pub stem: &'static str,
    pub name: &'static str,
    pub category: &'static str,
}

/// Every built-in effect, in the order the rack offers them. The stems and
/// names are Studio's catalog's (`SpherePluginHost::builtin`).
pub const BUILTIN_EFFECTS: &[BuiltinEffectInfo] = &[
    BuiltinEffectInfo {
        stem: "equz8",
        name: "EQ-Z8",
        category: "EQ",
    },
    BuiltinEffectInfo {
        stem: "equzx",
        name: "EQ-ZX",
        category: "EQ",
    },
    BuiltinEffectInfo {
        stem: "compresser",
        name: "Compressor",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "fa2a",
        name: "FA-2A",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "fa76",
        name: "FA-76",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "zcomp",
        name: "Z-Comp",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "transient",
        name: "Transient",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "waygate",
        name: "WayGate",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "clipper67",
        name: "67Clipper",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "burnlimit",
        name: "BurnLimit",
        category: "Dynamics",
    },
    BuiltinEffectInfo {
        stem: "mixstation",
        name: "MixStation",
        category: "Channel Strip",
    },
    BuiltinEffectInfo {
        stem: "echospace",
        name: "EchoSpace",
        category: "Delay",
    },
    BuiltinEffectInfo {
        stem: "verbspace",
        name: "VerbSpace",
        category: "Reverb",
    },
    BuiltinEffectInfo {
        stem: "imager",
        name: "Imager",
        category: "Utility",
    },
    BuiltinEffectInfo {
        stem: "whitesharp",
        name: "WhiteSharp",
        category: "Pitch",
    },
    BuiltinEffectInfo {
        stem: "rodharerist",
        name: "Rodhareist",
        category: "Multi-FX",
    },
];

pub fn effect_info(stem: &str) -> Option<&'static BuiltinEffectInfo> {
    BUILTIN_EFFECTS.iter().find(|info| info.stem == stem)
}

pub fn display_name(stem: &str) -> Option<&'static str> {
    effect_info(stem).map(|info| info.name)
}

/// Wire values a *new* insert of `stem` starts with on stage, where they
/// differ from the effect's own defaults: written into the insert's stored
/// params when it is added, so every editor shows them and a session saved
/// before keeps what it had. WhiteSharp starts on its live path — a singer
/// hears it in the monitors, where its Quality delay would be heard.
/// 67Clipper starts at 1× oversampling: LiveStage compensates no delay, and
/// Clip mode at 1× has none.
pub fn stage_defaults(stem: &str) -> &'static [(u32, f32)] {
    const WHITESHARP: &[(u32, f32)] = &[(
        whitesharp::ipc::LATENCY_INDEX,
        whitesharp::LatencyMode::Live.to_wire(),
    )];
    const CLIPPER67: &[(u32, f32)] = &[(
        clipper67::ipc::OVERSAMPLING_INDEX,
        clipper67::Oversampling::X1.to_wire(),
    )];
    match stem {
        "whitesharp" => WHITESHARP,
        "clipper67" => CLIPPER67,
        _ => &[],
    }
}

/// One parameter of a built-in effect, addressed by its wire index: what a
/// remote control without the native editor needs to draw a control for it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct BuiltinParam {
    pub index: u32,
    pub id: &'static str,
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// "dB", "ms", "%", … or "bool" / "enum" for switches and steps.
    pub unit: &'static str,
}

/// The parameters of built-in `stem`, in wire order. Taken from the effect's
/// own descriptor, each resolved to its wire index through the effect's id
/// table; a descriptor entry with no wire index is left out.
pub fn builtin_params(stem: &str) -> Vec<BuiltinParam> {
    macro_rules! params_of {
        ($descriptor:path, $index_of:path) => {{
            let mut params: Vec<BuiltinParam> = $descriptor()
                .params
                .iter()
                .filter_map(|p| {
                    $index_of(p.id).map(|index| BuiltinParam {
                        index,
                        id: p.id,
                        name: p.name,
                        min: p.min,
                        max: p.max,
                        default: p.default_value,
                        unit: p.unit,
                    })
                })
                .collect();
            params.sort_by_key(|p| p.index);
            params
        }};
    }
    match stem {
        "equz8" => params_of!(equz8::descriptor, equz8::ipc::ui_param_index),
        "equzx" => params_of!(equzx::descriptor, equzx::ipc::ui_param_index),
        "compresser" => params_of!(compresser::descriptor, compresser::ipc::ui_param_index),
        "fa2a" => params_of!(fa2a::descriptor, fa2a::ipc::ui_param_index),
        "fa76" => params_of!(fa76::descriptor, fa76::ipc::ui_param_index),
        "zcomp" => params_of!(zcomp::descriptor, zcomp::ipc::ui_param_index),
        "transient" => params_of!(transient::descriptor, transient::ipc::ui_param_index),
        "waygate" => params_of!(waygate::descriptor, waygate::ipc::ui_param_index),
        "clipper67" => params_of!(clipper67::descriptor, clipper67::ipc::ui_param_index),
        "burnlimit" => params_of!(burnlimit::descriptor, burnlimit::ipc::ui_param_index),
        "mixstation" => params_of!(mixstation::descriptor, mixstation::ipc::ui_param_index),
        "echospace" => params_of!(echospace::descriptor, echospace::ipc::ui_param_index),
        "verbspace" => params_of!(verbspace::descriptor, verbspace::ipc::ui_param_index),
        "imager" => params_of!(imager::descriptor, imager::ipc::ui_param_index),
        "whitesharp" => params_of!(whitesharp::descriptor, whitesharp::ipc::ui_param_index),
        "rodharerist" => params_of!(rodharerist::descriptor, rodharerist::ui_param_index),
        _ => Vec::new(),
    }
}

/// One factory preset: every wire value, by index.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BuiltinPreset {
    pub name: &'static str,
    pub values: Vec<f32>,
}

/// What an editor needs beyond the descriptor: every wire id (a parameter's
/// index is its position), every default as the DSP itself starts, and the
/// factory presets. The same tables Studio's native editors work from.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BuiltinSpec {
    pub ids: Vec<&'static str>,
    pub defaults: Vec<f32>,
    pub presets: Vec<BuiltinPreset>,
}

pub fn builtin_spec(stem: &str) -> Option<BuiltinSpec> {
    /// `ui_values` as a vector by wire index.
    fn by_index(ids: &[&'static str], values: &[(&'static str, f32)]) -> Vec<f32> {
        let mut out = vec![0.0; ids.len()];
        for (id, value) in values {
            if let Some(index) = ids.iter().position(|candidate| candidate == id) {
                out[index] = *value;
            }
        }
        out
    }
    macro_rules! spec_of {
        ($krate:ident, $ids:path, $values:path) => {{
            let ids: Vec<&'static str> = $ids.iter().copied().collect();
            let defaults = by_index(&ids, &$values(&$krate::default_params()));
            let presets = $krate::presets::factory_presets()
                .iter()
                .map(|preset| BuiltinPreset {
                    name: preset.name,
                    values: by_index(&ids, &$values(&preset.params)),
                })
                .collect();
            BuiltinSpec {
                ids,
                defaults,
                presets,
            }
        }};
    }
    Some(match stem {
        "equz8" => spec_of!(equz8, equz8::ipc::UI_PARAM_IDS, equz8::ipc::ui_values),
        // EQ-ZX ships no factory presets.
        "equzx" => {
            let ids: Vec<&'static str> = equzx::ipc::UI_PARAM_IDS.iter().copied().collect();
            let defaults = by_index(&ids, &equzx::ipc::ui_values(&equzx::default_params()));
            BuiltinSpec {
                ids,
                defaults,
                presets: Vec::new(),
            }
        }
        "compresser" => spec_of!(
            compresser,
            compresser::ipc::UI_PARAM_IDS,
            compresser::ipc::ui_values
        ),
        "fa2a" => spec_of!(fa2a, fa2a::ipc::UI_PARAM_IDS, fa2a::ipc::ui_values),
        "fa76" => spec_of!(fa76, fa76::ipc::UI_PARAM_IDS, fa76::ipc::ui_values),
        "zcomp" => spec_of!(zcomp, zcomp::ipc::UI_PARAM_IDS, zcomp::ipc::ui_values),
        "transient" => spec_of!(
            transient,
            transient::ipc::UI_PARAM_IDS,
            transient::ipc::ui_values
        ),
        "waygate" => spec_of!(waygate, waygate::ipc::UI_PARAM_IDS, waygate::ipc::ui_values),
        "clipper67" => spec_of!(
            clipper67,
            clipper67::ipc::UI_PARAM_IDS,
            clipper67::ipc::ui_values
        ),
        "burnlimit" => spec_of!(
            burnlimit,
            burnlimit::ipc::UI_PARAM_IDS,
            burnlimit::ipc::ui_values
        ),
        // MixStation's `ui_values` is already by wire index.
        "mixstation" => {
            let ids: Vec<&'static str> = mixstation::ipc::UI_PARAM_IDS.iter().copied().collect();
            let defaults = mixstation::ipc::ui_values(&mixstation::default_params()).to_vec();
            let presets = mixstation::presets::factory_presets()
                .iter()
                .map(|preset| BuiltinPreset {
                    name: preset.name,
                    values: mixstation::ipc::ui_values(&preset.params).to_vec(),
                })
                .collect();
            BuiltinSpec {
                ids,
                defaults,
                presets,
            }
        }
        "echospace" => spec_of!(
            echospace,
            echospace::ipc::UI_PARAM_IDS,
            echospace::ipc::ui_values
        ),
        "verbspace" => spec_of!(
            verbspace,
            verbspace::ipc::UI_PARAM_IDS,
            verbspace::ipc::ui_values
        ),
        "imager" => spec_of!(imager, imager::ipc::UI_PARAM_IDS, imager::ipc::ui_values),
        "whitesharp" => spec_of!(
            whitesharp,
            whitesharp::ipc::UI_PARAM_IDS,
            whitesharp::ipc::ui_values
        ),
        "rodharerist" => spec_of!(
            rodharerist,
            rodharerist::UI_PARAM_IDS,
            rodharerist::ui_values
        ),
        _ => return None,
    })
}

/// A built-in effect's DSP. One variant per crate rather than a boxed trait
/// object: each is built once per insert, and the match is one predictable
/// branch per block.
#[allow(clippy::large_enum_variant)]
pub enum BuiltinFx {
    Equz8(equz8::Dsp),
    Equzx(equzx::Dsp),
    Compresser(compresser::Dsp),
    Fa2a(fa2a::Dsp),
    Fa76(fa76::Dsp),
    Zcomp(zcomp::Dsp),
    Transient(transient::Dsp),
    WayGate(waygate::Dsp),
    Clipper67(clipper67::Dsp),
    BurnLimit(burnlimit::Dsp),
    MixStation(mixstation::Dsp),
    Echospace(echospace::Dsp),
    Verbspace(verbspace::Dsp),
    Imager(imager::Dsp),
    WhiteSharp(Box<whitesharp::Dsp>),
    Rodhareist(Box<rodharerist::Dsp>),
}

/// Runs `$body` with `$dsp` bound to whichever core `$fx` holds.
macro_rules! with_dsp {
    ($fx:expr, $dsp:ident => $body:expr) => {
        match $fx {
            BuiltinFx::Equz8($dsp) => $body,
            BuiltinFx::Equzx($dsp) => $body,
            BuiltinFx::Compresser($dsp) => $body,
            BuiltinFx::Fa2a($dsp) => $body,
            BuiltinFx::Fa76($dsp) => $body,
            BuiltinFx::Zcomp($dsp) => $body,
            BuiltinFx::Transient($dsp) => $body,
            BuiltinFx::WayGate($dsp) => $body,
            BuiltinFx::Clipper67($dsp) => $body,
            BuiltinFx::BurnLimit($dsp) => $body,
            BuiltinFx::MixStation($dsp) => $body,
            BuiltinFx::Echospace($dsp) => $body,
            BuiltinFx::Verbspace($dsp) => $body,
            BuiltinFx::Imager($dsp) => $body,
            BuiltinFx::WhiteSharp($dsp) => $body,
            BuiltinFx::Rodhareist($dsp) => $body,
        }
    };
}

impl BuiltinFx {
    /// The effect for catalog `stem` at `sample_rate`, at its defaults.
    /// `None` for a stem that is not a built-in effect.
    pub fn new(stem: &str, sample_rate: u32) -> Option<Self> {
        let sr = sample_rate.max(1) as f32;
        Some(match stem {
            "equz8" => Self::Equz8(equz8::Dsp::new(sr)),
            "equzx" => Self::Equzx(equzx::Dsp::new(sr)),
            "compresser" => Self::Compresser(compresser::Dsp::new(sr)),
            "fa2a" => Self::Fa2a(fa2a::Dsp::new(sr)),
            "fa76" => Self::Fa76(fa76::Dsp::new(sr)),
            "zcomp" => Self::Zcomp(zcomp::Dsp::new(sr)),
            "transient" => Self::Transient(transient::Dsp::new(sr)),
            "waygate" => Self::WayGate(waygate::Dsp::new(sr)),
            "clipper67" => Self::Clipper67(clipper67::Dsp::new(sr)),
            "burnlimit" => Self::BurnLimit(burnlimit::Dsp::new(sr)),
            "mixstation" => Self::MixStation(mixstation::Dsp::new(sr)),
            "echospace" => Self::Echospace(echospace::Dsp::new(sr)),
            "verbspace" => Self::Verbspace(verbspace::Dsp::new(sr)),
            "imager" => Self::Imager(imager::Dsp::new(sr)),
            "whitesharp" => Self::WhiteSharp(Box::new(whitesharp::Dsp::new(sr))),
            "rodharerist" => Self::Rodhareist(Box::new(rodharerist::Dsp::new(sr))),
            _ => return None,
        })
    }

    /// Set wire parameter `index` to `value`. Out-of-range indices are
    /// ignored. Audio thread (between blocks) or before the effect is
    /// published.
    pub fn apply_wire_param(&mut self, index: u32, value: f32) {
        match self {
            // Rodhareist's wire table names its parameters instead.
            Self::Rodhareist(dsp) => {
                if let Some(id) = rodharerist::ui_param_id(index) {
                    let _ = dsp.apply_ui_param(id, value);
                }
            }
            Self::Equz8(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Equzx(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Compresser(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Fa2a(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Fa76(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Zcomp(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Transient(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::WayGate(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Clipper67(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::BurnLimit(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::MixStation(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Echospace(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Verbspace(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::Imager(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
            Self::WhiteSharp(dsp) => {
                let _ = dsp.apply_wire_param(index, value);
            }
        }
    }

    /// Process one block in place. Audio thread; allocation-free.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if let Self::Rodhareist(dsp) = self {
            // A captured amp or cabinet swaps in at a block boundary only.
            dsp.begin_block();
        }
        with_dsp!(self, dsp => {
            for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                let (out_l, out_r) = dsp.process_stereo(*l, *r);
                *l = out_l;
                *r = out_r;
            }
        })
    }

    pub fn reset(&mut self) {
        with_dsp!(self, dsp => dsp.reset())
    }

    /// The effect's own level meters, as Studio's plug-in host maps them;
    /// `None` for one that does not meter itself. Audio thread.
    pub fn level_frame(&self) -> Option<LevelFrame> {
        // `$f` names the DSP's frame inside `$reduction`.
        macro_rules! frame {
            ($frame:expr, $f:ident => $reduction:expr) => {{
                let $f = $frame;
                LevelFrame {
                    in_peak: $f.in_peak,
                    in_rms: $f.in_rms,
                    out_peak: $f.out_peak,
                    out_rms: $f.out_rms,
                    gain_reduction_db: $reduction,
                    in_clip: $f.in_clip,
                    out_clip: $f.out_clip,
                    ..LevelFrame::default()
                }
            }};
        }
        Some(match self {
            Self::Fa2a(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            Self::Fa76(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            Self::Zcomp(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            // Shaping goes either way; the frame carries its size.
            Self::Transient(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            // The gate's attenuation as reduction; a single stage, so the
            // rack blocks carry the key and the detector's state
            // (`waygate::KEY_SLOT`), as in Studio's host.
            Self::WayGate(dsp) => {
                let f = dsp.meter_frame();
                let (slot_in_peak, slot_out_peak) = f.rack_slots();
                LevelFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    slot_in_peak,
                    slot_out_peak,
                }
            }
            Self::Clipper67(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            Self::BurnLimit(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            // Multiband: the largest band's; the bands come apart in
            // `band_reduction`.
            Self::Compresser(dsp) => frame!(dsp.meter_frame(), f => f.gain_reduction_db),
            // Width only moves energy between mid and side: nothing is taken off.
            Self::Imager(dsp) => frame!(dsp.meter_frame(), _f => 0.0),
            // The chain's compressor stage is not reported on its own.
            Self::Rodhareist(dsp) => frame!(dsp.meter_frame(), _f => 0.0),
            // The one built-in with a user-ordered rack: a level per position.
            Self::MixStation(dsp) => {
                let f = dsp.meter_frame();
                LevelFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    slot_in_peak: f.slot_in_peak,
                    slot_out_peak: f.slot_out_peak,
                }
            }
            Self::Equz8(_)
            | Self::Equzx(_)
            | Self::Echospace(_)
            | Self::Verbspace(_)
            | Self::WhiteSharp(_) => return None,
        })
    }

    /// Reduction per band, for the multiband Compressor (zeros in its single
    /// mode, so a stale multiband reading never lingers). Audio thread.
    pub fn band_reduction(&self) -> Option<[f32; REDUCTION_BANDS]> {
        match self {
            Self::Compresser(dsp) => Some(dsp.band_reduction_db()),
            _ => None,
        }
    }

    /// The Imager's stereo image, when it has finished a new one. Audio
    /// thread.
    pub fn take_image(&mut self) -> Option<ImageFrameRef> {
        match self {
            Self::Imager(dsp) => dsp.take_image_frame().map(|f| ImageFrameRef {
                correlation: f.correlation,
                band_correlation: f.band_correlation,
                band_level: f.band_level,
                scope: f.scope,
            }),
            _ => None,
        }
    }

    /// WhiteSharp's newest pitch readings. Audio thread.
    pub fn pitch_telemetry(&self) -> Option<[f32; PITCH_SLOTS]> {
        match self {
            Self::WhiteSharp(dsp) => Some(dsp.telemetry()),
            _ => None,
        }
    }
}

/// An image frame as the Imager hands it over: fixed arrays, nothing to
/// allocate on the audio thread.
pub struct ImageFrameRef {
    pub correlation: f32,
    pub band_correlation: [f32; IMAGE_BANDS],
    pub band_level: [f32; IMAGE_BANDS],
    pub scope: [f32; SCOPE_SAMPLES],
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new WhiteSharp on stage starts on its live path; its stage default
    /// is a real wire value of a real parameter.
    #[test]
    fn whitesharp_starts_live_on_stage() {
        let spec = builtin_spec("whitesharp").expect("whitesharp spec");
        for &(index, value) in stage_defaults("whitesharp") {
            assert!((index as usize) < spec.ids.len());
            assert_ne!(spec.defaults[index as usize], value, "not a default");
        }
        let mut dsp = whitesharp::Dsp::new(48_000.0);
        assert!(dsp.latency_samples() > 1_000, "Quality by itself");
        for &(index, value) in stage_defaults("whitesharp") {
            assert!(dsp.apply_wire_param(index, value));
        }
        assert_eq!(dsp.latency_samples(), whitesharp::splice::LATENCY);
        assert!(stage_defaults("fa76").is_empty());
    }

    /// Every catalog entry builds, processes a block to finite samples, and
    /// takes a wire parameter without panicking.
    #[test]
    fn every_builtin_effect_builds_and_processes() {
        for info in BUILTIN_EFFECTS {
            let mut fx = BuiltinFx::new(info.stem, 48_000)
                .unwrap_or_else(|| panic!("{} did not build", info.stem));
            fx.apply_wire_param(0, 0.5);
            fx.apply_wire_param(u32::MAX, 1.0);
            let mut left: Vec<f32> = (0..256).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
            let mut right = left.clone();
            fx.process(&mut left, &mut right);
            assert!(
                left.iter().chain(right.iter()).all(|s| s.is_finite()),
                "{} produced a non-finite sample",
                info.stem
            );
        }
        assert!(BuiltinFx::new("not-an-effect", 48_000).is_none());
    }

    /// With no delay compensation on stage, a new 67Clipper starts at zero
    /// latency (Studio's default oversamples, and reports its delay).
    #[test]
    fn the_clipper_starts_without_latency_on_stage() {
        let Some(BuiltinFx::Clipper67(mut dsp)) = BuiltinFx::new("clipper67", 48_000) else {
            panic!("clipper67 builds");
        };
        assert!(dsp.latency_samples() > 0, "4× by itself");
        let spec = builtin_spec("clipper67").expect("spec");
        for &(index, value) in stage_defaults("clipper67") {
            assert_eq!(spec.ids[index as usize], "oversampling");
            assert_ne!(spec.defaults[index as usize], value, "not a default");
            assert!(dsp.apply_wire_param(index, value));
        }
        assert_eq!(dsp.latency_samples(), 0);
    }

    /// Every effect describes its parameters, each on a distinct wire index
    /// with a sane range.
    #[test]
    fn every_builtin_effect_lists_its_parameters() {
        for info in BUILTIN_EFFECTS {
            let params = builtin_params(info.stem);
            assert!(!params.is_empty(), "{} lists no parameters", info.stem);
            for pair in params.windows(2) {
                assert!(
                    pair[0].index < pair[1].index,
                    "{} repeats a wire index",
                    info.stem
                );
            }
            for p in &params {
                assert!(p.min <= p.max, "{} {} has min > max", info.stem, p.id);
            }
        }
        assert!(builtin_params("not-an-effect").is_empty());
    }

    /// Every effect's wire table, defaults and presets line up: one value
    /// per id, every one finite, and every descriptor id in the table.
    #[test]
    fn every_builtin_effect_has_a_consistent_spec() {
        for info in BUILTIN_EFFECTS {
            let spec =
                builtin_spec(info.stem).unwrap_or_else(|| panic!("{} has no spec", info.stem));
            assert!(!spec.ids.is_empty(), "{}", info.stem);
            assert_eq!(
                spec.defaults.len(),
                spec.ids.len(),
                "{} defaults",
                info.stem
            );
            assert!(spec.defaults.iter().all(|v| v.is_finite()), "{}", info.stem);
            for preset in &spec.presets {
                assert_eq!(
                    preset.values.len(),
                    spec.ids.len(),
                    "{} {}",
                    info.stem,
                    preset.name
                );
            }
            for param in builtin_params(info.stem) {
                assert_eq!(spec.ids[param.index as usize], param.id, "{}", info.stem);
            }
        }
        assert!(builtin_spec("not-an-effect").is_none());
    }

    /// WayGate's frame: shut, it reports its range as reduction; open, the
    /// key and the detector's state ride the rack blocks the editors read.
    #[test]
    fn waygate_reports_its_range_key_and_state() {
        let mut fx = BuiltinFx::new("waygate", 48_000).unwrap();
        fx.apply_wire_param(waygate::ipc::RANGE_INDEX, -30.0);
        let mut left = vec![0.001f32; 256];
        let mut right = left.clone();
        fx.process(&mut left, &mut right);
        let shut = fx.level_frame().expect("the gate meters itself");
        assert!((shut.gain_reduction_db - 30.0).abs() < 1.0e-3);
        assert_eq!(shut.slot_out_peak[waygate::KEY_SLOT], 0.0);

        let mut left = vec![0.5f32; 256];
        let mut right = left.clone();
        fx.process(&mut left, &mut right);
        let open = fx.level_frame().expect("the gate meters itself");
        assert_eq!(open.gain_reduction_db, 0.0);
        assert_eq!(open.slot_out_peak[waygate::KEY_SLOT], 1.0);
        assert!(open.slot_in_peak[waygate::KEY_SLOT] > 0.4);
        assert_eq!(left[255], 0.5);
    }
}
