//! The Futureboard built-in effects, run inside the engine.
//!
//! Studio runs these in its plug-in host process; LiveStage runs them on its
//! own audio thread — no bridge latency, and the only effects a headless
//! server has. They are addressed the same way Studio addresses them: by
//! catalog stem, with parameters as wire values (`apply_wire_param`), so a
//! built-in editor that drives Studio drives LiveStage unchanged.

use builtin_dsp_core::StereoEffect;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
