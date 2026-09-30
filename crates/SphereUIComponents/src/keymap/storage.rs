use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::model::{KeyBinding, KeymapProfile, KeymapScope, USER_OVERRIDES_FILE};

#[derive(Debug, Clone, Deserialize)]
struct LegacyKeymapFile {
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    bindings: LegacyBindings,
}

impl Default for LegacyBindings {
    fn default() -> Self {
        LegacyBindings::Map(HashMap::new())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum LegacyBindings {
    Map(HashMap<String, String>),
    List(Vec<KeyBinding>),
}

pub fn user_keymaps_dir(app_data: &Path) -> PathBuf {
    app_data.join("Keymaps")
}

pub fn ensure_user_keymaps_dir(app_data: &Path) -> std::io::Result<PathBuf> {
    let dir = user_keymaps_dir(app_data);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn user_overrides_path(app_data: &Path) -> PathBuf {
    user_keymaps_dir(app_data).join(USER_OVERRIDES_FILE)
}

/// Read a keymap file.
///
/// Version 2 groups bindings by scope — `{"global": {…}, "midiEditor": {…}}` —
/// and each binding keeps its scope as its `context`. A value is one
/// accelerator, a list of them, or `null`/`[]` for "unbound here" (how a
/// profile or an override takes a default key away).
///
/// Version 1 files — the other bundled DAW profiles and user overrides saved
/// before scopes — are a flat `{action: key}` map or a binding list. Their
/// bindings carry no scope; the manager places them where the base profile
/// already binds that action (see `KeymapManager`).
pub fn load_profile_json(text: &str) -> Result<KeymapProfile, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("Invalid keymap JSON: {error}"))?;
    let is_scoped = value.as_object().is_some_and(|object| {
        KeymapScope::ALL
            .iter()
            .any(|s| object.contains_key(s.key()))
    });
    if is_scoped {
        return load_scoped_profile(&value);
    }

    let raw: LegacyKeymapFile =
        serde_json::from_value(value).map_err(|error| format!("Invalid keymap JSON: {error}"))?;
    let name = first_non_empty([raw.name, raw.label, raw.id]);
    let bindings = match raw.bindings {
        LegacyBindings::Map(map) => map
            .into_iter()
            .map(|(action, keys)| KeyBinding {
                action,
                keys: vec![keys],
                context: None,
                args: None,
                when: None,
            })
            .collect(),
        LegacyBindings::List(list) => list,
    };
    Ok(KeymapProfile {
        name,
        extends: raw.extends,
        version: raw.version.map(|v| v.to_string()),
        bindings,
    })
}

fn first_non_empty(candidates: [String; 3]) -> String {
    candidates
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

fn load_scoped_profile(value: &Value) -> Result<KeymapProfile, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Invalid keymap JSON: expected an object".to_string())?;
    let text_field = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut bindings = Vec::new();
    for scope in KeymapScope::ALL {
        let Some(entries) = object.get(scope.key()) else {
            continue;
        };
        let entries = entries
            .as_object()
            .ok_or_else(|| format!("Invalid keymap JSON: \"{}\" must be an object", scope.key()))?;
        for (action, keys) in entries {
            let keys = match keys {
                Value::Null => Vec::new(),
                Value::String(key) => vec![key.clone()],
                Value::Array(list) => list
                    .iter()
                    .map(|key| {
                        key.as_str().map(str::to_string).ok_or_else(|| {
                            format!(
                                "Invalid keymap JSON: {action} in {} lists a non-string key",
                                scope.key()
                            )
                        })
                    })
                    .collect::<Result<_, _>>()?,
                _ => {
                    return Err(format!(
                        "Invalid keymap JSON: {action} in {} must be a key, a list of keys or null",
                        scope.key()
                    ));
                }
            };
            bindings.push(KeyBinding {
                action: action.clone(),
                keys,
                context: Some(scope.key().to_string()),
                args: None,
                when: None,
            });
        }
    }
    Ok(KeymapProfile {
        name: first_non_empty([text_field("name"), text_field("label"), text_field("id")]),
        extends: object
            .get("extends")
            .and_then(Value::as_str)
            .map(str::to_string),
        version: Some("2".to_string()),
        bindings,
    })
}

/// A profile as version-2 JSON: header fields first, then one object per
/// scope in [`KeymapScope::ALL`] order, actions sorted within it. Written by
/// hand rather than through a map so the file reads top-down the way the
/// bundled default does.
pub fn profile_json_text(profile: &KeymapProfile) -> String {
    let quote = |text: &str| Value::String(text.to_string()).to_string();
    let mut by_scope: Vec<(KeymapScope, Vec<&KeyBinding>)> =
        KeymapScope::ALL.iter().map(|s| (*s, Vec::new())).collect();
    for binding in &profile.bindings {
        let scope = binding
            .context
            .as_deref()
            .and_then(KeymapScope::parse)
            .unwrap_or_else(|| KeymapScope::home_of(&binding.action));
        if let Some((_, list)) = by_scope.iter_mut().find(|(s, _)| *s == scope) {
            list.push(binding);
        }
    }

    let mut out = String::from("{\n  \"version\": 2");
    out.push_str(&format!(",\n  \"name\": {}", quote(&profile.name)));
    if let Some(extends) = &profile.extends {
        out.push_str(&format!(",\n  \"extends\": {}", quote(extends)));
    }
    for (scope, mut list) in by_scope {
        if list.is_empty() {
            continue;
        }
        list.sort_by(|a, b| a.action.cmp(&b.action));
        out.push_str(&format!(",\n  {}: {{", quote(scope.key())));
        for (index, binding) in list.iter().enumerate() {
            let keys = match binding.keys.as_slice() {
                [] => "null".to_string(),
                [key] => quote(key),
                keys => format!(
                    "[{}]",
                    keys.iter().map(|k| quote(k)).collect::<Vec<_>>().join(", ")
                ),
            };
            let comma = if index + 1 < list.len() { "," } else { "" };
            out.push_str(&format!("\n    {}: {keys}{comma}", quote(&binding.action)));
        }
        out.push_str("\n  }");
    }
    out.push_str("\n}\n");
    out
}

pub fn save_profile_json(profile: &KeymapProfile, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create keymap folder: {error}"))?;
    }
    fs::write(path, profile_json_text(profile))
        .map_err(|error| format!("Failed to write keymap: {error}"))
}

pub fn load_user_overrides(app_data: &Path) -> KeymapProfile {
    let path = user_overrides_path(app_data);
    let Ok(text) = fs::read_to_string(&path) else {
        return KeymapProfile {
            name: "User Overrides".to_string(),
            extends: Some("default".to_string()),
            ..KeymapProfile::default()
        };
    };
    load_profile_json(&text).unwrap_or_else(|error| {
        eprintln!("[keymap] failed to load user overrides: {error}");
        KeymapProfile {
            name: "User Overrides".to_string(),
            extends: Some("default".to_string()),
            ..KeymapProfile::default()
        }
    })
}

pub fn save_user_overrides(app_data: &Path, profile: &KeymapProfile) -> Result<(), String> {
    let path = user_overrides_path(app_data);
    save_profile_json(profile, &path)
}

pub fn builtin_profile_json(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        "default" => Some(include_str!("../../../../packages/keymaps/default.json")),
        "futureboard" => Some(include_str!(
            "../../../../packages/keymaps/futureboard.json"
        )),
        "fl-studio" => Some(include_str!("../../../../packages/keymaps/fl_studio.json")),
        "ableton-live" => Some(include_str!("../../../../packages/keymaps/ableton.json")),
        "cubase" => Some(include_str!("../../../../packages/keymaps/cubase.json")),
        "pro-tools" => Some(include_str!("../../../../packages/keymaps/pro_tools.json")),
        _ => None,
    }
}

pub fn load_builtin_profile(profile_id: &str) -> Result<KeymapProfile, String> {
    let text = builtin_profile_json(profile_id)
        .ok_or_else(|| format!("Unknown builtin profile: {profile_id}"))?;
    load_profile_json(text)
}

pub fn import_profile_file(path: &Path) -> Result<KeymapProfile, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("Failed to read file: {error}"))?;
    load_profile_json(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scoped_profile_round_trips() {
        let text = r#"{
            "version": 2, "name": "Mine",
            "global": { "transport:play-pause": "Space", "edit:redo": ["Ctrl+Shift+Z", "Ctrl+Y"] },
            "midiEditor": { "midi:quantize": "Q", "midi:fit-notes": null }
        }"#;
        let profile = load_profile_json(text).unwrap();
        assert_eq!(profile.name, "Mine");
        let find = |action: &str| {
            profile
                .bindings
                .iter()
                .find(|b| b.action == action)
                .unwrap()
                .clone()
        };
        assert_eq!(find("edit:redo").keys, ["Ctrl+Shift+Z", "Ctrl+Y"]);
        assert_eq!(find("midi:quantize").context.as_deref(), Some("midiEditor"));
        assert!(find("midi:fit-notes").keys.is_empty(), "null unbinds");

        let again = load_profile_json(&profile_json_text(&profile)).unwrap();
        let mut a = profile.bindings.clone();
        let mut b = again.bindings.clone();
        a.sort_by(|x, y| x.action.cmp(&y.action));
        b.sort_by(|x, y| x.action.cmp(&y.action));
        assert_eq!(a, b);
    }

    #[test]
    fn a_v1_map_still_loads_without_scopes() {
        let profile = load_profile_json(
            r#"{ "version": 1, "label": "Old", "bindings": { "transport:record": "F9" } }"#,
        )
        .unwrap();
        assert_eq!(profile.name, "Old");
        assert_eq!(profile.bindings[0].keys, ["F9"]);
        assert_eq!(profile.bindings[0].context, None);
    }

    #[test]
    fn the_bundled_profiles_all_load() {
        for id in [
            "default",
            "futureboard",
            "fl-studio",
            "ableton-live",
            "cubase",
            "pro-tools",
        ] {
            load_builtin_profile(id).unwrap_or_else(|e| panic!("{id}: {e}"));
        }
    }
}
