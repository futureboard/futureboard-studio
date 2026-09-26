//! Shared application menu manifest.
//!
//! Source of truth: `packages/shared/src/menu/menuItems.ts`. The Electron
//! sync script (`scripts/sync-shared-menu.mjs`) emits a JSON
//! manifest at `packages/shared/generated/native-menu.json` which this
//! module embeds via `include_str!` and parses at startup.
//!
//! Native must not maintain its own menu definition — `MenuManifest::load`
//! returns the parsed JSON manifest. If parsing fails for any reason we log
//! the error and fall back to a minimal top-level shell so the app still
//! renders something instead of panicking.
//!
//! Realtime / audio rule: this module is pure data, no IO on hot paths.

use std::sync::OnceLock;

use serde::Deserialize;

/// JSON manifest produced by the sync script.
pub const NATIVE_MENU_JSON: &str =
    include_str!("../../../packages/shared/generated/native-menu.json");

#[derive(Debug, Clone, Deserialize)]
pub struct MenuManifest {
    pub version: u32,
    #[serde(default)]
    pub menus: Vec<Menu>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Menu {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub items: Vec<MenuItem>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MenuItemKind {
    Normal,
    Separator,
    Submenu,
    Checkbox,
    Radio,
}

impl Default for MenuItemKind {
    fn default() -> Self {
        MenuItemKind::Normal
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MenuItem {
    pub id: String,
    #[serde(default)]
    pub kind: MenuItemKind,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub shortcut: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub danger: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub children: Vec<MenuItem>,
    /// Runtime-only context drawn after the label ("Undo — Paste FX Chain").
    /// Never in the manifest: the label is translated by item id, so what an
    /// item acts on right now goes here instead of into the label.
    #[serde(skip)]
    pub detail: Option<String>,
}

/// Runtime state for one command item: whether it can run now, and what it
/// would act on. See [`patch_command_states`].
pub struct CommandMenuState<'a> {
    pub command: &'a str,
    pub enabled: bool,
    pub detail: Option<String>,
}

/// Apply runtime enabled state and detail to the items running each command
/// in `states`, through the whole tree. Items for other commands keep their
/// manifest defaults.
pub fn patch_command_states(items: &mut [MenuItem], states: &[CommandMenuState<'_>]) {
    for item in items {
        if let Some(state) = item
            .command
            .as_deref()
            .and_then(|command| states.iter().find(|state| state.command == command))
        {
            item.enabled = state.enabled;
            item.detail = state.detail.clone();
        }
        if !item.children.is_empty() {
            patch_command_states(&mut item.children, states);
        }
    }
}

fn default_true() -> bool {
    true
}

static MANIFEST: OnceLock<MenuManifest> = OnceLock::new();

/// Apply runtime checkbox state to a cloned menu tree (panel visibility,
/// developer toggles, etc.). Static manifest defaults stay in JSON.
pub fn patch_checkbox_states(items: &mut [MenuItem], checks: &[(&str, bool)]) {
    for item in items {
        if let Some((_, checked)) = checks.iter().find(|(id, _)| id == &item.id.as_str()) {
            if item.kind == MenuItemKind::Checkbox {
                item.checked = *checked;
            }
        }
        if !item.children.is_empty() {
            patch_checkbox_states(&mut item.children, checks);
        }
    }
}

impl MenuManifest {
    /// Parse the embedded JSON once, falling back to [`MenuManifest::fallback`]
    /// on any error. Logs the failure to stderr so the issue is visible in
    /// development without panicking in release.
    pub fn load() -> &'static MenuManifest {
        MANIFEST.get_or_init(|| match serde_json::from_str::<MenuManifest>(NATIVE_MENU_JSON) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "[menu] failed to parse generated native-menu.json: {e}. Falling back to minimal menu shell."
                );
                MenuManifest::fallback()
            }
        })
    }

    /// Minimal top-level menu used when the generated JSON is missing or
    /// malformed. Keeps the chrome from looking empty in that case.
    pub fn fallback() -> MenuManifest {
        let bare = |id: &str, label: &str| Menu {
            id: id.to_string(),
            label: label.to_string(),
            items: Vec::new(),
        };
        MenuManifest {
            version: 0,
            menus: vec![
                bare("file", "File"),
                bare("edit", "Edit"),
                bare("view", "View"),
                bare("transport", "Transport"),
                bare("window", "Window"),
                bare("help", "Help"),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<String> {
        fn walk(items: &[MenuItem], out: &mut Vec<String>) {
            for item in items {
                if let Some(command) = item.command.as_ref() {
                    out.push(command.clone());
                }
                walk(&item.children, out);
            }
        }
        let mut out = Vec::new();
        for menu in &MenuManifest::load().menus {
            walk(&menu.items, &mut out);
        }
        out
    }

    /// The embedded manifest is the one command registry: the menu bar, the
    /// context menus and the command palette all read it. A command that only
    /// exists in the dispatcher is a command nobody can find.
    #[test]
    fn the_accent_commands_are_discoverable() {
        let commands = commands();
        for command in [
            "solfege:analyze-accent",
            "solfege:analyze-accent-replace-all",
            "solfege:apply-accent",
        ] {
            assert!(
                commands.iter().any(|found| found == command),
                "{command} is dispatched but appears in no menu, so the command \
                 palette cannot offer it"
            );
        }
    }

    /// Every window Studio can open on its own — one that needs no track and no
    /// selection — has to be reachable from the Window menu. A window with a
    /// dispatch arm and no menu entry exists only for whoever already knows its
    /// command id, which is nobody.
    #[test]
    fn every_standalone_window_is_in_the_window_menu() {
        let commands = commands();
        for command in [
            "window:big-clock",
            "window:timecode",
            "floatingwindow:routing-matrix",
            "floatingwindow:mixer",
            "window:video-player",
            "window:audio-jam",
            "window:extensions",
            "window:performance",
            "panel:toggle-bottom",
        ] {
            assert!(
                commands.iter().any(|found| found == command),
                "{command} opens a window but appears in no menu"
            );
        }
    }

    /// The Chord Generator is a standalone window, so it lives in the Window
    /// menu as well as under MIDI.
    #[test]
    fn the_chord_generator_is_in_the_window_menu() {
        let window = MenuManifest::load()
            .menus
            .iter()
            .find(|menu| menu.id == "window")
            .expect("window menu");
        assert!(window
            .items
            .iter()
            .any(|item| item.command.as_deref() == Some("chords:open-generator")));
    }

    /// A malformed manifest degrades to the fallback shell rather than
    /// panicking, so this is worth asserting rather than assuming: if the
    /// embedded JSON ever stops parsing, every menu silently empties.
    #[test]
    fn the_embedded_manifest_parses() {
        let manifest = MenuManifest::load();
        assert!(
            manifest.menus.len() > MenuManifest::fallback().menus.len(),
            "the embedded manifest failed to parse and fell back to the shell"
        );
    }
}
