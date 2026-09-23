//! CEF host for built-in plugin editors.
//!
//! Built-in plugins ship their editor as a compiled React app embedded in the
//! plugin library. This module owns the browser-process side: the CEF runtime,
//! the `mikoplugin://` asset registry, and the child views parented into the
//! editor window shell.
//!
//! ## Availability
//!
//! Everything here is behind the `builtin-plugin-editor` feature. Without it,
//! [`availability`] reports why the editor cannot open, and the caller surfaces
//! that instead of showing an empty window. That is deliberate: a checkout with
//! no CEF SDK must still build and run.
//!
//! ## Threading
//!
//! CEF is initialized lazily on the GPUI UI thread and must only be driven from
//! there ([`CefRuntime`] enforces this). [`pump`] has to be called from the UI
//! loop or the browser never paints.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a built-in editor can or cannot be hosted right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAvailability {
    /// CEF is compiled in and the plugin has embedded UI assets.
    Ready,
    /// The binary was built without the `builtin-plugin-editor` feature.
    NotCompiledIn,
    /// The plugin id is not a built-in that ships an editor.
    NoEditorForPlugin(String),
    /// The plugin's editor `dist/` was not built when the library was compiled.
    UiNotEmbedded(String),
    /// CEF failed to start.
    RuntimeFailed(String),
}

impl fmt::Display for HostAvailability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => write!(f, "ready"),
            Self::NotCompiledIn => write!(
                f,
                "built-in plugin editors are not available in this build \
                 (rebuild with --features builtin-plugin-editor)"
            ),
            Self::NoEditorForPlugin(id) => write!(f, "{id} does not ship an editor UI"),
            Self::UiNotEmbedded(id) => write!(
                f,
                "{id} was compiled without its editor UI (run `bun run build` in its editor bundle)"
            ),
            Self::RuntimeFailed(err) => write!(f, "CEF failed to start: {err}"),
        }
    }
}

/// The `mikoplugin://` origin for a built-in plugin.
///
/// Accepts both identifier forms a built-in travels under: the catalog id
/// (`builtin:rodharerist`) and the class id (`rodharerist`) that insert slots
/// actually store. Resolution is validated against the built-in catalog, so an
/// unprefixed external class id is never treated as a built-in.
pub fn origin_for_plugin_id(plugin_id: &str) -> Option<&'static str> {
    SpherePluginHost::resolve_builtin_stem(plugin_id)
}

/// Map an editor's string param id to `plugin_id`'s u32 wire index (the id
/// carried by the shared param ring into the plugin-host process). `None` for
/// unknown plugins or ids — the caller logs and drops, never guesses.
#[cfg(feature = "builtin-plugin-editor")]
pub fn builtin_param_index(plugin_id: &str, param_id: &str) -> Option<u32> {
    match origin_for_plugin_id(plugin_id)? {
        rodharerist::ui::UI_ORIGIN => rodharerist::ui_param_index(param_id),
        equz8::ui::UI_ORIGIN => equz8::ui_param_index(param_id),
        verbspace::ui::UI_ORIGIN => verbspace::ui_param_index(param_id),
        echospace::ui::UI_ORIGIN => echospace::ui_param_index(param_id),
        fa2a::ui::UI_ORIGIN => fa2a::ui_param_index(param_id),
        fa76::ui::UI_ORIGIN => fa76::ui_param_index(param_id),
        burnlimit::ui::UI_ORIGIN => burnlimit::ui_param_index(param_id),
        clipper67::ui::UI_ORIGIN => clipper67::ui_param_index(param_id),
        transient::ui::UI_ORIGIN => transient::ui_param_index(param_id),
        wrapsynth::ui::UI_ORIGIN => wrapsynth::ui_param_index(param_id),
        zcomp::ui::UI_ORIGIN => zcomp::ui_param_index(param_id),
        mixstation::ui::UI_ORIGIN => mixstation::ui_param_index(param_id),
        _ => None,
    }
}

/// Without the editor feature no plugin table is linked in; every id is
/// unknown (and no editor exists to send one anyway).
#[cfg(not(feature = "builtin-plugin-editor"))]
pub fn builtin_param_index(_plugin_id: &str, _param_id: &str) -> Option<u32> {
    None
}

/// Authoritative main-process mirror of every built-in insert's parameter
/// state, keyed by insert slot id. The single source of truth for what a
/// built-in insert *is*: seeded from the project blob, updated by every
/// forwarded editor edit, read back for `selectInstance` state, project save,
/// and host restore/respawn replay. Process-wide `Mutex` like `INBOUND` —
/// touched only from the UI thread today, but nothing about it is
/// thread-bound.
#[cfg(feature = "builtin-plugin-editor")]
mod state_mirror {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use super::origin_for_plugin_id;

    /// One insert's mirrored params, tagged with the built-in that owns them.
    ///
    /// The map is keyed by insert slot id alone, and a slot can be re-pointed at
    /// a different plugin, so the tag is what proves a stored blob belongs to
    /// the plugin asking for it. Untagged, an EQ-Z8 edit would fold into
    /// rodharerist's parameter table and be written to the project as such.
    /// Boxed so a slot of the map stays small regardless of which DSP core has
    /// the largest `Params`.
    enum BuiltinParams {
        Rodhareist(Box<rodharerist::Params>),
        Equz8(Box<equz8::Params>),
        Verbspace(Box<verbspace::Params>),
        Echospace(Box<echospace::Params>),
        Fa2a(Box<fa2a::Params>),
        Fa76(Box<fa76::Params>),
        BurnLimit(Box<burnlimit::Params>),
        Clipper67(Box<clipper67::Params>),
        Transient(Box<transient::Params>),
        WrapSynth(Box<wrapsynth::Params>),
        Zcomp(Box<zcomp::Params>),
        MixStation(Box<mixstation::Params>),
    }

    impl BuiltinParams {
        fn origin(&self) -> &'static str {
            match self {
                Self::Rodhareist(_) => rodharerist::ui::UI_ORIGIN,
                Self::Equz8(_) => equz8::ui::UI_ORIGIN,
                Self::Verbspace(_) => verbspace::ui::UI_ORIGIN,
                Self::Echospace(_) => echospace::ui::UI_ORIGIN,
                Self::Fa2a(_) => fa2a::ui::UI_ORIGIN,
                Self::Fa76(_) => fa76::ui::UI_ORIGIN,
                Self::BurnLimit(_) => burnlimit::ui::UI_ORIGIN,
                Self::Clipper67(_) => clipper67::ui::UI_ORIGIN,
                Self::Transient(_) => transient::ui::UI_ORIGIN,
                Self::WrapSynth(_) => wrapsynth::ui::UI_ORIGIN,
                Self::Zcomp(_) => zcomp::ui::UI_ORIGIN,
                Self::MixStation(_) => mixstation::ui::UI_ORIGIN,
            }
        }

        /// The plugin's own defaults, or `None` for a built-in with no mirror.
        fn defaults(origin: &str) -> Option<Self> {
            match origin {
                rodharerist::ui::UI_ORIGIN => {
                    Some(Self::Rodhareist(Box::new(rodharerist::default_params())))
                }
                equz8::ui::UI_ORIGIN => Some(Self::Equz8(Box::new(equz8::default_params()))),
                verbspace::ui::UI_ORIGIN => {
                    Some(Self::Verbspace(Box::new(verbspace::default_params())))
                }
                echospace::ui::UI_ORIGIN => {
                    Some(Self::Echospace(Box::new(echospace::default_params())))
                }
                fa2a::ui::UI_ORIGIN => Some(Self::Fa2a(Box::new(fa2a::default_params()))),
                fa76::ui::UI_ORIGIN => Some(Self::Fa76(Box::new(fa76::default_params()))),
                burnlimit::ui::UI_ORIGIN => {
                    Some(Self::BurnLimit(Box::new(burnlimit::default_params())))
                }
                clipper67::ui::UI_ORIGIN => {
                    Some(Self::Clipper67(Box::new(clipper67::default_params())))
                }
                transient::ui::UI_ORIGIN => {
                    Some(Self::Transient(Box::new(transient::default_params())))
                }
                wrapsynth::ui::UI_ORIGIN => {
                    Some(Self::WrapSynth(Box::new(wrapsynth::default_params())))
                }
                zcomp::ui::UI_ORIGIN => Some(Self::Zcomp(Box::new(zcomp::default_params()))),
                mixstation::ui::UI_ORIGIN => {
                    Some(Self::MixStation(Box::new(mixstation::default_params())))
                }
                _ => None,
            }
        }
    }

    static BUILTIN_STATE: OnceLock<Mutex<HashMap<String, BuiltinParams>>> = OnceLock::new();

    fn map() -> &'static Mutex<HashMap<String, BuiltinParams>> {
        BUILTIN_STATE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// The entry for `insert_id`, created at `origin`'s defaults when absent —
    /// or when a *different* built-in left state behind in this slot.
    fn entry_for<'a>(
        states: &'a mut HashMap<String, BuiltinParams>,
        insert_id: &str,
        origin: &str,
    ) -> Option<&'a mut BuiltinParams> {
        if states.get(insert_id).map(BuiltinParams::origin) != Some(origin) {
            states.insert(insert_id.to_string(), BuiltinParams::defaults(origin)?);
        }
        states.get_mut(insert_id)
    }

    /// Fold one forwarded editor edit (wire index + raw value) into the
    /// insert's mirrored params. Creates the entry at defaults on first edit.
    /// `clear_clip` (an action, not state) is ignored.
    pub fn builtin_state_apply(plugin_id: &str, insert_id: &str, wire_index: u32, value: f32) {
        let Some(origin) = origin_for_plugin_id(plugin_id) else {
            return;
        };
        // Resolve the index *before* touching the map, so an unknown one never
        // materializes default state for an insert that had none.
        let known = match origin {
            rodharerist::ui::UI_ORIGIN => rodharerist::ui_param_id(wire_index).is_some(),
            equz8::ui::UI_ORIGIN => equz8::ui_param_id(wire_index).is_some(),
            verbspace::ui::UI_ORIGIN => verbspace::ui_param_id(wire_index).is_some(),
            echospace::ui::UI_ORIGIN => echospace::ui_param_id(wire_index).is_some(),
            fa2a::ui::UI_ORIGIN => fa2a::ui_param_id(wire_index).is_some(),
            fa76::ui::UI_ORIGIN => fa76::ui_param_id(wire_index).is_some(),
            burnlimit::ui::UI_ORIGIN => burnlimit::ui_param_id(wire_index).is_some(),
            clipper67::ui::UI_ORIGIN => clipper67::ui_param_id(wire_index).is_some(),
            transient::ui::UI_ORIGIN => transient::ui_param_id(wire_index).is_some(),
            wrapsynth::ui::UI_ORIGIN => wrapsynth::ui_param_id(wire_index).is_some(),
            zcomp::ui::UI_ORIGIN => zcomp::ui_param_id(wire_index).is_some(),
            mixstation::ui::UI_ORIGIN => mixstation::ui_param_id(wire_index).is_some(),
            _ => false,
        };
        if !known {
            return;
        }
        let Ok(mut states) = map().lock() else {
            return;
        };
        match entry_for(&mut states, insert_id, origin) {
            Some(BuiltinParams::Rodhareist(params)) => {
                if let Some(id) = rodharerist::ui_param_id(wire_index) {
                    let _ = rodharerist::apply_to_params(params, id, value);
                }
            }
            Some(BuiltinParams::Equz8(params)) => {
                let _ = equz8::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Verbspace(params)) => {
                let _ = verbspace::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Echospace(params)) => {
                let _ = echospace::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Fa2a(params)) => {
                let _ = fa2a::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Fa76(params)) => {
                let _ = fa76::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::BurnLimit(params)) => {
                let _ = burnlimit::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Clipper67(params)) => {
                let _ = clipper67::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Transient(params)) => {
                let _ = transient::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::WrapSynth(params)) => {
                let _ = wrapsynth::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::Zcomp(params)) => {
                let _ = zcomp::ipc::apply_wire_param(params, wire_index, value);
            }
            Some(BuiltinParams::MixStation(params)) => {
                let _ = mixstation::ipc::apply_wire_param(params, wire_index, value);
            }
            None => {}
        }
    }

    /// Seed the mirror from `plugin_id`'s persisted state blob — only when this
    /// plugin has no entry yet, so live edits always win over stale disk state.
    pub fn builtin_state_seed(plugin_id: &str, insert_id: &str, state_bytes: &[u8]) {
        let Some(origin) = origin_for_plugin_id(plugin_id) else {
            return;
        };
        let Ok(text) = std::str::from_utf8(state_bytes) else {
            return;
        };
        let parsed = match origin {
            rodharerist::ui::UI_ORIGIN => rodharerist::RodhareistState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Rodhareist(Box::new(state.params))),
            equz8::ui::UI_ORIGIN => equz8::ipc::Equz8State::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Equz8(Box::new(state.params))),
            verbspace::ui::UI_ORIGIN => verbspace::ipc::VerbspaceState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Verbspace(Box::new(state.params))),
            echospace::ui::UI_ORIGIN => echospace::ipc::EchospaceState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Echospace(Box::new(state.params))),
            fa2a::ui::UI_ORIGIN => fa2a::ipc::Fa2aState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Fa2a(Box::new(state.params))),
            fa76::ui::UI_ORIGIN => fa76::ipc::Fa76State::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Fa76(Box::new(state.params))),
            burnlimit::ui::UI_ORIGIN => burnlimit::ipc::BurnLimitState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::BurnLimit(Box::new(state.params))),
            clipper67::ui::UI_ORIGIN => clipper67::ipc::Clipper67State::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Clipper67(Box::new(state.params))),
            transient::ui::UI_ORIGIN => transient::ipc::TransientState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Transient(Box::new(state.params))),
            wrapsynth::ui::UI_ORIGIN => wrapsynth::ipc::WrapSynthState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::WrapSynth(Box::new(state.params))),
            zcomp::ui::UI_ORIGIN => zcomp::ipc::ZcompState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::Zcomp(Box::new(state.params))),
            mixstation::ui::UI_ORIGIN => mixstation::ipc::MixStationState::from_json(text)
                .ok()
                .map(|state| BuiltinParams::MixStation(Box::new(state.params))),
            _ => None,
        };
        let Some(parsed) = parsed else {
            return;
        };
        if let Ok(mut states) = map().lock() {
            match states.get(insert_id) {
                // This plugin's live state is authoritative — leave it alone.
                Some(existing) if existing.origin() == origin => {}
                // Absent, or another plugin's leftovers in the same slot.
                _ => {
                    states.insert(insert_id.to_string(), parsed);
                }
            }
        }
    }

    /// Serialized state blob for persistence / `selectInstance`. `None` when the
    /// slot holds no state, or holds a *different* built-in's state — that is
    /// never handed to the asking plugin.
    pub fn builtin_state_bytes(plugin_id: &str, insert_id: &str) -> Option<Vec<u8>> {
        let origin = origin_for_plugin_id(plugin_id)?;
        let states = map().lock().ok()?;
        let json = match states.get(insert_id)? {
            BuiltinParams::Rodhareist(params) if origin == rodharerist::ui::UI_ORIGIN => {
                rodharerist::RodhareistState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Equz8(params) if origin == equz8::ui::UI_ORIGIN => {
                equz8::ipc::Equz8State::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Verbspace(params) if origin == verbspace::ui::UI_ORIGIN => {
                verbspace::ipc::VerbspaceState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Echospace(params) if origin == echospace::ui::UI_ORIGIN => {
                echospace::ipc::EchospaceState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Fa2a(params) if origin == fa2a::ui::UI_ORIGIN => {
                fa2a::ipc::Fa2aState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Fa76(params) if origin == fa76::ui::UI_ORIGIN => {
                fa76::ipc::Fa76State::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::BurnLimit(params) if origin == burnlimit::ui::UI_ORIGIN => {
                burnlimit::ipc::BurnLimitState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Clipper67(params) if origin == clipper67::ui::UI_ORIGIN => {
                clipper67::ipc::Clipper67State::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Transient(params) if origin == transient::ui::UI_ORIGIN => {
                transient::ipc::TransientState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::WrapSynth(params) if origin == wrapsynth::ui::UI_ORIGIN => {
                wrapsynth::ipc::WrapSynthState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::Zcomp(params) if origin == zcomp::ui::UI_ORIGIN => {
                zcomp::ipc::ZcompState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            BuiltinParams::MixStation(params) if origin == mixstation::ui::UI_ORIGIN => {
                mixstation::ipc::MixStationState::new((**params).clone())
                    .to_json()
                    .ok()?
            }
            _ => return None,
        };
        Some(json.into_bytes())
    }

    /// The mirrored state as `(wire index, raw value)` pairs, in replay-safe
    /// order — pushed through the live param channel to rebuild a host DSP
    /// after project open or host respawn. Empty when the insert has no
    /// mirrored state for this plugin (fresh insert: host defaults already
    /// match).
    pub fn builtin_state_replay(plugin_id: &str, insert_id: &str) -> Vec<(u32, f32)> {
        let Some(origin) = origin_for_plugin_id(plugin_id) else {
            return Vec::new();
        };
        let Ok(states) = map().lock() else {
            return Vec::new();
        };
        match states.get(insert_id) {
            Some(BuiltinParams::Rodhareist(params)) if origin == rodharerist::ui::UI_ORIGIN => {
                rodharerist::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| rodharerist::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Equz8(params)) if origin == equz8::ui::UI_ORIGIN => {
                equz8::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| equz8::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Verbspace(params)) if origin == verbspace::ui::UI_ORIGIN => {
                verbspace::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| verbspace::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Echospace(params)) if origin == echospace::ui::UI_ORIGIN => {
                echospace::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| echospace::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Fa2a(params)) if origin == fa2a::ui::UI_ORIGIN => {
                fa2a::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| fa2a::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Fa76(params)) if origin == fa76::ui::UI_ORIGIN => {
                fa76::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| fa76::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::BurnLimit(params)) if origin == burnlimit::ui::UI_ORIGIN => {
                burnlimit::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| burnlimit::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Clipper67(params)) if origin == clipper67::ui::UI_ORIGIN => {
                clipper67::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| clipper67::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Transient(params)) if origin == transient::ui::UI_ORIGIN => {
                transient::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| transient::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::WrapSynth(params)) if origin == wrapsynth::ui::UI_ORIGIN => {
                wrapsynth::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| wrapsynth::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::Zcomp(params)) if origin == zcomp::ui::UI_ORIGIN => {
                zcomp::ipc::ui_values(params)
                    .into_iter()
                    .filter_map(|(id, value)| zcomp::ui_param_index(id).map(|i| (i, value)))
                    .collect()
            }
            Some(BuiltinParams::MixStation(params)) if origin == mixstation::ui::UI_ORIGIN => {
                mixstation::ipc::ui_values(params)
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| (index as u32, value))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Drop one insert's mirrored state (insert removed/unloaded). Not keyed by
    /// plugin: the slot is going away whoever owned it.
    pub fn builtin_state_remove(insert_id: &str) {
        if let Ok(mut states) = map().lock() {
            states.remove(insert_id);
        }
    }

    /// Drop everything (project close).
    pub fn builtin_state_clear() {
        if let Ok(mut states) = map().lock() {
            states.clear();
        }
    }
}

#[cfg(feature = "builtin-plugin-editor")]
pub use state_mirror::{
    builtin_state_apply, builtin_state_bytes, builtin_state_clear, builtin_state_remove,
    builtin_state_replay, builtin_state_seed,
};

/// Featureless no-ops: without the editor there is no param wire, so there is
/// no state to mirror.
#[cfg(not(feature = "builtin-plugin-editor"))]
mod state_mirror_stubs {
    pub fn builtin_state_apply(_plugin_id: &str, _insert_id: &str, _wire_index: u32, _value: f32) {}
    pub fn builtin_state_seed(_plugin_id: &str, _insert_id: &str, _state_bytes: &[u8]) {}
    pub fn builtin_state_bytes(_plugin_id: &str, _insert_id: &str) -> Option<Vec<u8>> {
        None
    }
    pub fn builtin_state_replay(_plugin_id: &str, _insert_id: &str) -> Vec<(u32, f32)> {
        Vec::new()
    }
    pub fn builtin_state_remove(_insert_id: &str) {}
    pub fn builtin_state_clear() {}
}

#[cfg(not(feature = "builtin-plugin-editor"))]
pub use state_mirror_stubs::{
    builtin_state_apply, builtin_state_bytes, builtin_state_clear, builtin_state_remove,
    builtin_state_replay, builtin_state_seed,
};

/// Physical-pixel rect the editor view occupies inside its parent window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Whether built-in editors are hosted off-screen (OSR) or as a native CEF
/// child window.
///
/// Windows and macOS host them **windowed**: the editor shell creates a real
/// content child (`plugin_content_host` HWND, or an AppKit container via
/// `plugin_editor_mac_region`) and CEF owns a child browser inside it, so
/// Chromium paints, composites, and receives input directly — no shared-texture
/// copy, no frame republishing through the GPUI atlas, no synthesized input.
/// Linux retains the software off-screen path until its native embedding is
/// enabled.
pub const OFFSCREEN_HOSTING: bool = cfg!(not(any(target_os = "windows", target_os = "macos")));

/// Explicit browser lifetime. Illegal edges are rejected instead of being
/// applied to a closing or already-closed instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserPhase {
    Creating,
    Ready,
    Attached,
    Detached,
    Closing,
    Closed,
}

fn transition_browser_phase(from: BrowserPhase, to: BrowserPhase) -> Option<BrowserPhase> {
    use BrowserPhase::*;
    let allowed = matches!(
        (from, to),
        (Creating, Ready)
            | (Creating, Closing)
            | (Ready, Attached)
            | (Ready, Closing)
            | (Attached, Detached)
            | (Attached, Closing)
            | (Detached, Attached)
            | (Detached, Closing)
            | (Closing, Closed)
    );
    allowed.then_some(to)
}

/// Optional synchronous GPU sink supplied by the GPUI editor window.
#[cfg(feature = "builtin-plugin-editor")]
pub type AcceleratedFrameSink = std::sync::Arc<dyn sphere_webview::osr::OsrAcceleratedFrameSink>;

#[cfg(not(feature = "builtin-plugin-editor"))]
#[derive(Clone)]
pub struct AcceleratedFrameSink;

/// Modifier state accompanying a forwarded input event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub command: bool,
    pub left_button: bool,
    pub middle_button: bool,
    pub right_button: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorKeyKind {
    Down,
    Up,
    /// A typed character. `character` carries the UTF-16 code unit; the key
    /// code is ignored.
    Char,
}

/// `windows_key_code` is Chromium's platform-independent `VKEY_*` value (the
/// same numbering as Win32 `VK_*`), which CEF expects on every OS.
#[derive(Debug, Clone, Copy)]
pub struct EditorKey {
    pub kind: EditorKeyKind,
    pub windows_key_code: i32,
    pub character: u16,
    pub modifiers: EditorModifiers,
}

/// One input event forwarded from the GPUI shell into an off-screen browser.
/// Coordinates are **logical** pixels relative to the view's top-left corner —
/// the same space CEF lays the page out in.
#[derive(Debug, Clone, Copy)]
pub enum EditorInput {
    MouseMove {
        x: i32,
        y: i32,
        modifiers: EditorModifiers,
        leaving: bool,
    },
    MouseButton {
        x: i32,
        y: i32,
        button: EditorMouseButton,
        pressed: bool,
        click_count: i32,
        modifiers: EditorModifiers,
    },
    MouseWheel {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: EditorModifiers,
    },
    Key(EditorKey),
    Focus(bool),
    /// The host lost the native pointer grab. Forwarded to the browser as
    /// `SendCaptureLostEvent`; the caller is responsible for clearing its own
    /// held-button state, and must not also synthesize a mouse-up.
    CaptureLost,
}

/// Where a hosted browser view sits on the physical desktop. Mirrors
/// [`sphere_webview::osr::OsrScreenGeometry`] so callers outside the
/// `builtin-plugin-editor` feature still compile.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewScreenGeometry {
    /// Physical screen coordinates of the browser view's top-left pixel.
    pub view_origin_physical: (i32, i32),
    /// The real display's bounds in DIP.
    pub monitor_rect_dip: ViewRect,
    /// The DIP screen rectangle popups must stay inside.
    pub available_rect_dip: ViewRect,
}

/// Process-unique identity for one concrete editor window.
///
/// The logical editor id (`track::insert`) can be reused after a window closes;
/// native host commands must not be, otherwise a delayed close from the old
/// window can tear down the newly opened browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewId(u64);

pub fn allocate_view_id() -> ViewId {
    static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);
    ViewId(NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed))
}

/// Completion events produced by the serialized CEF command processor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Opened,
    OpenFailed(String),
    Closed,
    /// Accelerated shared-texture import failed and the host is recreating this
    /// browser in software OSR mode. Native DSP state and the editor window stay
    /// alive while the page bridge reconnects.
    AcceleratedFallback,
    /// The renderer process for this browser terminated (crash, OOM-kill,
    /// `chrome://kill` in dev). The browser object and native DSP state on
    /// the Rust side are untouched — only the page's JS state is gone, so
    /// the window stays open and the browser reloads its own URL; once
    /// `bridgeReady` arrives again the current selection is re-sent (see
    /// `BuiltinPluginEditorWindow::push_selected_instance`, gated on
    /// `browser_ready`).
    RendererCrashed,
}

#[cfg(not(feature = "builtin-plugin-editor"))]
mod imp {
    use super::{EditorInput, HostAvailability, ViewEvent, ViewId, ViewRect};

    pub fn availability(_plugin_id: &str) -> HostAvailability {
        HostAvailability::NotCompiledIn
    }

    pub fn pump() {}

    pub fn pump_after_input() {}

    pub fn open_view(
        _view_id: ViewId,
        _editor_id: &str,
        _plugin_id: &str,
        _parent_hwnd: u64,
        _rect: ViewRect,
        _scale_factor: f32,
        _accelerated_sink: Option<super::AcceleratedFrameSink>,
    ) -> Result<(), HostAvailability> {
        Err(HostAvailability::NotCompiledIn)
    }

    pub fn view_frame_generation(_view_id: ViewId) -> u64 {
        0
    }

    pub fn view_uses_accelerated_osr(_view_id: ViewId) -> bool {
        false
    }

    pub fn with_view_frame<R>(
        _view_id: ViewId,
        _read: impl FnOnce(&[u8], i32, i32) -> R,
    ) -> Option<R> {
        None
    }

    pub fn send_view_input(_view_id: ViewId, _input: EditorInput) {}

    pub fn init_at_boot() -> Result<(), HostAvailability> {
        Err(HostAvailability::NotCompiledIn)
    }

    pub fn preload() {}

    pub fn set_view_bounds(_view_id: ViewId, _rect: ViewRect, _scale_factor: f32) {}

    pub fn set_view_screen_geometry(_view_id: ViewId, _geometry: super::ViewScreenGeometry) {}

    pub fn close_view(_view_id: ViewId) {}

    pub fn browser_holds_native_parent(_view_id: ViewId) -> bool {
        false
    }

    pub fn take_view_events(_view_id: ViewId) -> Vec<ViewEvent> {
        Vec::new()
    }

    pub fn is_view_open(_view_id: ViewId) -> bool {
        false
    }

    pub fn take_inbound(_origin: &str) -> Vec<Vec<u8>> {
        Vec::new()
    }

    pub fn send_to_view(_view_id: ViewId, _code: &str) {}

    pub fn reload_view(_view_id: ViewId) {}

    pub fn take_global_play_pause_requests(_view_id: ViewId) -> u32 {
        0
    }
}

#[cfg(feature = "builtin-plugin-editor")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use sphere_webview::client::{plugin_browser_client_with_surface, BrowserLifecycle};
    use sphere_webview::osr::{
        OsrInput, OsrKey, OsrKeyKind, OsrModifiers, OsrMouseButton, OsrSurface,
    };
    use sphere_webview::runtime::cef::rc::Rc as _;
    use sphere_webview::runtime::{
        CefRuntime, CefRuntimeConfig, CefRuntimeError, NativeParent, WebView, WebViewConfig,
        WindowBounds,
    };
    use sphere_webview::scheme::{register_plugin_scheme_factory, BridgeSink, SchemeAsset};

    use super::{
        origin_for_plugin_id, AcceleratedFrameSink, EditorInput, EditorKey, EditorKeyKind,
        EditorModifiers, EditorMouseButton, HostAvailability, ViewEvent, ViewId, ViewRect,
        ViewScreenGeometry,
    };

    struct HostedView {
        _editor_id: String,
        // Drop the browser before the explicitly retained client.
        view: WebView<'static>,
        _client: sphere_webview::runtime::cef::Client,
        lifecycle: BrowserLifecycle,
        open: PendingOpen,
        bounds: PendingBounds,
        /// Last desktop placement pushed to the surface. Retained so a browser
        /// recreated for accelerated-copy fallback starts on the display it was
        /// already on rather than reporting an unresolved screen.
        screen_geometry: ViewScreenGeometry,
        opened_at: std::time::Instant,
        stability_reported: bool,
        phase: super::BrowserPhase,
    }

    struct ClosingView {
        hosted: HostedView,
        pump_ticks: u16,
        timeout_logged: bool,
    }

    struct FallbackClosingView {
        closing: ClosingView,
        view_id: ViewId,
        reopen: PendingOpen,
        bounds: PendingBounds,
        screen_geometry: ViewScreenGeometry,
        cancel_reopen: bool,
        timeout_reported: bool,
    }

    const MAX_CLOSE_PUMP_TICKS: u16 = 250;

    /// Boot-time warm-up browser (see [`preload`]).
    ///
    /// Kept alive for the whole session: it pins Chromium's helper processes
    /// (GPU, network service, a renderer) so every editor open — not just the
    /// first — skips the multi-hundred-millisecond subprocess spawn.
    struct WarmupBrowser {
        // Drop the browser before the explicitly retained client.
        _view: WebView<'static>,
        _client: sphere_webview::runtime::cef::Client,
        _lifecycle: BrowserLifecycle,
        /// Hidden native parent of a *windowed* warm-up browser (Windows).
        /// Declared last so it outlives the browser it hosts.
        _parent: Option<crate::components::plugin_content_host::HiddenHostWindow>,
    }

    /// One CEF runtime plus every open editor view.
    ///
    /// Field order is load-bearing: Rust drops fields in declaration order, and
    /// the detached views must be released before the runtime that created them
    /// (see `CefRuntime::create_webview_detached`).
    struct Host {
        views: HashMap<ViewId, HostedView>,
        closing_views: HashMap<ViewId, ClosingView>,
        /// Accelerated browsers being retired during transparent software
        /// fallback. They must stay alive through OnBeforeClose but must not
        /// emit `Closed` for the still-open GPUI editor window.
        fallback_closing_views: Vec<FallbackClosingView>,
        /// Browsers whose `OnBeforeClose` already ran. Dropped on the next
        /// pump, after this call has returned to the host run loop, so the
        /// CefRefPtr is not released on the CEF callback stack.
        deferred_release: Vec<HostedView>,
        warmup: Option<WarmupBrowser>,
        runtime: CefRuntime,
        // The exact CefApp passed to execute_process in the browser process.
        // Keep it alive until after the runtime shuts down.
        _application: sphere_webview::runtime::cef::App,
    }

    #[derive(Clone)]
    struct PendingOpen {
        editor_id: String,
        origin: &'static str,
        parent_hwnd: u64,
        rect: ViewRect,
        accelerated_sink: Option<AcceleratedFrameSink>,
    }

    /// Physical rect plus the scale it was measured at. Off-screen browsers are
    /// told a *logical* size and render at `scale`. A windowed Win32 child uses
    /// the physical rect directly; a windowed AppKit child is placed in points.
    #[derive(Debug, Clone, Copy)]
    struct PendingBounds {
        rect: ViewRect,
        scale_factor: f32,
    }

    #[derive(Default)]
    struct PendingViewCommands {
        open: Option<PendingOpen>,
        bounds: Option<PendingBounds>,
        /// Desktop placement, coalesced separately from `bounds`: dragging the
        /// window changes where the view is on screen without changing its
        /// size, and a DPI change does both.
        screen_geometry: Option<ViewScreenGeometry>,
        close: bool,
    }

    thread_local! {
        /// Browser-process CEF state, owned by the UI thread because
        /// `CefRuntime` is `!Send`.
        static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
        /// Browser-process app transferred from the executable after
        /// `execute_process` returns the browser-process sentinel.
        static PROCESS_APP: RefCell<Option<sphere_webview::runtime::cef::App>> = const { RefCell::new(None) };
        /// CEF initialization is process-global and must not be retried after a
        /// partial failure.
        static RUNTIME_FAILURE: RefCell<Option<String>> = const { RefCell::new(None) };
        /// Native operations are queued separately from `HOST`. GPUI/Win32 can
        /// re-enter while CEF is working; nested callbacks may enqueue commands,
        /// but must never try to borrow or call CEF synchronously.
        static COMMANDS: RefCell<HashMap<ViewId, PendingViewCommands>> = RefCell::new(HashMap::new());
        static EVENTS: RefCell<HashMap<ViewId, Vec<ViewEvent>>> = RefCell::new(HashMap::new());
        /// Per-view `(accelerated, software)` paint counts already published to
        /// the OSR profiler. See `mirror_paint_counters`.
        static MIRRORED_PAINTS: RefCell<HashMap<ViewId, (u64, u64)>> = RefCell::new(HashMap::new());
        static PUMPING: Cell<bool> = const { Cell::new(false) };
        /// Editor windows tick independently, but CEF owns one process-wide
        /// message loop. Coalesce near-simultaneous calls so N open plugin
        /// types do not pump Chromium N times and starve GPUI input handling.
        static LAST_CEF_PUMP: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
    }

    struct PumpGuard;

    impl PumpGuard {
        fn enter() -> Option<Self> {
            PUMPING.with(|pumping| {
                if pumping.replace(true) {
                    None
                } else {
                    Some(Self)
                }
            })
        }
    }

    impl Drop for PumpGuard {
        fn drop(&mut self) {
            PUMPING.with(|pumping| pumping.set(false));
        }
    }

    /// Resolve `mikoplugin://<origin>/<path>` to embedded bytes.
    ///
    /// This is the isolation boundary: an origin that is not a known built-in
    /// returns `None`, so one plugin's editor can never read another's assets.
    /// Runs on CEF's IO thread — it only indexes static tables.
    fn resolve_asset(origin: &str, path: &str) -> Option<SchemeAsset> {
        use builtin_ui_embed::EmbeddedPluginUi;
        let asset = match origin {
            rodharerist::ui::UI_ORIGIN => rodharerist::ui::RodhareistUi::resolve_ui_asset(path)?,
            equz8::ui::UI_ORIGIN => equz8::ui::Equz8Ui::resolve_ui_asset(path)?,
            verbspace::ui::UI_ORIGIN => verbspace::ui::VerbspaceUi::resolve_ui_asset(path)?,
            echospace::ui::UI_ORIGIN => echospace::ui::EchospaceUi::resolve_ui_asset(path)?,
            fa2a::ui::UI_ORIGIN => fa2a::ui::Fa2aUi::resolve_ui_asset(path)?,
            fa76::ui::UI_ORIGIN => fa76::ui::Fa76Ui::resolve_ui_asset(path)?,
            burnlimit::ui::UI_ORIGIN => burnlimit::ui::BurnLimitUi::resolve_ui_asset(path)?,
            clipper67::ui::UI_ORIGIN => clipper67::ui::Clipper67Ui::resolve_ui_asset(path)?,
            transient::ui::UI_ORIGIN => transient::ui::TransientUi::resolve_ui_asset(path)?,
            wrapsynth::ui::UI_ORIGIN => wrapsynth::ui::WrapSynthUi::resolve_ui_asset(path)?,
            zcomp::ui::UI_ORIGIN => zcomp::ui::ZcompUi::resolve_ui_asset(path)?,
            mixstation::ui::UI_ORIGIN => mixstation::ui::MixStationUi::resolve_ui_asset(path)?,
            _ => return None,
        };
        Some(SchemeAsset {
            bytes: asset.bytes,
            mime_type: asset.mime_type,
        })
    }

    /// Whether this build links a built-in's editor at all. Distinct from
    /// [`has_embedded_ui`]: an origin can be hosted here yet carry an empty
    /// asset table when its bundle was never built, and the two cases get
    /// different `HostAvailability` errors.
    fn hosts_editor(origin: &str) -> bool {
        matches!(
            origin,
            rodharerist::ui::UI_ORIGIN
                | equz8::ui::UI_ORIGIN
                | verbspace::ui::UI_ORIGIN
                | echospace::ui::UI_ORIGIN
                | fa2a::ui::UI_ORIGIN
                | fa76::ui::UI_ORIGIN
                | burnlimit::ui::UI_ORIGIN
                | clipper67::ui::UI_ORIGIN
                | transient::ui::UI_ORIGIN
                | wrapsynth::ui::UI_ORIGIN
                | zcomp::ui::UI_ORIGIN
                | mixstation::ui::UI_ORIGIN
        )
    }

    /// Whether a built-in plugin has embedded editor assets to serve.
    fn has_embedded_ui(origin: &str) -> bool {
        match origin {
            rodharerist::ui::UI_ORIGIN => rodharerist::ui::RodhareistUi::is_embedded(),
            equz8::ui::UI_ORIGIN => equz8::ui::Equz8Ui::is_embedded(),
            verbspace::ui::UI_ORIGIN => verbspace::ui::VerbspaceUi::is_embedded(),
            echospace::ui::UI_ORIGIN => echospace::ui::EchospaceUi::is_embedded(),
            fa2a::ui::UI_ORIGIN => fa2a::ui::Fa2aUi::is_embedded(),
            fa76::ui::UI_ORIGIN => fa76::ui::Fa76Ui::is_embedded(),
            burnlimit::ui::UI_ORIGIN => burnlimit::ui::BurnLimitUi::is_embedded(),
            clipper67::ui::UI_ORIGIN => clipper67::ui::Clipper67Ui::is_embedded(),
            transient::ui::UI_ORIGIN => transient::ui::TransientUi::is_embedded(),
            wrapsynth::ui::UI_ORIGIN => wrapsynth::ui::WrapSynthUi::is_embedded(),
            zcomp::ui::UI_ORIGIN => zcomp::ui::ZcompUi::is_embedded(),
            mixstation::ui::UI_ORIGIN => mixstation::ui::MixStationUi::is_embedded(),
            _ => false,
        }
    }

    /// React->native bridge inbound queue, keyed by scheme origin (the same
    /// stem `resolve_asset` matches on, e.g. `"rodharerist"`). Filled from
    /// CEF's IO thread (`bridge_sink`, below) — process-wide `Mutex`, not a
    /// UI-thread `thread_local!` like `HOST`/`COMMANDS`, since the scheme
    /// factory callback does not run on the UI thread. Drained by
    /// `take_inbound`, called from the GPUI pump tick (non-realtime).
    static INBOUND: OnceLock<Mutex<HashMap<String, Vec<Vec<u8>>>>> = OnceLock::new();

    fn inbound_map() -> &'static Mutex<HashMap<String, Vec<Vec<u8>>>> {
        INBOUND.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn bridge_sink() -> BridgeSink {
        Arc::new(|origin: &str, body: Vec<u8>| {
            if let Ok(mut map) = inbound_map().lock() {
                map.entry(origin.to_string()).or_default().push(body);
            }
        })
    }

    fn origin_still_attached(host: &Host, origin: &str) -> bool {
        host.views
            .values()
            .any(|hosted| hosted.open.origin == origin)
    }

    fn log_resource_counters(host: &Host) {
        if !sphere_webview::scheme::cef_diagnostics_enabled()
            && !crate::boot::has_flag("--cef-stress-test")
        {
            return;
        }
        thread_local! {
            static LAST_LOG: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
        }
        let due = LAST_LOG.with(|last| {
            let now = std::time::Instant::now();
            let due = last
                .get()
                .is_none_or(|previous| previous.elapsed() >= std::time::Duration::from_secs(5));
            if due {
                last.set(Some(now));
            }
            due
        });
        if !due {
            return;
        }
        let browsers = host.views.len()
            + host.closing_views.len()
            + host.fallback_closing_views.len()
            + host.deferred_release.len()
            + usize::from(host.warmup.is_some());
        let pending_close = host.closing_views.len() + host.fallback_closing_views.len();
        #[cfg(target_os = "macos")]
        let memory = crate::components::plugin_editor_mac_region::appkit::task_memory();
        #[cfg(not(target_os = "macos"))]
        let memory: Option<(u64, u64)> = None;
        let (resident, virtual_size) = memory.unwrap_or((0, 0));
        eprintln!(
            "[cef-resources] active_browser_count={browsers} pending_browser_close_count={pending_close} active_surface_count={browsers} active_texture_count=0 active_iosurface_count=host-does-not-own resident_bytes={resident} virtual_bytes={virtual_size}"
        );
    }

    fn discard_inbound(origin: &str) {
        if let Ok(mut map) = inbound_map().lock() {
            map.remove(origin);
        }
    }

    /// Drain every bridge message POSTed by `origin`'s page since the last
    /// call. Never blocks on CEF — just takes whatever `bridge_sink` queued.
    pub fn take_inbound(origin: &str) -> Vec<Vec<u8>> {
        inbound_map()
            .lock()
            .ok()
            .and_then(|mut map| map.remove(origin))
            .unwrap_or_default()
    }

    /// Run `code` in `view_id`'s document. No-op (not an error) if the view
    /// isn't open yet or already closed — callers already gate on
    /// `is_view_open`/`ViewEvent::Opened` where it matters.
    pub fn send_to_view(view_id: ViewId, code: &str) {
        if !sphere_webview::scheme::cef_ipc_enabled() {
            return;
        }
        HOST.with(|cell| {
            if let Ok(slot) = cell.try_borrow() {
                if let Some(host) = slot.as_ref() {
                    if let Some(hosted) = host.views.get(&view_id) {
                        if !matches!(
                            hosted.phase,
                            super::BrowserPhase::Ready | super::BrowserPhase::Attached
                        ) {
                            return;
                        }
                        if let Err(error) = hosted.view.execute_javascript(code) {
                            eprintln!(
                                "[plugin-bridge] execute_javascript failed view_id={view_id:?} err={error}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Reload `view_id`'s document. Used by the bridge-ready watchdog to
    /// recover a page whose load silently died (e.g. Chromium's network
    /// service crashed mid-transfer, which it auto-restarts for *future*
    /// requests but does not retry the one already in flight, so a page can
    /// finish HTTP-200 headers and still never actually paint).
    pub fn reload_view(view_id: ViewId) {
        HOST.with(|cell| {
            if let Ok(slot) = cell.try_borrow() {
                if let Some(host) = slot.as_ref() {
                    if let Some(hosted) = host.views.get(&view_id) {
                        if !matches!(
                            hosted.phase,
                            super::BrowserPhase::Ready | super::BrowserPhase::Attached
                        ) {
                            return;
                        }
                        if let Err(error) = hosted.view.reload() {
                            eprintln!(
                                "[plugin-bridge] reload failed view_id={view_id:?} err={error}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Drain global transport shortcuts captured by this view's native CEF
    /// keyboard handler.
    pub fn take_global_play_pause_requests(view_id: ViewId) -> u32 {
        HOST.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|slot| {
                    slot.as_ref()?
                        .views
                        .get(&view_id)
                        .map(|hosted| hosted.lifecycle.take_global_play_pause_requests())
                })
                .unwrap_or(0)
        })
    }

    pub fn availability(plugin_id: &str) -> HostAvailability {
        if crate::boot::has_flag("--disable-cef") {
            return HostAvailability::RuntimeFailed("CEF is disabled by --disable-cef".to_owned());
        }
        let Some(origin) = origin_for_plugin_id(plugin_id) else {
            return HostAvailability::NoEditorForPlugin(plugin_id.to_string());
        };
        if !hosts_editor(origin) {
            return HostAvailability::NoEditorForPlugin(plugin_id.to_string());
        }
        if !has_embedded_ui(origin) {
            return HostAvailability::UiNotEmbedded(plugin_id.to_string());
        }
        HostAvailability::Ready
    }

    /// Transfer ownership of the browser process's `CefApp` into the UI-thread
    /// host. CEF requires this exact object for both `execute_process` and
    /// `initialize`.
    pub fn install_process_app(
        app: sphere_webview::runtime::cef::App,
    ) -> Result<(), HostAvailability> {
        PROCESS_APP.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_some() || HOST.with(|host| host.borrow().is_some()) {
                return Err(HostAvailability::RuntimeFailed(
                    "CEF process application was installed more than once".to_owned(),
                ));
            }
            *slot = Some(app);
            Ok(())
        })
    }

    /// Start CEF if it is not already running. Idempotent.
    fn ensure_runtime(slot: &mut Option<Host>) -> Result<(), HostAvailability> {
        if slot.is_some() {
            return Ok(());
        }
        if crate::boot::has_flag("--disable-cef") {
            return Err(HostAvailability::RuntimeFailed(
                "CEF is disabled by --disable-cef".to_owned(),
            ));
        }
        if let Some(error) = RUNTIME_FAILURE.with(|failure| failure.borrow().clone()) {
            return Err(HostAvailability::RuntimeFailed(error));
        }

        let mut app = PROCESS_APP
            .with(|cell| cell.borrow_mut().take())
            .ok_or_else(|| {
                HostAvailability::RuntimeFailed(
                    "CEF process application was not installed before UI startup".to_owned(),
                )
            })?;
        let config = CefRuntimeConfig {
            remote_debugging_port: debug_port(),
            browser_subprocess: match sphere_webview::runtime::platform_browser_subprocess() {
                Ok(subprocess) => subprocess,
                Err(error) => return remember_runtime_failure(error.to_string()),
            },
            // Chromium decides this once, at initialize. Windows and macOS
            // host every built-in editor as a native child, and CEF advises
            // against enabling OSR support in a process that never uses it;
            // Linux is still OSR-hosted.
            windowless_rendering: super::OFFSCREEN_HOSTING,
            ..Default::default()
        };
        let runtime = match CefRuntime::initialize(config, Some(&mut app)) {
            Ok(runtime) => runtime,
            Err(error) => return remember_runtime_failure(error.to_string()),
        };

        // The factory can only be installed once initialize has succeeded.
        let resolver: sphere_webview::scheme::SchemeResolver =
            Arc::new(|origin: &str, path: &str| resolve_asset(origin, path));
        if let Err(error) = register_plugin_scheme_factory(resolver, Some(bridge_sink())) {
            return remember_runtime_failure(error.to_string());
        }

        *slot = Some(Host {
            views: HashMap::new(),
            closing_views: HashMap::new(),
            fallback_closing_views: Vec::new(),
            deferred_release: Vec::new(),
            warmup: None,
            runtime,
            _application: app,
        });
        Ok(())
    }

    fn remember_runtime_failure(error: String) -> Result<(), HostAvailability> {
        RUNTIME_FAILURE.with(|failure| {
            *failure.borrow_mut() = Some(error.clone());
        });
        Err(HostAvailability::RuntimeFailed(error))
    }

    /// Create the boot-time warm-up browser (idempotent).
    ///
    /// The first `CreateBrowserSync` of a session pays for spawning Chromium's
    /// helper processes (GPU, network service, renderer) and initializing the
    /// profile — several hundred milliseconds that used to land inside the
    /// first editor open. Creating a hidden `about:blank` browser during boot
    /// moves that cost behind the loading screen.
    ///
    /// `about:blank` is deliberate: preloading the real editor URL would run
    /// its page JS, whose `bridgeReady` POST lands in the origin-keyed
    /// [`INBOUND`] queue and would be misread as the real editor's handshake
    /// when one opens later.
    fn ensure_warmup(host: &mut Host) {
        ensure_warmup_supported(host);
    }

    fn ensure_warmup_supported(host: &mut Host) {
        if host.warmup.is_some() {
            return;
        }
        let url = "about:blank".to_string();
        // Windowed hosting needs a native parent, and the warm-up must never
        // show a window of its own: park it under a hidden top-level HWND.
        // Off-screen hosting paints into a throwaway 2×2 surface instead.
        let (parent_window, surface) = if super::OFFSCREEN_HOSTING {
            (None, Some(OsrSurface::new(2, 2, 1.0)))
        } else {
            match crate::components::plugin_content_host::HiddenHostWindow::create() {
                Some(window) => (Some(window), None),
                None => {
                    eprintln!("[cef-warmup] warm-up browser skipped: no hidden host window");
                    return;
                }
            }
        };
        let parent_handle = parent_window.as_ref().map_or(0, |window| window.hwnd());
        let (mut client, lifecycle) = plugin_browser_client_with_surface(&url, surface.clone());
        let result = WindowBounds::new(0, 0, 2, 2)
            .map_err(|error| error.to_string())
            .and_then(|bounds| {
                let mut config = WebViewConfig::new(url, bounds);
                if let Some(surface) = surface {
                    config = config.windowless(surface);
                }
                // SAFETY: the warm-up view is stored in `host.warmup`,
                // declared before `host.runtime`, and therefore released
                // first; its hidden parent lives in the same struct, after it.
                unsafe {
                    let parent = NativeParent::from_raw(hwnd_to_cef(parent_handle));
                    host.runtime
                        .create_webview_detached(parent, config, Some(&mut client))
                }
                .map_err(|error| error.to_string())
            });
        match result {
            Ok(view) => {
                eprintln!(
                    "[cef-warmup] warm-up browser created browser_id={} windowed={}",
                    view.browser_identifier(),
                    view.osr_surface().is_none()
                );
                host.warmup = Some(WarmupBrowser {
                    _view: view,
                    _client: client,
                    _lifecycle: lifecycle,
                    _parent: parent_window,
                });
            }
            Err(error) => {
                eprintln!("[cef-warmup] warm-up browser failed: {error}");
            }
        }
    }

    /// Boot-time preload: start CEF *and* spawn the warm-up browser so the
    /// first editor open only pays for its own page. Idempotent; failure is
    /// non-fatal (editors fall back to cold opens).
    ///
    /// Call from the UI thread, then drive [`pump`] for a couple of seconds
    /// (the boot pump loop) so the warm-up finishes spawning Chromium's helper
    /// processes while the loading screen is still up — with no editor window
    /// open nothing else pumps CEF.
    pub fn preload() {
        if crate::boot::has_flag("--disable-cef")
            || crate::boot::has_flag("--disable-cef-warmup")
            || std::env::var_os("FUTUREBOARD_DISABLE_CEF_WARMUP").is_some()
        {
            crate::boot::log("CEF warm-up skipped by runtime flag");
            return;
        }
        HOST.with(|cell| {
            let mut slot = cell.borrow_mut();
            if ensure_runtime(&mut slot).is_ok() {
                if let Some(host) = slot.as_mut() {
                    ensure_warmup(host);
                    if crate::boot::has_flag("--cef-stress-test") {
                        run_cef_stress(host);
                    }
                }
            }
        });
    }

    fn run_cef_stress(host: &mut Host) {
        let cycles = std::env::var("FUTUREBOARD_CEF_STRESS_CYCLES")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|cycles| *cycles > 0)
            .unwrap_or(500);
        let Some(parent_window) =
            crate::components::plugin_content_host::HiddenHostWindow::create()
        else {
            eprintln!("[cef-stress] skipped reason=no-hidden-parent");
            return;
        };
        eprintln!(
            "[cef-stress] begin cycles={cycles} static_test={} gpu_isolation={}",
            sphere_webview::scheme::cef_static_test_enabled(),
            std::env::var_os("FUTUREBOARD_CEF_DISABLE_GPU").is_some()
        );
        let mut failed_to_close = 0u32;
        for cycle in 1..=cycles {
            let url = if sphere_webview::scheme::cef_static_test_enabled() {
                "mikoplugin://stress/index.html".to_string()
            } else {
                "about:blank".to_string()
            };
            let (mut client, lifecycle) = plugin_browser_client_with_surface(&url, None);
            let result = WindowBounds::new(0, 0, 320, 200)
                .map_err(|error| error.to_string())
                .and_then(|bounds| {
                    let config = WebViewConfig::new(url, bounds);
                    unsafe {
                        let parent = NativeParent::from_raw(hwnd_to_cef(parent_window.hwnd()));
                        host.runtime
                            .create_webview_detached(parent, config, Some(&mut client))
                    }
                    .map_err(|error| error.to_string())
                });
            let Ok(view) = result else {
                failed_to_close += 1;
                eprintln!("[cef-stress] cycle={cycle} create_failed");
                continue;
            };
            if let Ok(bounds) = WindowBounds::new(0, 0, 640, 360) {
                let _ = view.set_bounds(bounds);
                let _ = view.set_bounds(bounds);
            }
            let _ = view.close(false);
            let mut closed = lifecycle.before_close();
            for _ in 0..40 {
                if closed {
                    break;
                }
                let _ = host.runtime.do_message_loop_work();
                closed = lifecycle.before_close();
            }
            if closed {
                drop(view);
                drop(client);
            } else {
                failed_to_close += 1;
                let view_id = ViewId(u64::MAX - u64::from(cycle));
                host.closing_views.insert(
                    view_id,
                    ClosingView {
                        hosted: HostedView {
                            _editor_id: format!("stress-{cycle}"),
                            view,
                            _client: client,
                            lifecycle,
                            open: PendingOpen {
                                editor_id: format!("stress-{cycle}"),
                                origin: "stress",
                                parent_hwnd: parent_window.hwnd(),
                                rect: ViewRect {
                                    x: 0,
                                    y: 0,
                                    width: 320,
                                    height: 200,
                                },
                                accelerated_sink: None,
                            },
                            bounds: PendingBounds {
                                rect: ViewRect {
                                    x: 0,
                                    y: 0,
                                    width: 320,
                                    height: 200,
                                },
                                scale_factor: 1.0,
                            },
                            screen_geometry: ViewScreenGeometry::default(),
                            opened_at: std::time::Instant::now(),
                            stability_reported: true,
                            phase: super::BrowserPhase::Closing,
                        },
                        pump_ticks: 0,
                        timeout_logged: false,
                    },
                );
            }
            if cycle % 50 == 0 || cycle == cycles {
                log_resource_counters(host);
                eprintln!(
                    "[cef-stress] cycle={cycle}/{cycles} not_closed_yet={failed_to_close} active_browsers={}",
                    host.views.len()
                        + host.closing_views.len()
                        + host.deferred_release.len()
                        + usize::from(host.warmup.is_some())
                );
            }
        }
        eprintln!("[cef-stress] complete cycles={cycles} close_timeouts={failed_to_close}");
        if failed_to_close > 0 {
            // The hidden window is the parent of browsers that have not
            // reached OnBeforeClose. Destroying it here would unmap their
            // compositor surfaces on the close stack.
            std::mem::forget(parent_window);
        }
    }

    /// Close every CEF browser and finish process-global shutdown on the UI
    /// thread after GPUI's application loop exits.
    pub fn shutdown() {
        HOST.with(|cell| {
            let Some(mut host) = cell.borrow_mut().take() else {
                return;
            };
            let open_count = host.views.len()
                + host.closing_views.len()
                + host.fallback_closing_views.len()
                + host.deferred_release.len()
                + usize::from(host.warmup.is_some());
            eprintln!("[cef-runtime] shutdown requested open_browser_count={open_count}");
            for hosted in host.views.values() {
                let _ = hosted.view.close(true);
            }
            for closing in host.closing_views.values() {
                let _ = closing.hosted.view.close(true);
            }
            for fallback in &host.fallback_closing_views {
                let _ = fallback.closing.hosted.view.close(true);
            }
            if let Some(warmup) = host.warmup.as_ref() {
                let _ = warmup._view.close(true);
            }

            let mut remaining = open_count;
            for _ in 0..MAX_CLOSE_PUMP_TICKS {
                let _ = host.runtime.do_message_loop_work();
                remaining = host
                    .views
                    .values()
                    .filter(|hosted| !hosted.lifecycle.before_close())
                    .count()
                    + host
                        .closing_views
                        .values()
                        .filter(|closing| !closing.hosted.lifecycle.before_close())
                        .count()
                    + host
                        .fallback_closing_views
                        .iter()
                        .filter(|fallback| !fallback.closing.hosted.lifecycle.before_close())
                        .count()
                    + host
                        .warmup
                        .as_ref()
                        .filter(|warmup| !warmup._lifecycle.before_close())
                        .map_or(0, |_| 1)
                    + host
                        .deferred_release
                        .iter()
                        .filter(|hosted| !hosted.lifecycle.before_close())
                        .count();
                if remaining == 0 {
                    break;
                }
            }
            eprintln!("[cef-runtime] shutdown close_pump_complete remaining_open={remaining}");
            if remaining != 0 {
                eprintln!(
                    "[cef-runtime] shutdown refused CefShutdown active_browser_count={remaining}"
                );
                host.runtime.suppress_shutdown();
                // Keep the CefRefPtrs alive. Dropping them, or calling
                // CefShutdown, while OnBeforeClose is still outstanding is the
                // SIGTRAP this path exists to avoid.
                std::mem::forget(host);
                debug_assert_eq!(
                    remaining, 0,
                    "CefShutdown requires every browser to have finished OnBeforeClose"
                );
                return;
            }
            debug_assert_eq!(remaining, 0);
            // Field order now releases all browser/client handles before
            // `CefRuntime::drop` invokes cef_shutdown exactly once.
            drop(host);
        });
    }

    /// Start CEF during application boot, on the UI thread.
    ///
    /// Initialization spawns Chromium's helper processes and takes on the order
    /// of a few hundred milliseconds. Doing it lazily on first editor open means
    /// paying that cost inside a render pass, which stalls the UI thread and
    /// delays the first paint of the editor window. Doing it at boot moves the
    /// cost into startup, where there is already a loading screen.
    ///
    /// The thread that calls this is the thread that must later drive
    /// [`pump`] and create every view — `CefRuntime` enforces that.
    ///
    /// Failure is not fatal: the editor route falls back to reporting the error
    /// in its window rather than bringing down the app.
    pub fn init_at_boot() -> Result<(), HostAvailability> {
        HOST.with(|cell| {
            let mut slot = cell.borrow_mut();
            ensure_runtime(&mut slot)
        })
    }

    /// Queue creation of the editor browser for `plugin_id` as a child of
    /// `parent_hwnd`.
    ///
    /// No CEF API is called here. This function is intentionally safe to invoke
    /// from a GPUI render/update: [`pump`] executes the native operation later,
    /// after GPUI has released its `AppCell` and entity borrows.
    pub fn open_view(
        view_id: ViewId,
        editor_id: &str,
        plugin_id: &str,
        parent_hwnd: u64,
        rect: ViewRect,
        scale_factor: f32,
        accelerated_sink: Option<AcceleratedFrameSink>,
    ) -> Result<(), HostAvailability> {
        match super::availability(plugin_id) {
            HostAvailability::Ready => {}
            other => return Err(other),
        }
        let Some(origin) = origin_for_plugin_id(plugin_id) else {
            return Err(HostAvailability::NoEditorForPlugin(plugin_id.to_string()));
        };
        // Windowed: `parent_hwnd` is the shell's content child and the browser
        // fills it. Off-screen: no native parent is required; one supplied
        // anyway is used only for monitor info and dialog ownership.
        if super::OFFSCREEN_HOSTING {
            WindowBounds::new(rect.x, rect.y, rect.width, rect.height)
        } else {
            windowed_fill_bounds(rect, scale_factor)
        }
        .map_err(|e| HostAvailability::RuntimeFailed(e.to_string()))?;

        let accelerated_sink = if crate::boot::has_flag("--disable-shared-texture")
            || std::env::var_os("FUTUREBOARD_DISABLE_SHARED_TEXTURE").is_some()
        {
            crate::boot::log("[GPU] shared-texture path disabled; using software OSR paint");
            None
        } else {
            accelerated_sink
        };

        COMMANDS.with(|commands| {
            let mut commands = commands.borrow_mut();
            let pending = commands.entry(view_id).or_default();
            if pending.close {
                return Err(HostAvailability::RuntimeFailed(
                    "editor view is already closing".to_string(),
                ));
            }
            pending.open = Some(PendingOpen {
                editor_id: editor_id.to_string(),
                origin,
                parent_hwnd,
                rect,
                accelerated_sink,
            });
            pending.bounds = Some(PendingBounds { rect, scale_factor });
            Ok(())
        })
    }

    /// Coalesce a browser resize for the next pump. An unknown id is allowed:
    /// the latest bounds are retained while its open command is still pending.
    pub fn set_view_bounds(view_id: ViewId, rect: ViewRect, scale_factor: f32) {
        if rect.width <= 0 || rect.height <= 0 {
            return;
        }
        COMMANDS.with(|commands| {
            let mut commands = commands.borrow_mut();
            let pending = commands.entry(view_id).or_default();
            if !pending.close {
                pending.bounds = Some(PendingBounds { rect, scale_factor });
            }
        });
    }

    /// Coalesce a desktop-placement update for the next pump. Applied before
    /// any bounds change in the same tick, so a browser that is both moving and
    /// resizing re-reads screen info against the placement it is landing on.
    pub fn set_view_screen_geometry(view_id: ViewId, geometry: ViewScreenGeometry) {
        COMMANDS.with(|commands| {
            let mut commands = commands.borrow_mut();
            let pending = commands.entry(view_id).or_default();
            if !pending.close {
                pending.screen_geometry = Some(geometry);
            }
        });
    }

    /// Logical size to hand an off-screen browser for a physical rect measured
    /// at `scale_factor`. Clamped to at least one pixel: a zero-sized view rect
    /// makes Chromium drop the browser's compositor frame entirely.
    fn logical_size(rect: ViewRect, scale_factor: f32) -> (i32, i32) {
        let scale = if scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        (
            ((rect.width as f32) / scale).round().max(1.0) as i32,
            ((rect.height as f32) / scale).round().max(1.0) as i32,
        )
    }

    /// Bounds for a windowed CEF child that fills its content parent.
    ///
    /// Win32 child HWNDs are physical pixels. AppKit frames are points, so a
    /// Retina physical rect has to be divided by the scale the shell measured
    /// it at — otherwise the browser is created at 2× and overflows the
    /// container.
    fn windowed_fill_bounds(
        rect: ViewRect,
        scale_factor: f32,
    ) -> Result<WindowBounds, CefRuntimeError> {
        #[cfg(target_os = "macos")]
        {
            let (width, height) = logical_size(rect, scale_factor);
            WindowBounds::new(0, 0, width, height)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = scale_factor;
            WindowBounds::new(rect.x, rect.y, rect.width, rect.height)
        }
    }

    /// Frame counter for `view_id`'s off-screen surface. `0` while the browser
    /// is windowed, absent, or has not painted yet.
    pub fn view_frame_generation(view_id: ViewId) -> u64 {
        HOST.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|slot| {
                    let host = slot.as_ref()?;
                    let hosted = host.views.get(&view_id)?;
                    Some(hosted.view.osr_surface()?.generation())
                })
                .unwrap_or(0)
        })
    }

    /// Whether the currently active browser for `view_id` is using CEF shared
    /// textures instead of software `OnPaint` frames.
    pub fn view_uses_accelerated_osr(view_id: ViewId) -> bool {
        HOST.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|slot| {
                    let host = slot.as_ref()?;
                    let hosted = host.views.get(&view_id)?;
                    Some(hosted.view.osr_surface()?.is_accelerated())
                })
                .unwrap_or(false)
        })
    }

    /// Read `view_id`'s latest off-screen frame (BGRA bytes, physical width,
    /// physical height). `None` for a windowed browser or before first paint.
    pub fn with_view_frame<R>(
        view_id: ViewId,
        read: impl FnOnce(&[u8], i32, i32) -> R,
    ) -> Option<R> {
        HOST.with(|cell| {
            let slot = cell.try_borrow().ok()?;
            let host = slot.as_ref()?;
            let hosted = host.views.get(&view_id)?;
            hosted.view.osr_surface()?.with_frame(read)
        })
    }

    /// Forward one input event to an off-screen browser. Silently ignored for
    /// a windowed browser, which receives real platform input directly.
    pub fn send_view_input(view_id: ViewId, input: EditorInput) {
        HOST.with(|cell| {
            let Ok(slot) = cell.try_borrow() else { return };
            let Some(host) = slot.as_ref() else { return };
            let Some(hosted) = host.views.get(&view_id) else {
                return;
            };
            if hosted.view.osr_surface().is_none() {
                return;
            }
            if let Err(error) = hosted.view.send_input(to_osr_input(input)) {
                eprintln!("[plugin-bridge] send_input failed view_id={view_id:?} err={error}");
            }
        });
    }

    fn to_osr_modifiers(modifiers: EditorModifiers) -> OsrModifiers {
        OsrModifiers {
            shift: modifiers.shift,
            control: modifiers.control,
            alt: modifiers.alt,
            command: modifiers.command,
            left_button: modifiers.left_button,
            middle_button: modifiers.middle_button,
            right_button: modifiers.right_button,
        }
    }

    fn to_osr_key(key: EditorKey) -> OsrKey {
        OsrKey {
            kind: match key.kind {
                EditorKeyKind::Down => OsrKeyKind::Down,
                EditorKeyKind::Up => OsrKeyKind::Up,
                EditorKeyKind::Char => OsrKeyKind::Char,
            },
            windows_key_code: key.windows_key_code,
            character: key.character,
            modifiers: to_osr_modifiers(key.modifiers),
        }
    }

    fn to_osr_input(input: EditorInput) -> OsrInput {
        match input {
            EditorInput::MouseMove {
                x,
                y,
                modifiers,
                leaving,
            } => OsrInput::MouseMove {
                x,
                y,
                modifiers: to_osr_modifiers(modifiers),
                leaving,
            },
            EditorInput::MouseButton {
                x,
                y,
                button,
                pressed,
                click_count,
                modifiers,
            } => OsrInput::MouseButton {
                x,
                y,
                button: match button {
                    EditorMouseButton::Left => OsrMouseButton::Left,
                    EditorMouseButton::Middle => OsrMouseButton::Middle,
                    EditorMouseButton::Right => OsrMouseButton::Right,
                },
                pressed,
                click_count,
                modifiers: to_osr_modifiers(modifiers),
            },
            EditorInput::MouseWheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => OsrInput::MouseWheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers: to_osr_modifiers(modifiers),
            },
            EditorInput::Key(key) => OsrInput::Key(to_osr_key(key)),
            EditorInput::Focus(focused) => OsrInput::Focus(focused),
            EditorInput::CaptureLost => OsrInput::CaptureLost,
        }
    }

    fn to_osr_rect(rect: ViewRect) -> sphere_webview::osr::OsrRect {
        sphere_webview::osr::OsrRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }

    fn to_osr_screen_geometry(
        geometry: ViewScreenGeometry,
    ) -> sphere_webview::osr::OsrScreenGeometry {
        sphere_webview::osr::OsrScreenGeometry {
            view_origin_physical: geometry.view_origin_physical,
            monitor_rect_dip: to_osr_rect(geometry.monitor_rect_dip),
            available_rect_dip: to_osr_rect(geometry.available_rect_dip),
        }
    }

    /// Mirror each hosted surface's paint counters into the shared OSR
    /// profiler.
    ///
    /// Called from the pump rather than from the paint callback: `SphereWebView`
    /// deliberately has no GPUI dependency, and the producer's critical path
    /// should not be taking the profiler's lock. Per-view deltas are tracked so
    /// closing an editor retires its history instead of stalling the totals.
    fn mirror_paint_counters(host: &Host) {
        use gpui::osr_profile::{self, Counter};
        if !osr_profile::enabled() {
            return;
        }
        MIRRORED_PAINTS.with(|mirrored| {
            let Ok(mut mirrored) = mirrored.try_borrow_mut() else {
                return;
            };
            for (view_id, hosted) in &host.views {
                let Some(surface) = hosted.view.osr_surface() else {
                    continue;
                };
                let (accelerated, software) = surface.paint_counts();
                let seen = mirrored.entry(*view_id).or_insert((0, 0));
                osr_profile::count(
                    Counter::AcceleratedPaints,
                    accelerated.saturating_sub(seen.0),
                );
                osr_profile::count(Counter::SoftwarePaints, software.saturating_sub(seen.1));
                *seen = (accelerated, software);
            }
            mirrored.retain(|view_id, _| host.views.contains_key(view_id));
        });
    }

    /// Queue a close. Close dominates an unprocessed open/resize for this unique
    /// view id, so closing a shell before its first pump never creates a browser
    /// against a dead parent HWND.
    pub fn close_view(view_id: ViewId) {
        COMMANDS.with(|commands| {
            let mut commands = commands.borrow_mut();
            let pending = commands.entry(view_id).or_default();
            pending.open = None;
            pending.bounds = None;
            pending.close = true;
        });
    }

    pub fn take_view_events(view_id: ViewId) -> Vec<ViewEvent> {
        EVENTS.with(|events| events.borrow_mut().remove(&view_id).unwrap_or_default())
    }

    pub fn browser_holds_native_parent(view_id: ViewId) -> bool {
        HOST.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|slot| {
                    slot.as_ref().map(|host| {
                        host.views.contains_key(&view_id)
                            || host.closing_views.contains_key(&view_id)
                            || host
                                .fallback_closing_views
                                .iter()
                                .any(|fallback| fallback.view_id == view_id)
                    })
                })
                .unwrap_or(false)
        })
    }

    pub fn is_view_open(view_id: ViewId) -> bool {
        let pending_open = COMMANDS.with(|commands| {
            commands
                .try_borrow()
                .ok()
                .and_then(|commands| {
                    commands
                        .get(&view_id)
                        .map(|pending| pending.open.is_some() && !pending.close)
                })
                .unwrap_or(false)
        });
        pending_open
            || HOST.with(|cell| {
                cell.try_borrow()
                    .ok()
                    .and_then(|slot| {
                        slot.as_ref().map(|host| {
                            host.views.contains_key(&view_id)
                                || host.fallback_closing_views.iter().any(|fallback| {
                                    fallback.view_id == view_id
                                        && !fallback.cancel_reopen
                                        && !fallback.timeout_reported
                                })
                        })
                    })
                    .unwrap_or(false)
            })
    }

    /// Execute queued native operations and advance CEF's message loop.
    ///
    /// Call this only from a GPUI foreground task *outside* `AsyncApp::update`.
    /// CEF may synchronously dispatch Win32 messages; keeping every GPUI borrow
    /// out of this stack is what prevents `AppCell` double-borrow panics.
    pub fn pump() {
        pump_impl(false);
    }

    /// Process a discrete user input event without waiting for the periodic
    /// editor tick. The caller schedules this after the GPUI event handler has
    /// released its entity/App borrows, so CEF may safely re-enter the platform
    /// message loop.
    pub fn pump_after_input() {
        pump_impl(true);
    }

    fn pump_impl(force: bool) {
        let Some(_guard) = PumpGuard::enter() else {
            return;
        };
        const MIN_PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);
        let (should_pump, stalled) = LAST_CEF_PUMP.with(|last| {
            let now = std::time::Instant::now();
            let stalled = last.get().is_some_and(|previous| {
                now.duration_since(previous) > std::time::Duration::from_secs(2)
            });
            let should_pump = if !force
                && last
                    .get()
                    .is_some_and(|previous| now.duration_since(previous) < MIN_PUMP_INTERVAL)
            {
                false
            } else {
                last.set(Some(now));
                true
            };
            (should_pump, stalled)
        });
        if !should_pump {
            return;
        }
        let commands = COMMANDS.with(|commands| std::mem::take(&mut *commands.borrow_mut()));
        let mut completed = Vec::new();
        let mut fallback_reopens = Vec::new();

        HOST.with(|cell| {
            let mut slot = cell.borrow_mut();
            if let Err(error) = ensure_runtime(&mut slot) {
                for (view_id, pending) in commands {
                    if pending.close {
                        completed.push((view_id, ViewEvent::Closed));
                    } else if pending.open.is_some() {
                        completed.push((view_id, ViewEvent::OpenFailed(error.to_string())));
                    }
                }
                return;
            }
            let host = slot.as_mut().expect("ensure_runtime installs the host");
            drop(std::mem::take(&mut host.deferred_release));
            if stalled {
                for hosted in host.views.values() {
                    if hosted.view.osr_surface().is_none()
                        && matches!(hosted.phase, super::BrowserPhase::Attached)
                    {
                        let _ = hosted.view.refresh_compositor();
                    }
                }
            }
            log_resource_counters(host);

            for (view_id, pending) in commands {
                if pending.close {
                    if let Some(fallback) = host
                        .fallback_closing_views
                        .iter_mut()
                        .find(|fallback| fallback.view_id == view_id)
                    {
                        fallback.cancel_reopen = true;
                        continue;
                    }
                    if let Some(mut hosted) = host.views.remove(&view_id) {
                        let browser_id = hosted.view.browser_identifier();
                        let origin = hosted.open.origin;
                        hosted.phase = super::transition_browser_phase(
                            hosted.phase,
                            super::BrowserPhase::Closing,
                        )
                        .unwrap_or(super::BrowserPhase::Closing);
                        let _ = hosted.view.close(true);
                        host.closing_views.insert(
                            view_id,
                            ClosingView {
                                hosted,
                                pump_ticks: 0,
                                timeout_logged: false,
                            },
                        );
                        if !origin_still_attached(host, origin) {
                            discard_inbound(origin);
                        }
                        eprintln!(
                            "[CEF][Browser {browser_id}] detach view_id={view_id:?} {}",
                            sphere_webview::runtime::thread_label()
                        );
                        eprintln!(
                            "[cef-registry] event=close_requested view_id={view_id:?} browser_id={browser_id} editor_count={} removal_deferred_until=OnBeforeClose",
                            host.views.len() + host.closing_views.len()
                        );
                    } else if !host.closing_views.contains_key(&view_id) {
                        // A close that canceled an unprocessed open has no native
                        // browser lifetime to wait for.
                        completed.push((view_id, ViewEvent::Closed));
                    }
                    continue;
                }

                let mut opened_now = false;
                if let Some(open) = pending.open {
                    if host.views.contains_key(&view_id) {
                        completed.push((view_id, ViewEvent::Opened));
                    } else {
                        let bounds_command = pending.bounds.unwrap_or(PendingBounds {
                            rect: open.rect,
                            scale_factor: 1.0,
                        });
                        let rect = bounds_command.rect;
                        let url = diagnostic_control_url().unwrap_or_else(|| {
                            format!("mikoplugin://{}/index.html", open.origin)
                        });
                        // Windowed: CEF creates its own child window inside
                        // `parent_hwnd` at the physical rect and handles DPI,
                        // painting, and input itself — no surface at all.
                        // Off-screen: CEF lays out in logical pixels and paints
                        // physical ones into the surface the client owns.
                        let screen_geometry = pending.screen_geometry.unwrap_or_default();
                        let surface = super::OFFSCREEN_HOSTING.then(|| {
                            let (width, height) =
                                logical_size(rect, bounds_command.scale_factor);
                            let surface = if let Some(sink) = open.accelerated_sink.clone() {
                                OsrSurface::new_accelerated(
                                    width,
                                    height,
                                    bounds_command.scale_factor,
                                    sink,
                                )
                            } else {
                                OsrSurface::new(width, height, bounds_command.scale_factor)
                            };
                            // Placement must be in place before the browser
                            // exists: Chromium queries `GetScreenInfo` during
                            // creation, and there is no notification that can
                            // retroactively fix the screen the page first sees.
                            surface.set_screen_geometry(to_osr_screen_geometry(screen_geometry));
                            surface
                        });
                        let (mut client, lifecycle) =
                            plugin_browser_client_with_surface(&url, surface.clone());
                        let result = if surface.is_some() {
                            WindowBounds::new(rect.x, rect.y, rect.width, rect.height)
                        } else {
                            windowed_fill_bounds(rect, bounds_command.scale_factor)
                        }
                        .map_err(|error| error.to_string())
                            .and_then(|bounds| {
                                let mut config = WebViewConfig::new(url, bounds);
                                if let Some(surface) = surface {
                                    config = config.windowless(surface);
                                }
                                // SAFETY: the returned view is stored in
                                // `host.views`, declared before `host.runtime`,
                                // and therefore released first.
                                unsafe {
                                    let parent =
                                        NativeParent::from_raw(hwnd_to_cef(open.parent_hwnd));
                                    host.runtime.create_webview_detached(
                                        parent,
                                        config,
                                        Some(&mut client),
                                    )
                                }
                                .map_err(|error| error.to_string())
                            });
                        eprintln!(
                            "[cef-ref] object_type=cef_client_t event=after_CreateBrowserSync has_one_ref={} has_at_least_one_ref={} thread={:?}",
                            client.has_one_ref(),
                            client.has_at_least_one_ref(),
                            std::thread::current().id()
                        );
                        match result {
                            Ok(view) if lifecycle.after_created() => {
                                if let Err(error) = view.set_zoom_level(0.0) {
                                    eprintln!(
                                        "[plugin-editor] failed to lock zoom view_id={view_id:?} err={error}"
                                    );
                                }
                                let browser_id = view.browser_identifier();
                                let editor_id = open.editor_id.clone();
                                host.views.insert(
                                    view_id,
                                    HostedView {
                                        _editor_id: editor_id.clone(),
                                        view,
                                        _client: client,
                                        lifecycle,
                                        open,
                                        bounds: bounds_command,
                                        screen_geometry,
                                        opened_at: std::time::Instant::now(),
                                        stability_reported: false,
                                        phase: super::BrowserPhase::Attached,
                                    },
                                );
                                eprintln!(
                                    "[CEF][Browser {browser_id}] attach view_id={view_id:?} editor={editor_id} {}",
                                    sphere_webview::runtime::thread_label()
                                );
                                eprintln!(
                                    "[cef-registry] event=insert source=OnAfterCreated view_id={view_id:?} browser_id={browser_id} editor_count={}",
                                    host.views.len() + host.closing_views.len()
                                );
                                opened_now = true;
                                completed.push((view_id, ViewEvent::Opened));
                            }
                            Ok(view) => {
                                let browser_id = view.browser_identifier();
                                let _ = view.close(true);
                                host.closing_views.insert(
                                    view_id,
                                    ClosingView {
                                        hosted: HostedView {
                                            _editor_id: open.editor_id.clone(),
                                            view,
                                            _client: client,
                                            lifecycle,
                                            open,
                                            bounds: bounds_command,
                                            screen_geometry,
                                            opened_at: std::time::Instant::now(),
                                            stability_reported: false,
                                            phase: super::BrowserPhase::Closing,
                                        },
                                        pump_ticks: 0,
                                        timeout_logged: false,
                                    },
                                );
                                completed.push((
                                    view_id,
                                    ViewEvent::OpenFailed(format!(
                                        "CreateBrowserSync returned browser {browser_id} before OnAfterCreated"
                                    )),
                                ));
                            }
                            Err(error) => {
                                completed.push((view_id, ViewEvent::OpenFailed(error)));
                            }
                        }
                    }
                }

                if !opened_now {
                    // Placement first: `NotifyScreenInfoChanged` makes Chromium
                    // re-read the handler synchronously, so the surface has to
                    // already describe where the view has landed.
                    let mut screen_info_stale = false;
                    if let Some(geometry) = pending.screen_geometry {
                        if let Some(hosted) = host.views.get_mut(&view_id) {
                            hosted.screen_geometry = geometry;
                            if let Some(surface) = hosted.view.osr_surface() {
                                screen_info_stale =
                                    surface.set_screen_geometry(to_osr_screen_geometry(geometry));
                            }
                        }
                    }
                    if let Some(PendingBounds { rect, scale_factor }) = pending.bounds {
                        if let Some(hosted) = host.views.get_mut(&view_id) {
                            hosted.bounds = PendingBounds { rect, scale_factor };
                            // A windowed child fills its content parent; an
                            // off-screen browser is told the logical size it
                            // should lay out at, and the scale it renders with.
                            let bounds = match hosted.view.osr_surface() {
                                Some(surface) => {
                                    let (width, height) = logical_size(rect, scale_factor);
                                    // Surface first, notifications after — CEF
                                    // reads back through the render handler from
                                    // inside both calls.
                                    let change =
                                        surface.set_view_size(width, height, scale_factor);
                                    screen_info_stale |= change.scale_changed;
                                    WindowBounds::new(0, 0, width, height)
                                }
                                None => windowed_fill_bounds(rect, scale_factor),
                            };
                            if let Ok(bounds) = bounds {
                                // `set_bounds` already issues `WasResized`, which
                                // re-reads the view rect but *not* screen info.
                                let _ = hosted.view.set_bounds(bounds);
                            }
                        }
                    }
                    if screen_info_stale {
                        if let Some(hosted) = host.views.get(&view_id) {
                            if let Err(error) = hosted.view.notify_screen_info_changed() {
                                eprintln!(
                                    "[plugin-bridge] notify_screen_info_changed failed view_id={view_id:?} err={error}"
                                );
                            }
                        }
                    }
                }
            }

            let _ = host.runtime.do_message_loop_work();
            mirror_paint_counters(host);

            // CEF cannot switch an existing browser from shared textures to
            // software OnPaint. If the synchronous D3D11 copy failed (adapter
            // mismatch, RDP, unsupported format, device loss), retire that
            // browser and transparently queue the same view without a GPU sink.
            let accelerated_failures = host
                .views
                .iter()
                .filter_map(|(view_id, hosted)| {
                    hosted
                        .view
                        .osr_surface()
                        .is_some_and(|surface| surface.take_accelerated_failure())
                        .then_some(*view_id)
                })
                .collect::<Vec<_>>();
            for view_id in accelerated_failures {
                let Some(hosted) = host.views.remove(&view_id) else {
                    continue;
                };
                let browser_id = hosted.view.browser_identifier();
                let mut reopen = hosted.open.clone();
                reopen.accelerated_sink = None;
                let bounds = hosted.bounds;
                let screen_geometry = hosted.screen_geometry;
                let _ = hosted.view.close(true);
                host.fallback_closing_views.push(FallbackClosingView {
                    closing: ClosingView {
                        hosted,
                        pump_ticks: 0,
                        timeout_logged: false,
                    },
                    view_id,
                    reopen,
                    bounds,
                    screen_geometry,
                    cancel_reopen: false,
                    timeout_reported: false,
                });
                completed.push((view_id, ViewEvent::AcceleratedFallback));
                eprintln!(
                    "[cef-osr] accelerated browser_id={browser_id} view_id={view_id:?} falling back to software OnPaint"
                );
            }

            // Renderer crash detection (`BrowserLifecycle`, set from
            // `on_render_process_terminated`). The browser object and native
            // DSP state survive a renderer crash — only the page's JS state
            // is gone — so this reloads the same URL rather than tearing the
            // window down; `ViewEvent::RendererCrashed` lets the GPUI window
            // reset `browser_ready` so it re-sends the current selection once
            // the fresh page announces `bridgeReady` again.
            let editor_count = host.views.len() + host.closing_views.len();
            for (view_id, hosted) in &mut host.views {
                if hosted.lifecycle.take_renderer_terminated() {
                    eprintln!("[plugin-scheme] reloading crashed renderer view_id={view_id:?}");
                    let _ = hosted.view.reload();
                    let _ = hosted.view.set_zoom_level(0.0);
                    completed.push((*view_id, ViewEvent::RendererCrashed));
                }
                if !hosted.stability_reported
                    && hosted.opened_at.elapsed() >= std::time::Duration::from_secs(60)
                {
                    hosted.stability_reported = true;
                    eprintln!(
                        "[cef-stability] browser_id={} view_id={view_id:?} elapsed_seconds=60 javascript_executed={} renderer_alive=true editor_count={}",
                        hosted.view.browser_identifier(),
                        hosted.lifecycle.javascript_executed(),
                        editor_count
                    );
                }
            }

            // `close_browser(true)` is asynchronous. Keep the WebView alive
            // until CEF confirms OnBeforeClose; only then may the GPUI window
            // consume `Closed`. The timeout is a bounded shutdown escape hatch
            // for a wedged renderer process.
            let mut closed = Vec::new();
            for (view_id, closing) in &mut host.closing_views {
                closing.pump_ticks = closing.pump_ticks.saturating_add(1);
                if closing.hosted.lifecycle.before_close() {
                    closed.push(*view_id);
                } else if closing.pump_ticks >= MAX_CLOSE_PUMP_TICKS && !closing.timeout_logged {
                    closing.timeout_logged = true;
                    let browser_id = closing.hosted.view.browser_identifier();
                    eprintln!(
                        "[CEF][Browser {browser_id}] close still pending view_id={view_id:?} pump_ticks={} — keeping the browser and its native parent until OnBeforeClose",
                        closing.pump_ticks
                    );
                }
            }
            for view_id in closed {
                let Some(closing) = host.closing_views.remove(&view_id) else {
                    continue;
                };
                let browser_id = closing.hosted.view.browser_identifier();
                let mut hosted = closing.hosted;
                hosted.phase = super::transition_browser_phase(
                    hosted.phase,
                    super::BrowserPhase::Closed,
                )
                .unwrap_or(super::BrowserPhase::Closed);
                host.deferred_release.push(hosted);
                eprintln!(
                    "[cef-registry] event=remove source=OnBeforeClose view_id={view_id:?} browser_id={browser_id} editor_count={} release=next-pump",
                    host.views.len() + host.closing_views.len()
                );
                completed.push((view_id, ViewEvent::Closed));
            }

            let mut index = 0;
            while index < host.fallback_closing_views.len() {
                let fallback = &mut host.fallback_closing_views[index];
                fallback.closing.pump_ticks = fallback.closing.pump_ticks.saturating_add(1);
                let before_close = fallback.closing.hosted.lifecycle.before_close();
                if before_close {
                    let fallback = host.fallback_closing_views.swap_remove(index);
                    let FallbackClosingView {
                        closing,
                        view_id,
                        reopen,
                        bounds,
                        screen_geometry,
                        cancel_reopen,
                        timeout_reported,
                    } = fallback;
                    host.deferred_release.push(closing.hosted);
                    if cancel_reopen {
                        completed.push((view_id, ViewEvent::Closed));
                    } else if !timeout_reported {
                        // This origin had exactly one shared built-in editor.
                        // Drop any late messages from the retired page before
                        // the replacement browser is allowed to post.
                        discard_inbound(reopen.origin);
                        fallback_reopens.push((view_id, reopen, bounds, screen_geometry));
                    }
                } else {
                    if fallback.closing.pump_ticks >= MAX_CLOSE_PUMP_TICKS
                        && !fallback.timeout_reported
                    {
                        fallback.timeout_reported = true;
                        completed.push((
                            fallback.view_id,
                            ViewEvent::OpenFailed(
                                "accelerated browser did not close before software fallback"
                                    .to_string(),
                            ),
                        ));
                    }
                    index += 1;
                }
            }
        });

        if !fallback_reopens.is_empty() {
            COMMANDS.with(|commands| {
                let mut commands = commands.borrow_mut();
                for (view_id, open, bounds, screen_geometry) in fallback_reopens {
                    let pending = commands.entry(view_id).or_default();
                    if !pending.close {
                        pending.open = Some(open);
                        pending.bounds = Some(bounds);
                        // Without this the replacement browser would come up
                        // reporting an unresolved screen until the window next
                        // moved, so its first popup would be misplaced.
                        pending.screen_geometry = Some(screen_geometry);
                    }
                }
            });
        }

        if !completed.is_empty() {
            EVENTS.with(|events| {
                let mut events = events.borrow_mut();
                for (view_id, event) in completed {
                    events.entry(view_id).or_default().push(event);
                }
            });
        }
    }

    /// On Windows `cef_window_handle_t` is cef-dll-sys's own `HWND` newtype,
    /// which is distinct from the `windows` crate's `HWND` the rest of the app
    /// passes around as a `u64`.
    #[cfg(target_os = "windows")]
    fn hwnd_to_cef(handle: u64) -> sphere_webview::runtime::cef::sys::cef_window_handle_t {
        sphere_webview::runtime::cef::sys::HWND(handle as *mut _)
    }

    #[cfg(not(target_os = "windows"))]
    fn hwnd_to_cef(handle: u64) -> sphere_webview::runtime::cef::sys::cef_window_handle_t {
        handle as _
    }

    /// Optional normal-page control. The exact URL is also whitelisted by the
    /// diagnostic client; absent this variable the plugin custom scheme is used.
    fn diagnostic_control_url() -> Option<String> {
        std::env::var("FUTUREBOARD_CEF_CONTROL_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
    }

    /// Opens Chromium's remote-debugging endpoint (`http://127.0.0.1:<port>`)
    /// so a real browser's devtools can inspect the editor's console/network
    /// when `FUTUREBOARD_PLUGIN_VIEW_DEBUG=1` — otherwise off, since it is a
    /// local unauthenticated debug surface.
    fn debug_port() -> Option<u16> {
        std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").map(|_| 9222)
    }
}

#[cfg(feature = "builtin-plugin-editor")]
pub use imp::install_process_app;
#[cfg(feature = "builtin-plugin-editor")]
pub use imp::shutdown;
pub use imp::{
    availability, browser_holds_native_parent, close_view, init_at_boot, is_view_open, open_view,
    preload, pump, pump_after_input, reload_view, send_to_view, send_view_input, set_view_bounds,
    set_view_screen_geometry, take_global_play_pause_requests, take_inbound, take_view_events,
    view_frame_generation, view_uses_accelerated_osr, with_view_frame,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_phase_rejects_use_after_close() {
        assert_eq!(
            transition_browser_phase(BrowserPhase::Creating, BrowserPhase::Ready),
            Some(BrowserPhase::Ready)
        );
        assert_eq!(
            transition_browser_phase(BrowserPhase::Ready, BrowserPhase::Attached),
            Some(BrowserPhase::Attached)
        );
        assert_eq!(
            transition_browser_phase(BrowserPhase::Attached, BrowserPhase::Closing),
            Some(BrowserPhase::Closing)
        );
        assert_eq!(
            transition_browser_phase(BrowserPhase::Closing, BrowserPhase::Closed),
            Some(BrowserPhase::Closed)
        );
        assert_eq!(
            transition_browser_phase(BrowserPhase::Closed, BrowserPhase::Attached),
            None
        );
        assert_eq!(
            transition_browser_phase(BrowserPhase::Closing, BrowserPhase::Ready),
            None
        );
        assert_eq!(
            transition_browser_phase(BrowserPhase::Closed, BrowserPhase::Closing),
            None
        );
    }

    #[test]
    fn five_hundred_open_close_cycles_end_closed() {
        for _ in 0..500 {
            let mut phase = BrowserPhase::Creating;
            for next in [
                BrowserPhase::Ready,
                BrowserPhase::Attached,
                BrowserPhase::Closing,
                BrowserPhase::Closed,
            ] {
                phase = transition_browser_phase(phase, next).expect("legal editor cycle");
            }
            assert_eq!(phase, BrowserPhase::Closed);
            assert!(transition_browser_phase(phase, BrowserPhase::Attached).is_none());
        }
    }

    #[test]
    fn built_in_ids_map_to_their_url_origin() {
        assert_eq!(
            origin_for_plugin_id("builtin:rodharerist"),
            Some("rodharerist")
        );
        assert_eq!(origin_for_plugin_id("builtin:equz8"), Some("equz8"));
        assert_eq!(origin_for_plugin_id("builtin:verbspace"), Some("verbspace"));
        assert_eq!(origin_for_plugin_id("builtin:echospace"), Some("echospace"));
        assert_eq!(origin_for_plugin_id("builtin:fa2a"), Some("fa2a"));
        assert_eq!(origin_for_plugin_id("builtin:fa76"), Some("fa76"));
        assert_eq!(origin_for_plugin_id("builtin:burnlimit"), Some("burnlimit"));
        assert_eq!(origin_for_plugin_id("builtin:clipper67"), Some("clipper67"));
        assert_eq!(origin_for_plugin_id("builtin:transient"), Some("transient"));
        assert_eq!(
            origin_for_plugin_id("builtin:mixstation"),
            Some("mixstation")
        );
    }

    /// Regression guard for the bug that left the editor unopenable in the real
    /// app: an insert slot stores the registry *class id* (`rodharerist`), not
    /// the catalog id (`builtin:rodharerist`), so both forms must resolve.
    #[test]
    fn the_class_id_form_stored_by_insert_slots_also_resolves() {
        assert_eq!(origin_for_plugin_id("rodharerist"), Some("rodharerist"));
        assert_eq!(origin_for_plugin_id("equz8"), Some("equz8"));
        assert_eq!(origin_for_plugin_id("verbspace"), Some("verbspace"));
        assert_eq!(origin_for_plugin_id("echospace"), Some("echospace"));
        assert_eq!(origin_for_plugin_id("fa2a"), Some("fa2a"));
        assert_eq!(origin_for_plugin_id("fa76"), Some("fa76"));
        assert_eq!(origin_for_plugin_id("burnlimit"), Some("burnlimit"));
        assert_eq!(origin_for_plugin_id("clipper67"), Some("clipper67"));
        assert_eq!(origin_for_plugin_id("transient"), Some("transient"));
        assert_eq!(origin_for_plugin_id("mixstation"), Some("mixstation"));
        assert_eq!(
            origin_for_plugin_id("rodharerist"),
            origin_for_plugin_id("builtin:rodharerist"),
            "both identifier forms must resolve to the same origin"
        );
    }

    /// Resolution is catalog-validated, not shape-based: an external plug-in
    /// whose class id merely lacks a prefix must not be mistaken for a built-in.
    #[test]
    fn unknown_unprefixed_ids_are_not_treated_as_built_ins() {
        assert_eq!(origin_for_plugin_id("SomeVst3ControllerClass"), None);
        assert_eq!(origin_for_plugin_id("builtin:not-a-real-plugin"), None);
    }

    /// Regression guard for the routing bug that made built-in editors do
    /// nothing: `open_insert_editor` must dispatch on the plugin id alone.
    ///
    /// Built-ins have no VST3 runtime instance, so their `runtime_state` sits at
    /// `NotLoaded` forever. The editor route therefore cannot depend on runtime
    /// state, load status, plugin path, or plugin format — only on the id. If
    /// this identification ever stops being self-contained, the built-in branch
    /// will fall through into the VST3 gate again and silently return.
    #[test]
    fn built_in_routing_depends_only_on_the_plugin_id() {
        // Both forms, since the editor route is reached from an insert slot.
        for id in [
            "builtin:rodharerist",
            "rodharerist",
            "builtin:equz8",
            "equz8",
            "builtin:mixstation",
            "mixstation",
        ] {
            assert!(
                SpherePluginHost::is_builtin_ref(id),
                "{id} must be routable without consulting runtime state"
            );
            assert!(origin_for_plugin_id(id).is_some());
        }
        for id in ["vst3:foo", "clap:bar", "", "definitely-not-builtin"] {
            assert!(!SpherePluginHost::is_builtin_ref(id));
        }
    }

    #[test]
    fn external_plugin_ids_have_no_origin() {
        assert_eq!(origin_for_plugin_id("vst3:some-plugin"), None);
        assert_eq!(origin_for_plugin_id("some-vst3-class"), None);
        assert_eq!(origin_for_plugin_id(""), None);
    }

    #[test]
    fn availability_explains_itself_rather_than_returning_a_bare_bool() {
        // Whatever the build config, a non-built-in never reports Ready and the
        // reason is always human-readable.
        let result = availability("vst3:whatever");
        assert_ne!(result, HostAvailability::Ready);
        assert!(!result.to_string().is_empty());
    }

    #[cfg(not(feature = "builtin-plugin-editor"))]
    #[test]
    fn without_the_feature_every_plugin_reports_not_compiled_in() {
        assert_eq!(
            availability("builtin:rodharerist"),
            HostAvailability::NotCompiledIn
        );
        assert!(HostAvailability::NotCompiledIn
            .to_string()
            .contains("builtin-plugin-editor"));
    }

    #[cfg(feature = "builtin-plugin-editor")]
    #[test]
    fn builtins_with_an_editor_are_hostable_and_the_rest_are_not() {
        // These embed a UI in any build that ran their build script against a
        // built dist; either way they must never be `NotCompiledIn` here.
        for id in ["builtin:rodharerist", "builtin:equz8", "builtin:mixstation"] {
            assert_ne!(availability(id), HostAvailability::NotCompiledIn);
        }
        // A catalogued built-in that ships no editor bundle is refused by name,
        // not reported as an empty asset table.
        assert_eq!(
            availability("builtin:compresser"),
            HostAvailability::NoEditorForPlugin("builtin:compresser".to_string())
        );
    }

    /// The mirror is keyed by insert slot id, which is not unique across
    /// plugins — one built-in must never read, replay, or persist another's
    /// state out of the same slot.
    #[cfg(feature = "builtin-plugin-editor")]
    #[test]
    fn the_state_mirror_never_hands_one_builtin_another_plugins_state() {
        let insert = "test-insert-state-mirror-isolation";
        let freq = builtin_param_index("equz8", "band1_freq").expect("band1_freq is an EQ-Z8 id");
        builtin_state_apply("equz8", insert, freq, 137.0);

        let bytes = builtin_state_bytes("equz8", insert).expect("EQ-Z8 owns this slot's state");
        let json = String::from_utf8(bytes).expect("state blobs are UTF-8 JSON");
        assert!(json.contains("137"), "the edit is missing from {json}");
        assert!(!builtin_state_replay("equz8", insert).is_empty());

        // Same slot id, different plugin: nothing to hand over.
        assert!(builtin_state_bytes("rodharerist", insert).is_none());
        assert!(builtin_state_replay("rodharerist", insert).is_empty());

        // Re-pointing the slot replaces the state rather than folding into it.
        builtin_state_apply("rodharerist", insert, 0, 0.5);
        assert!(builtin_state_bytes("equz8", insert).is_none());

        builtin_state_remove(insert);
        assert!(builtin_state_bytes("rodharerist", insert).is_none());
    }

    #[cfg(feature = "builtin-plugin-editor")]
    #[test]
    fn param_ids_resolve_per_plugin_and_never_cross_over() {
        // `outputDb` exists in EQ-Z8's table; asking rodharerist for it must not
        // silently resolve against the wrong plugin's indices.
        assert!(builtin_param_index("builtin:equz8", "band3_freq").is_some());
        assert!(builtin_param_index("builtin:equz8", "not-a-param").is_none());
        assert!(builtin_param_index("builtin:compresser", "band3_freq").is_none());
        assert_eq!(
            builtin_param_index("builtin:mixstation", "inputTrimDb"),
            Some(mixstation::ipc::INPUT_TRIM_INDEX)
        );
        assert!(builtin_param_index("builtin:mixstation", "not-a-param").is_none());
    }

    #[cfg(feature = "builtin-plugin-editor")]
    #[test]
    fn mixstation_state_defaults_apply_serialize_seed_and_replay() {
        let insert = "test-mixstation-state-round-trip";
        let trim = builtin_param_index("mixstation", "inputTrimDb").expect("trim index");
        builtin_state_apply("mixstation", insert, trim, 7.5);

        let bytes = builtin_state_bytes("mixstation", insert).expect("state serializes");
        let state = mixstation::ipc::MixStationState::from_json(
            std::str::from_utf8(&bytes).expect("state is UTF-8"),
        )
        .expect("state parses");
        assert_eq!(state.params.input_trim_db, 7.5);
        assert_eq!(
            builtin_state_replay("mixstation", insert)
                .into_iter()
                .find(|(index, _)| *index == trim),
            Some((trim, 7.5))
        );

        let seeded_insert = "test-mixstation-state-seed";
        builtin_state_seed("builtin:mixstation", seeded_insert, &bytes);
        let seeded = builtin_state_bytes("mixstation", seeded_insert).expect("seed persisted");
        let seeded_state = mixstation::ipc::MixStationState::from_json(
            std::str::from_utf8(&seeded).expect("seed is UTF-8"),
        )
        .expect("seed parses");
        assert_eq!(seeded_state.params.input_trim_db, 7.5);

        builtin_state_remove(insert);
        builtin_state_remove(seeded_insert);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn desktop_hosts_builtin_editors_as_native_children() {
        assert!(
            !OFFSCREEN_HOSTING,
            "Windows and macOS must host built-in editors windowed so Chromium owns paint and input"
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn linux_hosts_builtin_editors_off_screen() {
        assert!(OFFSCREEN_HOSTING);
    }
}
