use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::OnceLock;

use gpui::KeyDownEvent;

use crate::menu::{MenuItem, MenuManifest};

use super::conflicts::{annotate_row_conflicts, find_conflicts_for_binding};
use super::model::{
    KeyBinding, KeymapConflict, KeymapProfile, KeymapRow, KeymapScope, KeymapSource,
    PROFILE_DESCRIPTORS, ResolvedKeyBinding,
};
use super::normalize::{canonical_accel, format_accel_display};
use super::storage::{
    ensure_user_keymaps_dir, import_profile_file, load_builtin_profile, load_user_overrides,
    profile_json_text, save_profile_json, save_user_overrides, user_keymaps_dir,
    user_overrides_path,
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
    /// Key token → command, per scope. A lookup walks
    /// [`KeymapScope::chain`], so an editor's own key shadows nothing global
    /// (the conflict rules keep those apart) but does override the
    /// arrangement key an Automation binding layers on.
    reverse: HashMap<KeymapScope, HashMap<String, String>>,
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
        manager.canonicalize_overrides();
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

    /// Bind `keys` to `action` in `scope`. With conflicts and no `force`,
    /// nothing changes and the conflicts come back. With `force`, the key is
    /// taken off every conflicting binding first — "Replace" means the other
    /// action stops answering to it, not that two actions share it.
    pub fn tap_binding(
        &mut self,
        action: &str,
        keys: Vec<String>,
        scope: KeymapScope,
        args: Option<serde_json::Value>,
        force: bool,
    ) -> Result<Vec<KeymapConflict>, String> {
        let conflicts = find_conflicts_for_binding(action, &keys, scope, &self.resolved);
        if !conflicts.is_empty() && !force {
            return Ok(conflicts);
        }
        for conflict in &conflicts {
            let Some(token) = canonical_accel(&conflict.keystroke) else {
                continue;
            };
            let Some(other) = self
                .resolved
                .iter()
                .find(|b| b.action == conflict.action && b.scope == conflict.scope)
            else {
                continue;
            };
            let remaining: Vec<String> = other
                .keys
                .iter()
                .filter(|k| canonical_accel(k).as_deref() != Some(token.as_str()))
                .cloned()
                .collect();
            upsert_override(
                &mut self.user_overrides,
                scoped_binding(&conflict.action, remaining, conflict.scope, None),
            );
        }
        upsert_override(
            &mut self.user_overrides,
            scoped_binding(action, keys, scope, args),
        );
        self.dirty = true;
        self.rebuild();
        Ok(conflicts)
    }

    /// Drop the user's change to `action` in `scope`, back to the profile.
    pub fn reset_binding(&mut self, action: &str, scope: KeymapScope) {
        self.user_overrides
            .bindings
            .retain(|binding| !(binding.action == action && binding_scope(binding) == Some(scope)));
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
        self.canonicalize_overrides();
        self.dirty = false;
        self.rebuild();
    }

    pub fn load_json_text(&mut self, text: &str) -> Result<(), String> {
        let profile = super::storage::load_profile_json(text)?;
        self.user_overrides = profile;
        self.canonicalize_overrides();
        self.dirty = true;
        self.rebuild();
        Ok(())
    }

    pub fn json_text(&self) -> Result<String, String> {
        Ok(profile_json_text(&self.export_profile()))
    }

    /// The command `event` runs while `scope` has the keyboard.
    pub fn command_for_event(&self, event: &KeyDownEvent, scope: KeymapScope) -> Option<&str> {
        if event.is_held {
            return None;
        }
        let token = super::normalize::canonical_event(event)?;
        let command = self.command_for_token(&token, scope);
        if shortcut_debug_enabled() {
            eprintln!(
                "[shortcut] resolve profile={} scope={} token={} -> {:?}",
                self.active_profile_id,
                scope.key(),
                token,
                command
            );
        }
        command
    }

    /// The command a canonical key token (`"ctrl+shift+s"`) runs in `scope`.
    pub fn command_for_token(&self, token: &str, scope: KeymapScope) -> Option<&str> {
        scope.chain().iter().find_map(|scope| {
            self.reverse
                .get(scope)
                .and_then(|keys| keys.get(token))
                .map(String::as_str)
        })
    }

    /// Display string for the accelerator bound to `command` under the active
    /// profile, e.g. `"Ctrl+D"` — or `None` when the command has no binding.
    ///
    /// Used to backfill shortcut hints on menus and tooltips. The global
    /// binding wins, then the one in the scope the command acts on, then any.
    pub fn shortcut_for_command(&self, command: &str) -> Option<String> {
        let key = self.raw_accel_for_command(command)?;
        Some(format_keystroke_list(std::slice::from_ref(&key)))
    }

    /// The raw authored accelerator (e.g. `"Ctrl+Z"`) bound to `command` under
    /// the active profile, before any platform display formatting. Style-neutral,
    /// so tests and lookups can assert bindings without depending on the host OS
    /// or the `FUTUREBOARD_ACCEL_STYLE` env override.
    pub fn raw_accel_for_command(&self, command: &str) -> Option<String> {
        let first_key = |scope: Option<KeymapScope>| {
            self.resolved
                .iter()
                .filter(|b| b.action == command && scope.is_none_or(|s| b.scope == s))
                .find_map(|b| b.keys.iter().find(|key| !key.trim().is_empty()).cloned())
        };
        first_key(Some(KeymapScope::Global))
            .or_else(|| first_key(Some(KeymapScope::home_of(command))))
            .or_else(|| first_key(None))
    }

    /// The raw accelerator for `command` in exactly `scope`.
    pub fn raw_accel_in_scope(&self, command: &str, scope: KeymapScope) -> Option<String> {
        self.resolved
            .iter()
            .find(|b| b.action == command && b.scope == scope)
            .and_then(|b| b.keys.iter().find(|key| !key.trim().is_empty()).cloned())
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

    pub fn user_overrides_path(&self) -> PathBuf {
        user_overrides_path(&self.app_data)
    }

    /// Give every override a scope. Overrides saved before scopes existed
    /// name only an action; each is placed where the default profile binds
    /// that action, so an old "track:mute = Ctrl+Shift+H" moves the mute key
    /// in the arrangement and the mixer rather than landing nowhere.
    fn canonicalize_overrides(&mut self) {
        if self
            .user_overrides
            .bindings
            .iter()
            .all(|b| binding_scope(b).is_some())
        {
            return;
        }
        let base = effective_base_profile("default").unwrap_or_default();
        let mut placed: Vec<KeyBinding> = Vec::new();
        for binding in std::mem::take(&mut self.user_overrides.bindings) {
            if binding_scope(&binding).is_some() {
                placed.push(binding);
                continue;
            }
            for scope in scopes_for_unscoped(&binding.action, &base.bindings) {
                placed.push(scoped_binding(
                    &binding.action,
                    binding.keys.clone(),
                    scope,
                    binding.args.clone(),
                ));
            }
        }
        self.user_overrides.bindings = placed;
    }
}

fn row_matches_query(row: &KeymapRow, query: &str) -> bool {
    row.action_label.to_ascii_lowercase().contains(query)
        || row.action_id.to_ascii_lowercase().contains(query)
        || row
            .keystrokes
            .iter()
            .any(|key| key.to_ascii_lowercase().contains(query))
        || row.scope.label().to_ascii_lowercase().contains(query)
        || row.source.label().to_ascii_lowercase().contains(query)
}

fn binding_scope(binding: &KeyBinding) -> Option<KeymapScope> {
    binding.context.as_deref().and_then(KeymapScope::parse)
}

fn scoped_binding(
    action: &str,
    keys: Vec<String>,
    scope: KeymapScope,
    args: Option<serde_json::Value>,
) -> KeyBinding {
    KeyBinding {
        action: action.to_string(),
        keys,
        context: Some(scope.key().to_string()),
        args,
        when: None,
    }
}

/// Where a binding with no scope goes: every scope `base` binds the action
/// in, or the action's home scope when `base` has none.
fn scopes_for_unscoped(action: &str, base: &[KeyBinding]) -> Vec<KeymapScope> {
    let mut scopes: Vec<KeymapScope> = base
        .iter()
        .filter(|b| b.action == action)
        .filter_map(binding_scope)
        .collect();
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() {
        scopes.push(KeymapScope::home_of(action));
    }
    scopes
}

fn upsert_override(profile: &mut KeymapProfile, binding: KeyBinding) {
    let scope = binding_scope(&binding);
    if let Some(existing) = profile
        .bindings
        .iter_mut()
        .find(|b| b.action == binding.action && binding_scope(b) == scope)
    {
        *existing = binding;
    } else {
        profile.bindings.push(binding);
    }
}

fn effective_base_profile(profile_id: &str) -> Result<KeymapProfile, String> {
    if profile_id == "custom" || profile_id == "default" {
        return load_builtin_profile("default");
    }
    // Every built-in profile layers on top of the default map: a DAW profile
    // (Ableton, Cubase, …) only ships the accelerators it deliberately changes,
    // so without the default base those profiles would silently drop every
    // command they don't re-map (e.g. Undo, Save, the panel toggles). Load the
    // default first, then overlay the profile's own bindings so switching a
    // profile re-skins the shortcuts it defines and inherits the rest.
    let mut merged = load_builtin_profile("default")?;
    let overlay = load_builtin_profile(profile_id)?;
    overlay_bindings(&mut merged, overlay.bindings);
    merged.name = overlay.name;
    merged.extends = Some("default".to_string());
    Ok(merged)
}

/// Overlay `overrides` onto `base` in place. A scoped override replaces that
/// scope's binding for the action; an unscoped one (every v1 DAW profile)
/// replaces the action in each scope the base binds it in.
fn overlay_bindings(base: &mut KeymapProfile, overrides: Vec<KeyBinding>) {
    for binding in overrides {
        if binding_scope(&binding).is_some() {
            upsert_override(base, binding);
            continue;
        }
        for scope in scopes_for_unscoped(&binding.action, &base.bindings) {
            upsert_override(
                base,
                scoped_binding(
                    &binding.action,
                    binding.keys.clone(),
                    scope,
                    binding.args.clone(),
                ),
            );
        }
    }
}

fn resolve_effective_bindings(
    profile_id: &str,
    user_overrides: &KeymapProfile,
    imported: Option<&KeymapProfile>,
) -> Vec<ResolvedKeyBinding> {
    let imported_base = profile_id == "custom" && imported.is_some();
    let mut base = if imported_base {
        let mut merged = load_builtin_profile("default").unwrap_or_default();
        merged.bindings.clear();
        overlay_bindings(&mut merged, imported.cloned().unwrap_or_default().bindings);
        merged
    } else {
        effective_base_profile(profile_id).unwrap_or_default()
    };
    let base_source = if imported_base {
        KeymapSource::Imported
    } else {
        KeymapSource::Default
    };

    let mut map: BTreeMap<(KeymapScope, String), ResolvedKeyBinding> = BTreeMap::new();
    let mut insert = |binding: KeyBinding, source: KeymapSource, is_user_override: bool| {
        let scope =
            binding_scope(&binding).unwrap_or_else(|| KeymapScope::home_of(&binding.action));
        map.insert(
            (scope, binding.action.clone()),
            ResolvedKeyBinding {
                action: binding.action,
                keys: binding.keys,
                scope,
                args: binding.args,
                source,
                profile: profile_id.to_string(),
                is_user_override,
            },
        );
    };
    for binding in std::mem::take(&mut base.bindings) {
        insert(binding, base_source, false);
    }
    if !imported_base {
        for binding in user_overrides.bindings.iter().cloned() {
            insert(binding, KeymapSource::User, true);
        }
    }
    map.into_values().collect()
}

fn build_rows(
    labels: &HashMap<String, String>,
    resolved: &[ResolvedKeyBinding],
    profile_id: &str,
) -> Vec<KeymapRow> {
    let mut bindings: Vec<ResolvedKeyBinding> = resolved.to_vec();
    // An action no scope binds still gets a row — in the scope it acts on —
    // so it can be given a key.
    for action in labels.keys() {
        if !resolved.iter().any(|b| &b.action == action) {
            bindings.push(ResolvedKeyBinding {
                action: action.clone(),
                keys: Vec::new(),
                scope: KeymapScope::home_of(action),
                args: None,
                source: KeymapSource::Default,
                profile: profile_id.to_string(),
                is_user_override: false,
            });
        }
    }

    let mut rows: Vec<KeymapRow> = bindings
        .into_iter()
        .map(|binding| {
            let arguments_json = binding
                .args
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok());
            KeymapRow {
                id: format!("{}/{}", binding.scope.key(), binding.action),
                action_label: labels
                    .get(&binding.action)
                    .cloned()
                    .unwrap_or_else(|| binding.action.clone()),
                action_id: binding.action.clone(),
                command: binding.action.clone(),
                arguments_json,
                keystrokes: binding.keys.clone(),
                scope: binding.scope,
                source: binding.source,
                profile: binding.profile.clone(),
                is_user_override: binding.is_user_override,
                is_conflict: false,
                conflict_with: Vec::new(),
                enabled: true,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.scope
            .cmp(&b.scope)
            .then_with(|| a.action_label.cmp(&b.action_label))
    });
    rows
}

fn build_reverse_index(
    resolved: &[ResolvedKeyBinding],
) -> HashMap<KeymapScope, HashMap<String, String>> {
    let mut reverse: HashMap<KeymapScope, HashMap<String, String>> = HashMap::new();
    // `resolved` is sorted by (scope, action), so within a scope the first
    // action wins a shared key. Such a pair is a conflict the editor shows;
    // this only keeps the choice stable until it is fixed.
    for binding in resolved {
        let keys = reverse.entry(binding.scope).or_default();
        for key in &binding.keys {
            let Some(token) = canonical_accel(key) else {
                continue;
            };
            keys.entry(token).or_insert_with(|| binding.action.clone());
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
    #[rustfmt::skip]
    let fallback: &[(&str, &str)] = &[
        ("app:force-reload",               "App › Force Reload"),
        ("audio:bounce-in-place",          "Audio › Bounce in Place"),
        ("audio:create-crossfade",         "Audio › Create Crossfade"),
        ("audio:find-tempo-key",           "Audio › Find Tempo & Key"),
        ("chords:open-generator",          "Chords › Chord Generator"),
        ("chords:open-track",              "Chords › Show Chord Track"),
        ("chords:hide-track",              "Chords › Hide Chord Track"),
        ("chords:toggle-track",            "View › Chord Track"),
        ("chords:to-midi",                 "Chords › Create MIDI Clip from Chords"),
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
        ("midi:duplicate-selected",        "MIDI › Duplicate Selected"),
        ("midi:export-clip",               "MIDI › Export Clip"),
        ("midi:nudge-left",                "MIDI › Nudge Left"),
        ("midi:nudge-right",               "MIDI › Nudge Right"),
        ("midi:toggle-snap",               "MIDI › Toggle Snap"),
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
        ("panel:toggle-midi-editor",       "View › Editor Panel"),
        ("project:new-from-template",      "Project › New from Template"),
        ("project:reveal-folder",          "Project › Reveal in Finder"),
        ("song_text.add_chord_at_playhead", "Song Text › Add Chord at Playhead"),
        ("song_text.add_lyric_at_playhead", "Song Text › Add Lyric at Playhead"),
        ("song_text.add_both_at_playhead", "Song Text › Add Chord and Lyric at Playhead"),
        ("song_text.commit",               "Song Text › Commit"),
        ("song_text.commit_next_grid",     "Song Text › Commit and Move to Next Grid"),
        ("song_text.commit_next_beat",     "Song Text › Commit and Move to Next Beat"),
        ("song_text.commit_next_bar",      "Song Text › Commit and Move to Next Bar"),
        ("song_text.previous_event",       "Song Text › Previous Event"),
        ("song_text.next_event",           "Song Text › Next Event"),
        ("song_text.move_to_playhead",     "Song Text › Move to Playhead"),
        ("song_text.delete_selected",      "Song Text › Delete Selected"),
        ("timeline:toggle-snap",           "View › Toggle Snap"),
        ("view:waveform-zoom-in",          "View › Waveform Zoom In"),
        ("view:waveform-zoom-out",         "View › Waveform Zoom Out"),
        ("view:waveform-zoom-reset",       "View › Waveform Zoom 1×"),
        ("tools:command-palette",          "Tools › Command Palette"),
        ("tools:select-mute",              "Tools › Select / Mute Tool"),
        ("track:add",                      "Project › Add Track"),
        ("track:arm",                      "Track › Arm for Recording"),
        ("track:mute",                     "Track › Mute"),
        ("track:solo",                     "Track › Solo"),
        ("window:minimize",                "Window › Minimize"),
        ("window:toggle-fullscreen",       "Window › Toggle Full Screen"),
    ];
    for (id, label) in fallback {
        out.entry(id.to_string())
            .or_insert_with(|| label.to_string());
    }
    // Commands with no handler yet are not offered for binding: a key that
    // does nothing is a control that lies.
    for unhandled in UNHANDLED_COMMANDS {
        out.remove(*unhandled);
    }
    out
}

/// Commands that appear in menus or older profiles but that nothing in the
/// studio runs yet. They get no row and no key until they do.
pub(crate) const UNHANDLED_COMMANDS: &[&str] = &[
    "app:force-reload",
    "audio:bounce-in-place",
    "audio:render-selection",
    "clip:consolidate",
    "file:import-audio",
    "panel:toggle-automation",
    "panel:toggle-device-panel",
    "project:snapshot",
    "tools:developer-tools",
    "tools:quick-search",
];

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
    use KeymapScope::*;

    fn fresh() -> KeymapManager {
        // An empty data folder: no user overrides leak in from a real one.
        let dir = std::env::temp_dir().join(format!(
            "fb-keymap-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        KeymapManager::new(dir)
    }

    fn resolve(manager: &KeymapManager, scope: KeymapScope, accel: &str) -> Option<String> {
        manager
            .command_for_token(&canonical_accel(accel).unwrap(), scope)
            .map(str::to_string)
    }

    /// The bare transport keys the studio relies on resolve everywhere.
    #[test]
    fn transport_keys_work_in_every_scope() {
        let manager = fresh();
        for scope in KeymapScope::ALL {
            assert_eq!(
                resolve(&manager, scope, "Space").as_deref(),
                Some("transport:play-pause")
            );
            assert_eq!(
                resolve(&manager, scope, "R").as_deref(),
                Some("transport:record")
            );
            assert_eq!(
                resolve(&manager, scope, "K").as_deref(),
                Some("transport:toggle-metronome")
            );
        }
    }

    /// One key, a different job per editor — the reason for scopes.
    #[test]
    fn the_same_key_means_each_editors_own_command() {
        let manager = fresh();
        for (accel, scope, command) in [
            ("G", Arrangement, "timeline:toggle-snap"),
            ("G", MidiEditor, "midi:toggle-snap"),
            ("Ctrl+A", Arrangement, "edit:select-all"),
            ("Ctrl+A", MidiEditor, "midi:select-all"),
            ("Ctrl+A", AudioEditor, "edit:select-all"),
            ("Ctrl+A", AutomationEditor, "automation:select-all-points"),
            ("Delete", MidiEditor, "midi:delete-selected"),
            (
                "Delete",
                AutomationEditor,
                "automation:delete-selected-points",
            ),
            ("Delete", SongTextEditor, "song_text.delete_selected"),
            ("M", Arrangement, "track:mute"),
            ("S", Arrangement, "clip:split-at-playhead"),
            ("S", Mixer, "track:solo"),
            ("1", Arrangement, "tools:select-pointer"),
            ("1", MidiEditor, "midi:tool-select"),
            ("Enter", SongTextEditor, "song_text.commit"),
            ("Alt+Enter", Arrangement, "clip:properties"),
            ("Alt+Enter", MidiEditor, "solfege:apply-accent"),
        ] {
            assert_eq!(
                resolve(&manager, scope, accel).as_deref(),
                Some(command),
                "{accel} in {scope:?}"
            );
        }
        // Keys stay in their own editor.
        assert_eq!(resolve(&manager, MidiEditor, "S"), None);
        assert_eq!(resolve(&manager, Mixer, "G"), None);
    }

    /// Automation is a layer on the arrangement: its keys win, the rest of
    /// the arrangement's keys still work.
    #[test]
    fn automation_layers_on_the_arrangement() {
        let manager = fresh();
        assert_eq!(
            resolve(&manager, AutomationEditor, "A").as_deref(),
            Some("automation:toggle-mode")
        );
        assert_eq!(
            resolve(&manager, AutomationEditor, "1").as_deref(),
            Some("tools:select-pointer")
        );
        assert_eq!(
            resolve(&manager, Arrangement, "A").as_deref(),
            Some("track:arm")
        );
    }

    /// The shipped default has no conflicts at all: no key is used twice in
    /// one scope, and no editor reuses a global key.
    #[test]
    fn the_default_profile_has_no_conflicts() {
        let manager = fresh();
        let conflicts: Vec<String> = manager
            .rows()
            .iter()
            .filter(|row| row.is_conflict)
            .map(|row| format!("{} {:?}: {:?}", row.id, row.keystrokes, row.conflict_with))
            .collect();
        assert!(conflicts.is_empty(), "{conflicts:#?}");
    }

    /// Nothing is bound to a command no handler runs.
    #[test]
    fn no_default_key_is_bound_to_a_missing_command() {
        let manager = fresh();
        for command in UNHANDLED_COMMANDS {
            assert_eq!(manager.raw_accel_for_command(command), None, "{command}");
        }
    }

    /// Every id offered in the profile picker must resolve to a shipped keymap.
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
        let mut manager = fresh();
        manager.set_active_profile("pro-tools").expect("pro-tools");
        assert_eq!(
            resolve(&manager, Arrangement, "F12").as_deref(),
            Some("transport:record"),
            "F12 must trigger record on the Pro Tools profile"
        );
        assert_eq!(
            resolve(&manager, Arrangement, "Ctrl+E").as_deref(),
            Some("clip:split-at-playhead"),
            "Ctrl+E is Separate Clip at Selection"
        );
        assert_eq!(
            resolve(&manager, Arrangement, "F8").as_deref(),
            Some("tools:select-pointer"),
            "F8 is the Grabber"
        );
    }

    /// Non-default built-in profiles inherit the default map: a DAW profile only
    /// ships the accelerators it re-maps, so commands it does not mention must
    /// still resolve from the default base.
    #[test]
    fn builtin_profiles_inherit_default_bindings() {
        for profile in [
            "ableton-live",
            "cubase",
            "fl-studio",
            "pro-tools",
            "futureboard",
        ] {
            let mut manager = fresh();
            manager
                .set_active_profile(profile)
                .unwrap_or_else(|error| panic!("profile {profile}: {error}"));
            assert!(
                manager.raw_accel_for_command("edit:undo").is_some(),
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
        let mut manager = fresh();
        manager.set_active_profile("ableton-live").expect("ableton");
        assert_eq!(
            manager.raw_accel_for_command("transport:record").as_deref(),
            Some("F9"),
            "Ableton overrides Record to F9"
        );
        assert_eq!(
            resolve(&manager, Mixer, "F9").as_deref(),
            Some("transport:record")
        );
    }

    /// Replacing a conflict takes the key off the other action, in that
    /// scope only, and survives a save and reload.
    #[test]
    fn replacing_a_conflict_moves_the_key() {
        let mut manager = fresh();
        let conflicts = manager
            .tap_binding("midi:quantize", vec!["G".into()], MidiEditor, None, false)
            .unwrap();
        assert_eq!(conflicts.len(), 1, "G is MIDI snap");
        assert_eq!(
            resolve(&manager, MidiEditor, "G").as_deref(),
            Some("midi:toggle-snap")
        );

        manager
            .tap_binding("midi:quantize", vec!["G".into()], MidiEditor, None, true)
            .unwrap();
        assert_eq!(
            resolve(&manager, MidiEditor, "G").as_deref(),
            Some("midi:quantize")
        );
        assert_eq!(
            manager.raw_accel_in_scope("midi:toggle-snap", MidiEditor),
            None
        );
        // The arrangement's G is untouched.
        assert_eq!(
            resolve(&manager, Arrangement, "G").as_deref(),
            Some("timeline:toggle-snap")
        );
        assert!(!manager.rows().iter().any(|row| row.is_conflict));

        manager.save_changes().unwrap();
        let reloaded = KeymapManager::new(manager.app_data.clone());
        assert_eq!(
            resolve(&reloaded, MidiEditor, "G").as_deref(),
            Some("midi:quantize")
        );
    }

    /// A key bound in an editor may not take a global key.
    #[test]
    fn an_editor_key_may_not_take_a_global_one() {
        let mut manager = fresh();
        let conflicts = manager
            .tap_binding(
                "midi:quantize",
                vec!["Space".into()],
                MidiEditor,
                None,
                false,
            )
            .unwrap();
        assert_eq!(conflicts[0].action, "transport:play-pause");
        assert_eq!(conflicts[0].scope, Global);
    }

    /// Overrides saved before scopes existed still apply, in every scope the
    /// default binds the action.
    #[test]
    fn an_unscoped_override_lands_where_the_action_lives() {
        let dir = std::env::temp_dir().join(format!("fb-keymap-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Keymaps")).unwrap();
        std::fs::write(
            dir.join("Keymaps")
                .join(super::super::model::USER_OVERRIDES_FILE),
            r#"{"name":"User","extends":"default","bindings":[
                {"action":"track:mute","keys":["Ctrl+Shift+H"],"context":"Studio"}]}"#,
        )
        .unwrap();
        let manager = KeymapManager::new(dir);
        assert_eq!(
            resolve(&manager, Arrangement, "Ctrl+Shift+H").as_deref(),
            Some("track:mute")
        );
        assert_eq!(
            resolve(&manager, Mixer, "Ctrl+Shift+H").as_deref(),
            Some("track:mute")
        );
        assert_eq!(resolve(&manager, Arrangement, "M"), None);
    }
}
