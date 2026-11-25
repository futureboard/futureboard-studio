//! The parameter and preset tables as NUL-terminated strings, built once and
//! kept for the life of the process, so the C side can hold the pointers.

use std::ffi::{CString, c_char};
use std::sync::OnceLock;

/// One wire parameter, as the C side sees it.
pub(crate) struct ParamRow {
    pub id: CString,
    pub name: CString,
    pub unit: CString,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub flags: u32,
}

/// [`ParamRow::flags`]: the descriptor gives this parameter a name and range.
pub(crate) const PARAM_DESCRIBED: u32 = 1;
/// …and it is a switch (0 or 1).
pub(crate) const PARAM_TOGGLE: u32 = 2;
/// …and it picks one of a list (whole numbers from `min` to `max`).
pub(crate) const PARAM_CHOICE: u32 = 4;
/// Not a setting: writing it performs an action (`clear_clip`).
pub(crate) const PARAM_ACTION: u32 = 8;

fn c(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}

pub(crate) fn params() -> &'static [ParamRow] {
    static ROWS: OnceLock<Vec<ParamRow>> = OnceLock::new();
    ROWS.get_or_init(|| {
        let descriptor = rodharerist::descriptor();
        let defaults = rodharerist::ui_values(&rodharerist::default_params());
        rodharerist::UI_PARAM_IDS
            .iter()
            .map(|&id| {
                let default = defaults
                    .iter()
                    .find(|(candidate, _)| *candidate == id)
                    .map_or(0.0, |(_, value)| *value);
                match descriptor.params.iter().find(|param| param.id == id) {
                    Some(param) => {
                        let mut flags = PARAM_DESCRIBED;
                        match param.unit {
                            "bool" => flags |= PARAM_TOGGLE,
                            "mode" | "model" | "division" | "channel" | "row" | "stage"
                            | "type" => flags |= PARAM_CHOICE,
                            _ => {}
                        }
                        ParamRow {
                            id: c(id),
                            name: c(param.name),
                            unit: c(param.unit),
                            min: param.min,
                            max: param.max,
                            default: param.default_value,
                            flags,
                        }
                    }
                    None => ParamRow {
                        id: c(id),
                        name: c(id),
                        unit: c(""),
                        min: 0.0,
                        max: 0.0,
                        default,
                        flags: if id == "clear_clip" { PARAM_ACTION } else { 0 },
                    },
                }
            })
            .collect()
    })
}

pub(crate) struct PresetRow {
    pub id: CString,
    pub name: CString,
}

pub(crate) fn presets() -> &'static [PresetRow] {
    static ROWS: OnceLock<Vec<PresetRow>> = OnceLock::new();
    ROWS.get_or_init(|| {
        rodharerist::factory_presets()
            .iter()
            .map(|preset| PresetRow {
                id: c(preset.id),
                name: c(preset.name),
            })
            .collect()
    })
}

/// The library's version, NUL-terminated.
pub(crate) fn version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}
