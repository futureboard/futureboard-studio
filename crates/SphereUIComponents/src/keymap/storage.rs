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
    /// `{action: key}`; a `null` key takes the action's base key away.
    Map(HashMap<String, Option<String>>),
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
                keys: keys.into_iter().collect(),
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
    write_file_atomically(path, profile_json_text(profile).as_bytes())
        .map_err(|error| format!("Failed to write keymap: {error}"))
}

/// Replace `path` with `bytes` atomically: the bytes go to a uniquely named
/// temporary file in the same folder, are flushed to disk, and the temporary
/// file is then renamed over `path` (on Windows `std::fs::rename` replaces an
/// existing file). A crash or power loss mid-save leaves either the old file
/// or the new one, never a truncated mix. The parent folder must exist.
pub fn write_file_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy();
    let temp = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Why the user-overrides file could not be used as it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserOverridesIssue {
    /// What went wrong, worded for the keymap window.
    pub message: String,
    /// Where a copy of the unreadable file was kept. `None` means no copy
    /// could be made, so saving must not replace the file.
    pub backup: Option<PathBuf>,
}

impl UserOverridesIssue {
    /// Whether a save would destroy the only copy of the user's file.
    pub fn blocks_save(&self) -> bool {
        self.backup.is_none()
    }
}

/// The user overrides as read from disk, and why they could not be read.
#[derive(Debug, Clone)]
pub struct UserOverridesLoad {
    pub profile: KeymapProfile,
    pub issue: Option<UserOverridesIssue>,
}

fn empty_user_overrides() -> KeymapProfile {
    KeymapProfile {
        name: "User Overrides".to_string(),
        extends: Some("default".to_string()),
        ..KeymapProfile::default()
    }
}

/// Read the user-overrides file. A missing file is an empty profile. A file
/// that cannot be read or parsed also starts empty, but is never silently
/// thrown away: it is copied aside first (see [`back_up_unreadable`]), and if
/// that copy fails the returned issue blocks saving over it.
pub fn load_user_overrides(app_data: &Path) -> UserOverridesLoad {
    let path = user_overrides_path(app_data);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return UserOverridesLoad {
                profile: empty_user_overrides(),
                issue: None,
            };
        }
        Err(error) => {
            eprintln!("[keymap] failed to read user overrides: {error}");
            return UserOverridesLoad {
                profile: empty_user_overrides(),
                issue: Some(UserOverridesIssue {
                    message: format!(
                        "Custom shortcuts could not be read from {} ({error}). \
                         They are not shown, and changes will not be saved over that file.",
                        path.display()
                    ),
                    backup: None,
                }),
            };
        }
    };
    let parsed = std::str::from_utf8(&bytes)
        .map_err(|error| format!("Invalid keymap text: {error}"))
        .and_then(load_profile_json);
    let error = match parsed {
        Ok(profile) => {
            return UserOverridesLoad {
                profile,
                issue: None,
            }
        }
        Err(error) => error,
    };
    eprintln!("[keymap] failed to load user overrides: {error}");
    let issue = match back_up_unreadable(&path, &bytes) {
        Ok(backup) => UserOverridesIssue {
            message: format!(
                "Custom shortcuts file was unreadable ({error}). \
                 It was copied to {} before starting with no custom shortcuts.",
                backup.display()
            ),
            backup: Some(backup),
        },
        Err(backup_error) => {
            eprintln!("[keymap] failed to back up user overrides: {backup_error}");
            UserOverridesIssue {
                message: format!(
                    "Custom shortcuts file {} is unreadable ({error}) and could not be \
                     backed up ({backup_error}). Changes will not be saved over it.",
                    path.display()
                ),
                backup: None,
            }
        }
    };
    UserOverridesLoad {
        profile: empty_user_overrides(),
        issue: Some(issue),
    }
}

/// Copy an unreadable keymap file next to itself as
/// `<name>.unreadable.bak` (or `<name>.unreadable-N.bak`), reusing a backup
/// that already holds the same bytes so reloading does not pile up copies,
/// and never replacing a different earlier backup.
fn back_up_unreadable(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    for index in 0..100u32 {
        let name = if index == 0 {
            format!("{file_name}.unreadable.bak")
        } else {
            format!("{file_name}.unreadable-{index}.bak")
        };
        let candidate = path.with_file_name(name);
        match fs::read(&candidate) {
            Ok(existing) if existing == bytes => return Ok(candidate),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_file_atomically(&candidate, bytes)?;
                return Ok(candidate);
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many earlier keymap backups",
    ))
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
    fn a_v1_null_unbinds() {
        let profile =
            load_profile_json(r#"{ "version": 1, "bindings": { "tools:select-glue": null } }"#)
                .unwrap();
        assert!(profile.bindings[0].keys.is_empty());
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fb-keymap-storage-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(user_keymaps_dir(&dir)).unwrap();
        dir
    }

    fn sample_overrides() -> KeymapProfile {
        load_profile_json(
            r#"{ "version": 2, "name": "User Overrides", "extends": "default",
                 "global": { "transport:record": "F9" } }"#,
        )
        .unwrap()
    }

    #[test]
    fn user_overrides_round_trip_and_leave_no_temp_files() {
        let dir = scratch_dir("roundtrip");
        save_user_overrides(&dir, &sample_overrides()).unwrap();
        // A second save replaces the existing file.
        save_user_overrides(&dir, &sample_overrides()).unwrap();
        let loaded = load_user_overrides(&dir);
        assert!(loaded.issue.is_none());
        assert_eq!(loaded.profile.bindings, sample_overrides().bindings);
        let names: Vec<_> = fs::read_dir(user_keymaps_dir(&dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(USER_OVERRIDES_FILE)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_overrides_file_is_not_an_issue() {
        let dir = scratch_dir("missing");
        let loaded = load_user_overrides(&dir);
        assert!(loaded.issue.is_none());
        assert!(loaded.profile.bindings.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_overrides_file_is_backed_up_and_reported() {
        let dir = scratch_dir("torn");
        let path = user_overrides_path(&dir);
        let torn = br#"{ "version": 2, "global": { "transport:rec"#;
        fs::write(&path, torn).unwrap();

        let loaded = load_user_overrides(&dir);
        let issue = loaded.issue.expect("a torn file is reported");
        assert!(!issue.blocks_save());
        let backup = issue.backup.unwrap();
        assert_eq!(fs::read(&backup).unwrap(), torn);
        assert_eq!(fs::read(&path).unwrap(), torn, "the original is untouched");

        // Loading again reuses the same backup instead of piling up copies.
        let again = load_user_overrides(&dir).issue.unwrap();
        assert_eq!(again.backup.as_deref(), Some(backup.as_path()));

        // A different unreadable file never replaces the earlier backup.
        fs::write(&path, b"not json").unwrap();
        let other = load_user_overrides(&dir).issue.unwrap().backup.unwrap();
        assert_ne!(other, backup);
        assert_eq!(fs::read(&backup).unwrap(), torn);
        assert_eq!(fs::read(&other).unwrap(), b"not json");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_atomic_write_replaces_the_target_whole() {
        let dir = scratch_dir("atomic");
        let path = dir.join("target.json");
        fs::write(&path, b"old contents that are longer").unwrap();
        write_file_atomically(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(&dir);
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
