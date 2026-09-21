use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use gpui::KeyDownEvent;

use crate::menu::{MenuItem, MenuManifest};

use super::conflicts::{annotate_row_conflicts, find_conflicts_for_binding};
use super::model::{
    KeyBinding, KeymapConflict, KeymapProfile, KeymapRow, KeymapSource, ResolvedKeyBinding,
    PROFILE_DESCRIPTORS,
};
use super::normalize::{canonical_accel, format_accel_display, global_priority};
use super::storage::{
    ensure_user_keymaps_dir, import_profile_file, load_builtin_profile, load_user_overrides,
    save_profile_json, save_user_overrides, user_keymaps_dir, user_overrides_path,
};

pub fn shortcut_debug_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_SHORTCUT_DEBUG").is_some())
}

#[derive(Debug, Clone)]
pub struct KeymapManager {
    app_data: PathBuf,
    active_profile_id: String,
    user_overrides: KeymapProfile,
    imported_profile: Option<KeymapProfile>,
    dirty: bool,
    action_labels: HashMap<String, String>,
    resolved: Vec<ResolvedKeyBinding>,
    rows: Vec<KeymapRow>,
    reverse: HashMap<String, String>,
}

impl Default for KeymapManager {
    fn default() -> Self {
        Self::new(std::env::temp_dir())
    }
}

impl KeymapManager {
    pub fn new(app_data: PathBuf) -> Self {
        let _ = ensure_user_keymaps_dir(&app_data);
        let action_labels = build_action_catalog();
        let user_overrides = load_user_overrides(&app_data);
        let mut manager = Self {
            app_data,
            active_profile_id: "default".to_string(),
            user_overrides,
            imported_profile: None,
            dirty: false,
            action_labels,
            resolved: Vec::new(),
            rows: Vec::new(),
            reverse: HashMap::new(),
        };
        manager.rebuild();
        manager
    }

    pub fn active_profile_id(&self) -> &str {
        &self.active_profile_id
    }

    pub fn active_profile_label(&self) -> &str {
        PROFILE_DESCRIPTORS
            .iter()
            .find(|p| p.id == self.active_profile_id)
            .map(|p| p.label)
            .unwrap_or("Default")
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn rows(&self) -> &[KeymapRow] {
        &self.rows
    }

    pub fn conflict_count(&self) -> usize {
        self.rows.iter().filter(|row| row.is_conflict).count()
    }

    pub fn filtered_rows(&self, query: &str) -> Vec<&KeymapRow> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return self.rows.iter().collect();
        }
        self.rows
            .iter()
            .filter(|row| row_matches_query(row, &query))
            .collect()
    }

    pub fn set_active_profile(&mut self, profile_id: &str) -> Result<(), String> {
        if !PROFILE_DESCRIPTORS.iter().any(|p| p.id == profile_id) {
            return Err(format!("Unknown profile: {profile_id}"));
        }
        if profile_id == self.active_profile_id {
            return Ok(());
        }
        self.active_profile_id = profile_id.to_string();
        if profile_id != "custom" {
            self.imported_profile = None;
        }
        self.rebuild();
        Ok(())
    }

    pub fn tap_binding(
        &mut self,
        action: &str,
        keys: Vec<String>,
        context: Option<String>,
        args: Option<serde_json::Value>,
        force: bool,
    ) -> Result<Vec<KeymapConflict>, String> {
        let candidate = KeyBinding {
            action: action.to_string(),
            keys,
            context: context.or_else(|| Some("Studio".to_string())),
            args,
            when: None,
        };
        let conflicts = find_conflicts_for_binding(&candidate, &self.resolved, Some(action));
        if !conflicts.is_empty() && !force {
            return Ok(conflicts);
        }
        upsert_override(&mut self.user_overrides, candidate);
        self.dirty = true;
        self.rebuild();
        Ok(conflicts)
    }

    pub fn reset_binding(&mut self, action: &str) {
        self.user_overrides
            .bindings
            .retain(|binding| binding.action != action);
        self.dirty = true;
        self.rebuild();
    }

    pub fn import_profile(&mut self, path: &std::path::Path) -> Result<(), String> {
        let profile = import_profile_file(path)?;
        self.imported_profile = Some(profile);
        self.active_profile_id = "custom".to_string();
        self.dirty = true;
        self.rebuild();
        Ok(())
    }

    pub fn export_active_profile(&self, path: &std::path::Path) -> Result<(), String> {
        let profile = self.export_profile();
        save_profile_json(&profile, path)
    }

    pub fn export_profile(&self) -> KeymapProfile {
        if self.active_profile_id == "custom" {
            if let Some(imported) = &self.imported_profile {
                return imported.clone();
            }
            return self.user_overrides.clone();
        }
        let mut profile = effective_base_profile(&self.active_profile_id)
            .unwrap_or_else(|_| KeymapProfile::default());
        profile.name = self.active_profile_label().to_string();
        profile.extends = Some(self.active_profile_id.clone());
        profile.bindings = self.user_overrides.bindings.to_vec();
        profile
    }

    pub fn save_changes(&mut self) -> Result<(), String> {
        save_user_overrides(&self.app_data, &self.user_overrides)?;
        self.dirty = false;
        Ok(())
    }

    pub fn discard_dirty(&mut self) {
        self.user_overrides = load_user_overrides(&self.app_data);
        self.dirty = false;
        self.rebuild();
    }

    pub fn load_json_text(&mut self, text: &str) -> Result<(), String> {
        let profile = super::storage::load_profile_json(text)?;
        self.user_overrides = profile;
        self.dirty = true;
        self.rebuild();
        Ok(())
    }

    pub fn json_text(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&self.export_profile())
            .map_err(|error| format!("Failed to serialize keymap: {error}"))
    }

    pub fn command_for_event(&self, event: &KeyDownEvent) -> Option<&str> {
        if event.is_held {
            return None;
        }
        let token = super::normalize::canonical_event(event)?;
        let command = self.reverse.get(&token).map(String::as_str);
        if shortcut_debug_enabled() {
            eprintln!(
                "[shortcut] resolve profile={} token={} -> {:?}",
                self.active_profile_id, token, command
            );
        }
        command
    }

    /// Display string for the accelerator bound to `command` under the active
    /// profile, e.g. `"Ctrl+D"` — or `None` when the command has no binding.
    ///
    /// Used to backfill shortcut hints on context-menu items so every surface
    /// that dispatches a command can surface its key binding without hardcoding
    /// it. The first resolved keystroke wins, matching the menubar convention.
    pub fn shortcut_for_command(&self, command: &str) -> Option<String> {
        let binding = self.resolved.iter().find(|b| b.action == command)?;
        let key = binding.keys.iter().find(|key| !key.trim().is_empty())?;
        Some(format_keystroke_list(std::slice::from_ref(key)))
    }

    /// The raw authored accelerator (e.g. `"Ctrl+Z"`) bound to `command` under
    /// the active profile, before any platform display formatting. Style-neutral,
    /// so tests and lookups can assert bindings without depending on the host OS
    /// or the `FUTUREBOARD_ACCEL_STYLE` env override.
    #[allow(dead_code)]
    pub fn raw_accel_for_command(&self, command: &str) -> Option<String> {
        let binding = self.resolved.iter().find(|b| b.action == command)?;
        binding
            .keys
            .iter()
            .find(|key| !key.trim().is_empty())
            .cloned()
    }

    pub fn rebuild(&mut self) {
        self.resolved = resolve_effective_bindings(
            &self.active_profile_id,
            &self.user_overrides,
            self.imported_profile.as_ref(),
        );
        self.rows = build_rows(&self.action_labels, &self.resolved, &self.active_profile_id);
        annotate_row_conflicts(&mut self.rows, &self.resolved);
        self.reverse = build_reverse_index(&self.resolved);
    }

    pub fn user_keymaps_dir(&self) -> PathBuf {
        user_keymaps_dir(&self.app_data)
    }

    pub fn dispatch_reverse(&self) -> &std::collections::HashMap<String, String> {
        &self.reverse
    }

    pub fn user_overrides_path(&self) -> PathBuf {
        user_overrides_path(&self.app_data)
    }
}

fn row_matches_query(row: &KeymapRow, query: &str) -> bool {
    row.action_label.to_ascii_lowercase().contains(query)
        || row.action_id.to_ascii_lowercase().contains(query)
        || row.command.to_ascii_lowercase().contains(query)
        || row
            .keystrokes
            .iter()
            .any(|key| key.to_ascii_lowercase().contains(query))
        || row
            .context
            .as_ref()
            .is_some_and(|ctx| ctx.to_ascii_lowercase().contains(query))
        || row.source.label().to_ascii_lowercase().contains(query)
}

fn upsert_override(profile: &mut KeymapProfile, binding: KeyBinding) {
    if let Some(existing) = profile
        .bindings
        .iter_mut()
        .find(|b| b.action == binding.action)
    {
        *existing = binding;
    } else {
        profile.bindings.push(binding);
    }
}

fn effective_base_profile(profile_id: &str) -> Result<KeymapProfile, String> {
    if profile_id == "custom" {
        return load_builtin_profile("default");
    }
    // Every built-in profile layers on top of the default map: a DAW profile
    // (Ableton, Cubase, …) only ships the accelerators it deliberately changes,
    // so without the default base those profiles would silently drop every
    // command they don't re-map (e.g. Undo, Save, the panel toggles). Load the
    // default first, then overlay the profile's own bindings so switching a
    // profile re-skins the shortcuts it defines and inherits the rest.
    if profile_id == "default" {
        return load_builtin_profile("default");
    }
    let mut merged = load_builtin_profile("default")?;
    let overlay = load_builtin_profile(profile_id)?;
    overlay_bindings(&mut merged, overlay.bindings);
    merged.name = overlay.name;
    merged.extends = Some("default".to_string());
    Ok(merged)
}

/// Overlay `overrides` onto `base` in place: an override for an action replaces
/// the base binding for that same action; actions the override does not mention
/// keep their base binding. Matches the per-action override semantics used by
/// user overrides in [`resolve_effective_bindings`].
fn overlay_bindings(base: &mut KeymapProfile, overrides: Vec<KeyBinding>) {
    for binding in overrides {
        upsert_override(base, binding);
    }
}

fn resolve_effective_bindings(
    profile_id: &str,
    user_overrides: &KeymapProfile,
    imported: Option<&KeymapProfile>,
) -> Vec<ResolvedKeyBinding> {
    let mut map: HashMap<String, ResolvedKeyBinding> = HashMap::new();

    let base = if profile_id == "custom" {
        if let Some(imported) = imported {
            imported.clone()
        } else {
            let extends = user_overrides.extends.as_deref().unwrap_or("default");
            let mut merged = load_builtin_profile(extends).unwrap_or_default();
            merged.bindings.extend(user_overrides.bindings.clone());
            merged
        }
    } else {
        effective_base_profile(profile_id).unwrap_or_default()
    };

    for binding in base.bindings {
        map.insert(
            binding.action.clone(),
            ResolvedKeyBinding {
                action: binding.action.clone(),
                keys: binding.keys.clone(),
                context: binding.context.clone(),
                args: binding.args.clone(),
                source: if profile_id == "custom" && imported.is_some() {
                    KeymapSource::Imported
                } else {
                    KeymapSource::Default
                },
                profile: profile_id.to_string(),
                is_user_override: false,
            },
        );
    }

    if profile_id != "custom" || imported.is_none() {
        for binding in &user_overrides.bindings {
            map.insert(
                binding.action.clone(),
                ResolvedKeyBinding {
                    action: binding.action.clone(),
                    keys: binding.keys.clone(),
                    context: binding.context.clone(),
                    args: binding.args.clone(),
                    source: KeymapSource::User,
                    profile: profile_id.to_string(),
                    is_user_override: true,
                },
            );
        }
    }

    let mut resolved: Vec<_> = map.into_values().collect();
    resolved.sort_by(|a, b| a.action.cmp(&b.action));
    resolved
}

fn build_rows(
    labels: &HashMap<String, String>,
    resolved: &[ResolvedKeyBinding],
    profile_id: &str,
) -> Vec<KeymapRow> {
    let mut actions: HashMap<String, ResolvedKeyBinding> = HashMap::new();
    for binding in resolved {
        actions.insert(binding.action.clone(), binding.clone());
    }
    for action in labels.keys() {
        actions
            .entry(action.clone())
            .or_insert_with(|| ResolvedKeyBinding {
                action: action.clone(),
                keys: Vec::new(),
                context: Some("Studio".to_string()),
                args: None,
                source: KeymapSource::Default,
                profile: profile_id.to_string(),
                is_user_override: false,
            });
    }

    let mut rows: Vec<KeymapRow> = actions
        .into_values()
        .map(|binding| {
            let arguments_json = binding
                .args
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok());
            KeymapRow {
                id: binding.action.clone(),
                action_label: labels
                    .get(&binding.action)
                    .cloned()
                    .unwrap_or_else(|| binding.action.clone()),
                action_id: binding.action.clone(),
                command: binding.action.clone(),
                arguments_json,
                keystrokes: binding.keys.clone(),
                context: binding.context.clone(),
                source: binding.source,
                profile: binding.profile.clone(),
                is_user_override: binding.is_user_override,
                is_conflict: false,
                conflict_with: Vec::new(),
                enabled: true,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.action_label.cmp(&b.action_label));
    rows
}

fn build_reverse_index(resolved: &[ResolvedKeyBinding]) -> HashMap<String, String> {
    let mut reverse: HashMap<String, String> = HashMap::new();
    let mut entries: Vec<&ResolvedKeyBinding> = resolved.iter().collect();
    entries.sort_by(|a, b| a.action.cmp(&b.action));
    for binding in entries {
        for key in &binding.keys {
            let Some(token) = canonical_accel(key) else {
                continue;
            };
            match reverse.get(&token) {
                Some(existing) if global_priority(existing) <= global_priority(&binding.action) => {
                }
                _ => {
                    reverse.insert(token, binding.action.clone());
                }
            }
        }
    }
    reverse
}

fn build_action_catalog() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for menu in &MenuManifest::load().menus {
        collect_menu_actions(&menu.items, &menu.label, &mut out);
    }
    // Actions that are bound to shortcuts but do not appear in the native menu
    // bar (context-menu-only, shortcut-only, or floating-window commands). Menu
    // labels are the source of truth; these fill the gaps so the Keymap panel
    // shows human-readable names instead of raw action IDs.
    let fallback: &[(&str, &str)] = &[
        ("app:force-reload",               "App › Force Reload"),
        ("audio:bounce-in-place",          "Audio › Bounce in Place"),
        ("audio:create-crossfade",         "Audio › Create Crossfade"),
        ("audio:render-selection",         "Audio › Render Selection"),
        ("clip:consolidate",               "Clip › Consolidate"),
        ("clip:properties",                "Clip › Properties"),
        ("clip:rename",                    "Clip › Rename"),
        ("clip:split-at-playhead",         "Clip › Split at Playhead"),
        ("edit:delete-backspace",          "Edit › Delete (Backspace)"),
        ("editor:open-bottom",             "Editor › Open in Bottom Panel"),
        ("extensions:manager",             "Tools › Extensions Manager"),
        ("file:export-audio",              "File › Export Audio"),
        ("file:export-stems",              "File › Export Stems"),
        ("file:import-audio",              "File › Import Audio"),
        ("floatingwindow:video-player",    "Window › Video Player"),
        ("jam:open",                       "Window › Jam Session"),
        ("midi:duplicate-selected",        "MIDI › Duplicate Selected"),
        ("midi:export-clip",               "MIDI › Export Clip"),
        ("midi:nudge-left",                "MIDI › Nudge Left"),
        ("midi:nudge-right",               "MIDI › Nudge Right"),
        ("midi:toggle-snap",               "MIDI › Toggle Snap"),
        ("midi:toggle-virtual-keyboard",   "MIDI › Toggle Virtual Keyboard"),
        ("midi:tool-draw",                 "MIDI › Draw Tool"),
        ("midi:tool-line",                 "MIDI › Line Tool"),
        ("midi:tool-select",               "MIDI › Select Tool"),
        ("midi:transpose-down",            "MIDI › Transpose Down"),
        ("midi:transpose-octave-down",     "MIDI › Transpose Down One Octave"),
        ("midi:transpose-octave-up",       "MIDI › Transpose Up One Octave"),
        ("midi:transpose-up",              "MIDI › Transpose Up"),
        ("midi:velocity-decrease",         "MIDI › Decrease Velocity"),
        ("midi:velocity-increase",         "MIDI › Increase Velocity"),
        ("mixer:create-bus",               "Mixer › Create Bus"),
        ("mixer:reset-pan",                "Mixer › Reset Pan"),
        ("mixer:reset-volume",             "Mixer › Reset Volume"),
        ("panel:toggle-automation",        "View › Automation Panel"),
        ("panel:toggle-device-panel",      "View › Device Panel"),
        ("panel:toggle-midi-editor",       "View › MIDI Editor"),
        ("project:new-from-template",      "Project › New from Template"),
        ("project:reveal-folder",          "Project › Reveal in Finder"),
        ("project:snapshot",               "Project › Snapshot"),
        ("timeline:toggle-snap",           "View › Toggle Snap"),
        ("tools:command-palette",          "Tools › Command Palette"),
        ("tools:developer-tools",          "Tools › Developer Tools"),
        ("tools:quick-search",             "Tools › Quick Search"),
        ("tools:select-mute",              "Tools › Select / Mute Tool"),
        ("track:add",                      "Project › Add Track"),
        ("track:arm",                      "Track › Arm for Recording"),
        ("track:mute",                     "Track › Mute"),
        ("track:solo",                     "Track › Solo"),
        ("window:minimize",                "Window › Minimize"),
        ("window:toggle-fullscreen",       "Window › Toggle Full Screen"),
    ];
    for (id, label) in fallback {
        out.entry(id.to_string()).or_insert_with(|| label.to_string());
    }
    out
}

fn collect_menu_actions(items: &[MenuItem], path: &str, out: &mut HashMap<String, String>) {
    for item in items {
        if let Some(command) = item.command.as_ref().filter(|cmd| !cmd.is_empty()) {
            if let Some(label) = item.label.as_ref() {
                out.insert(command.clone(), format!("{path} › {label}"));
            }
        }
        if !item.children.is_empty() {
            let child_path = if let Some(label) = &item.label {
                format!("{path} › {label}")
            } else {
                path.to_string()
            };
            collect_menu_actions(&item.children, &child_path, out);
        }
    }
}

pub fn format_keystroke_list(keys: &[String]) -> String {
    if keys.is_empty() {
        return "—".to_string();
    }
    keys.iter()
        .map(|key| {
            canonical_accel(key)
                .map(|token| format_accel_display(&token))
                .unwrap_or_else(|| key.clone())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn profile_label(profile_id: &str) -> &'static str {
    PROFILE_DESCRIPTORS
        .iter()
        .find(|p| p.id == profile_id)
        .map(|p| p.label)
        .unwrap_or("Default")
}

#[cfg(test)]
mod default_binding_tests {
    use super::*;

    /// The bare transport keys the studio relies on must resolve under the
    /// built-in default profile. Regression guard: a broken reverse index (or a
    /// default.json that drops one of these) silently kills the shortcut because
    /// only Space has a hard-coded fallback in the key handler.
    #[test]
    fn default_profile_binds_core_transport_keys() {
        let manager = KeymapManager::new(std::env::temp_dir());
        let reverse = manager.dispatch_reverse();
        assert_eq!(
            reverse.get("r").map(String::as_str),
            Some("transport:record"),
            "R must trigger record on the default profile"
        );
        assert_eq!(
            reverse.get("space").map(String::as_str),
            Some("transport:play-pause")
        );
        assert_eq!(
            reverse.get("s").map(String::as_str),
            Some("clip:split-at-playhead")
        );
    }

    /// Every id offered in the profile picker must resolve to a shipped keymap.
    /// A descriptor without a matching `builtin_profile_json` arm leaves the
    /// picker offering a profile that cannot be selected.
    #[test]
    fn every_builtin_descriptor_has_a_profile() {
        for descriptor in PROFILE_DESCRIPTORS.iter().filter(|p| p.builtin) {
            super::super::storage::load_builtin_profile(descriptor.id).unwrap_or_else(|error| {
                panic!("profile {} failed to load: {error}", descriptor.id)
            });
        }
    }

    /// The Pro Tools profile must carry real Pro Tools bindings rather than a
    /// copy of the defaults — its whole point is muscle memory.
    #[test]
    fn pro_tools_profile_uses_pro_tools_bindings() {
        let mut manager = KeymapManager::new(std::env::temp_dir());
        manager.set_active_profile("pro-tools").expect("pro-tools");
        let reverse = manager.dispatch_reverse();
        assert_eq!(
            reverse.get("f12").map(String::as_str),
            Some("transport:record"),
            "F12 must trigger record on the Pro Tools profile"
        );
        assert_eq!(
            reverse.get("ctrl+e").map(String::as_str),
            Some("clip:split-at-playhead"),
            "Ctrl+E is Separate Clip at Selection"
        );
        assert_eq!(
            reverse.get("f8").map(String::as_str),
            Some("tools:select-pointer"),
            "F8 is the Grabber"
        );
    }

    /// Non-default built-in profiles inherit the default map: a DAW profile only
    /// ships the accelerators it re-maps, so commands it does not mention must
    /// still resolve from the default base. Regression guard for the bug where
    /// selecting Ableton/Cubase/… silently dropped every un-remapped command.
    #[test]
    fn builtin_profiles_inherit_default_bindings() {
        // `edit:undo` (Ctrl+Z) and `project:save` (Ctrl+S) are default-only —
        // no DAW override touches them — so every profile must still bind them.
        for profile in [
            "ableton-live",
            "cubase",
            "fl-studio",
            "pro-tools",
            "futureboard",
        ] {
            let mut manager = KeymapManager::new(std::env::temp_dir());
            manager
                .set_active_profile(profile)
                .unwrap_or_else(|error| panic!("profile {profile}: {error}"));
            assert_eq!(
                manager.raw_accel_for_command("edit:undo").as_deref(),
                Some("Ctrl+Z"),
                "{profile} must inherit edit:undo from the default map"
            );
            assert!(
                manager.raw_accel_for_command("project:save").is_some(),
                "{profile} must inherit project:save from the default map"
            );
        }
    }

    /// A profile override replaces the default for the same command rather than
    /// stacking beside it: Ableton records on F9, not the default R.
    #[test]
    fn profile_override_replaces_default_binding() {
        let mut manager = KeymapManager::new(std::env::temp_dir());
        manager.set_active_profile("ableton-live").expect("ableton");
        assert_eq!(
            manager.raw_accel_for_command("transport:record").as_deref(),
            Some("F9"),
            "Ableton overrides Record to F9"
        );
        let reverse = manager.dispatch_reverse();
        assert_eq!(
            reverse.get("f9").map(String::as_str),
            Some("transport:record"),
            "F9 must resolve to Record under Ableton"
        );
    }

    /// The actionable commands that gained accelerators must resolve on the
    /// default profile. Regression guard for keymap coverage: these were `null`
    /// in the manifest and unreachable from the keyboard before.
    #[test]
    fn newly_bound_actionable_commands_resolve() {
        let manager = KeymapManager::new(std::env::temp_dir());
        for (command, accel) in [
            ("panel:toggle-bottom", "Ctrl+7"),
            ("track:delete", "Ctrl+Shift+Delete"),
            ("midi:open-editor", "Ctrl+Shift+M"),
            ("track:add-audio", "Ctrl+Shift+A"),
            ("project:save-copy", "Ctrl+Alt+Shift+S"),
            // Second-wave global actions wired from the dispatcher audit: track
            // state, clip actions, window openers, and export — all reachable
            // from the keyboard now, not just the right-click / menu path.
            ("track:mute", "Ctrl+Shift+H"),
            ("track:solo", "Ctrl+Shift+L"),
            ("track:arm", "Ctrl+Shift+B"),
            ("clip:rename", "F2"),
            ("clip:properties", "Ctrl+Shift+P"),
            ("plugins:scan", "Ctrl+Alt+U"),
            ("file:export-audio", "Ctrl+Shift+X"),
            ("window:big-clock", "Ctrl+Alt+K"),
            ("window:performance", "Ctrl+Alt+P"),
            ("midi:export-clip", "Ctrl+Alt+I"),
            // Third-wave audit: MIDI editor, mixer, solfege, and secondary
            // window commands that were dispatchable but had no keyboard path.
            ("midi:tool-select", "Ctrl+Alt+1"),
            ("midi:tool-draw", "Ctrl+Alt+2"),
            ("midi:velocity-increase", "Ctrl+Alt+Up"),
            ("midi:toggle-snap", "Ctrl+Alt+4"),
            ("midi:fit-notes", "Ctrl+Alt+5"),
            ("editor:open-bottom", "Ctrl+Alt+6"),
            ("mixer:create-bus", "Ctrl+Alt+7"),
            ("mixer:reset-volume", "Ctrl+Alt+8"),
            ("mixer:reset-pan", "Ctrl+Alt+9"),
            ("solfege:analyze-accent", "Ctrl+Alt+A"),
            ("automation:select-all-points", "Ctrl+Alt+Shift+P"),
            ("audio:stem-extractor", "Ctrl+Alt+Shift+E"),
            ("jam:open", "Ctrl+Alt+Shift+J"),
        ] {
            assert_eq!(
                manager.raw_accel_for_command(command).as_deref(),
                Some(accel),
                "{command} must bind to {accel} on the default profile"
            );
        }
    }
}
