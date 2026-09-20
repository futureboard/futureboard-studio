//! macOS native menubar — maps [`MenuManifest`] to GPUI `cx.set_menus`.
//!
//! Command dispatch uses shared manifest command IDs so keyboard shortcuts and
//! GPUI dropdowns stay aligned with the system menu.

use std::sync::{Arc, Mutex, OnceLock};

use gpui::App;

use crate::menu::{MenuItem as AppMenuItem, MenuItemKind};
use crate::platform_chrome::APP_WINDOW_TITLE;

/// Commands that macOS keeps in the application menu instead of File/Edit/Help.
const APPLICATION_MENU_COMMANDS: &[&str] = &[
    "app:about",
    "app:check-for-updates",
    "app:preferences",
    "app:quit",
];

/// One row of the macOS application menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationMenuEntry {
    /// Runs a manifest command id through the shared dispatcher.
    Command {
        label: String,
        command: &'static str,
    },
    Separator,
    /// Submenu populated by the system.
    Services {
        label: &'static str,
    },
}

/// Rows of the macOS application menu, in AppKit order.
///
/// AppKit renders the first menu of the main menu as the application menu: it
/// replaces the title with the process name and owns the About / Settings /
/// Quit slots. Without this leading menu the manifest's first menu — File —
/// becomes the application menu, so its items are reachable only under the app
/// name and every later menu shifts one place to the left.
pub fn application_menu_entries() -> Vec<ApplicationMenuEntry> {
    vec![
        ApplicationMenuEntry::Command {
            label: format!("About {APP_WINDOW_TITLE}"),
            command: "app:about",
        },
        ApplicationMenuEntry::Command {
            label: "Check for Updates...".to_string(),
            command: "app:check-for-updates",
        },
        ApplicationMenuEntry::Separator,
        ApplicationMenuEntry::Command {
            label: "Settings...".to_string(),
            command: "app:preferences",
        },
        ApplicationMenuEntry::Separator,
        ApplicationMenuEntry::Services { label: "Services" },
        ApplicationMenuEntry::Separator,
        ApplicationMenuEntry::Command {
            label: format!("Quit {APP_WINDOW_TITLE}"),
            command: "app:quit",
        },
    ]
}

/// Manifest items with the application-menu commands removed and the
/// separators they orphaned collapsed.
pub fn items_without_application_menu_commands(items: &[AppMenuItem]) -> Vec<AppMenuItem> {
    let kept: Vec<AppMenuItem> = items
        .iter()
        .filter(|item| {
            !item
                .command
                .as_deref()
                .is_some_and(|command| APPLICATION_MENU_COMMANDS.contains(&command))
        })
        .map(|item| {
            let mut item = item.clone();
            if !item.children.is_empty() {
                item.children = items_without_application_menu_commands(&item.children);
            }
            item
        })
        .collect();

    collapse_separators(kept)
}

fn collapse_separators(items: Vec<AppMenuItem>) -> Vec<AppMenuItem> {
    let is_separator = |item: &AppMenuItem| item.kind == MenuItemKind::Separator;
    let mut collapsed: Vec<AppMenuItem> = Vec::with_capacity(items.len());
    for item in items {
        if is_separator(&item) && collapsed.last().is_none_or(&is_separator) {
            continue;
        }
        collapsed.push(item);
    }
    while collapsed.last().is_some_and(&is_separator) {
        collapsed.pop();
    }
    collapsed
}

static COMMAND_DISPATCHER: OnceLock<Mutex<Option<Arc<dyn Fn(&str, &mut App) + Send + Sync>>>> =
    OnceLock::new();

fn dispatcher_slot() -> &'static Mutex<Option<Arc<dyn Fn(&str, &mut App) + Send + Sync>>> {
    COMMAND_DISPATCHER.get_or_init(|| Mutex::new(None))
}

/// Register the handler that runs menu command IDs (typically `StudioLayout`).
pub fn set_command_dispatcher(dispatcher: Arc<dyn Fn(&str, &mut App) + Send + Sync>) {
    *dispatcher_slot().lock().expect("menu dispatcher lock") = Some(dispatcher);
}

/// Install the application menu from the shared manifest. No-op off macOS.
pub fn install_native_macos_menu(cx: &mut App) {
    #[cfg(target_os = "macos")]
    {
        if !crate::platform_chrome::PlatformChromePolicy::current().use_native_macos_menubar {
            return;
        }
        install_native_macos_menu_inner(cx);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

#[cfg(target_os = "macos")]
mod macos {
    use gpui::{App, KeyBinding, Menu, MenuItem as GpuiMenuItem, SharedString, SystemMenuType};

    use super::{ApplicationMenuEntry, APP_WINDOW_TITLE};
    use crate::menu::{MenuItem as AppMenuItem, MenuItemKind, MenuManifest};

    #[derive(Clone, PartialEq, gpui::Action)]
    #[action(no_json)]
    pub(super) struct RunMenuCommand {
        pub command_id: SharedString,
    }

    /// Commands whose accelerator is focus-gated in the studio key handler: the
    /// timeline and the docked MIDI/Solfege editors share these, and the
    /// keyboard path routes them to whichever holds focus. An AppKit key
    /// equivalent fires the menu action *before* that handler runs, so the piano
    /// roll would lose Cmd+C / Cmd+A / etc. to the timeline. These stay in the
    /// menu; they just carry no printed accelerator. Keep in sync with
    /// `layout::helpers::is_midi_routable_edit_command`.
    const FOCUS_GATED_COMMANDS: &[&str] = &[
        "edit:select-all",
        "edit:copy",
        "edit:cut",
        "edit:paste",
        "edit:duplicate",
        "edit:delete",
        "edit:delete-backspace",
        "clip:delete",
        "clip:duplicate",
    ];

    /// Translate a shared-manifest accelerator (authored Windows-first, e.g.
    /// `"Ctrl+Shift+S"`, `"Alt+F4"`) into a GPUI keystroke string for the macOS
    /// menubar, where the primary modifier is Cmd. Returns `None` for anything
    /// that must not become an AppKit key equivalent — the binding is skipped so
    /// the menu still lists the command without stealing the key:
    ///   * bare keys and Shift-only chords (would swallow a plain keypress),
    ///   * focus-gated edit commands (see `FOCUS_GATED_COMMANDS`),
    ///   * keys we can't map.
    /// `app:quit` ships as `Alt+F4` for Windows; macOS quit is Cmd+Q.
    pub(super) fn manifest_accel_to_mac_keystroke(command: &str, accel: &str) -> Option<String> {
        if FOCUS_GATED_COMMANDS.contains(&command) {
            return None;
        }
        if command == "app:quit" {
            return Some("cmd-q".to_string());
        }
        let mut cmd = false;
        let mut alt = false;
        let mut shift = false;
        let mut key: Option<String> = None;
        for raw in accel.split('+') {
            match raw.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "ctrl" | "control" | "cmd" | "command" | "meta" | "super" => cmd = true,
                "alt" | "option" | "opt" => alt = true,
                "shift" => shift = true,
                other => key = Some(map_key(other)?),
            }
        }
        let key = key?;
        // Require a Cmd/Alt anchor: a bare or Shift-only key equivalent would let
        // the menubar intercept an ordinary keystroke (e.g. Space, R, Shift+B).
        if !cmd && !alt {
            return None;
        }
        let mut out = String::new();
        if cmd {
            out.push_str("cmd-");
        }
        if alt {
            out.push_str("alt-");
        }
        if shift {
            out.push_str("shift-");
        }
        out.push_str(&key);
        Some(out)
    }

    /// Map a manifest key token to the GPUI keystroke key spelling.
    fn map_key(token: &str) -> Option<String> {
        let mapped = match token {
            "esc" | "escape" => "escape",
            "del" | "delete" => "delete",
            "backspace" => "backspace",
            "enter" | "return" => "enter",
            "tab" => "tab",
            "space" => "space",
            "home" => "home",
            "end" => "end",
            "pageup" | "page_up" | "pgup" => "pageup",
            "pagedown" | "page_down" | "pgdn" => "pagedown",
            "left" | "arrowleft" | "arrow_left" => "left",
            "right" | "arrowright" | "arrow_right" => "right",
            "up" | "arrowup" | "arrow_up" => "up",
            "down" | "arrowdown" | "arrow_down" => "down",
            "plus" | "=" => "=",
            "minus" | "-" => "-",
            "," | "." | "/" | ";" | "'" | "[" | "]" | "\\" | "`" => token,
            f if f.len() > 1
                && f.starts_with('f')
                && f[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                return Some(f.to_string());
            }
            other if other.chars().count() == 1 => other,
            _ => return None,
        };
        Some(mapped.to_string())
    }

    /// Walk the manifest and collect `(command, keystroke)` pairs for every
    /// item whose accelerator survives [`manifest_accel_to_mac_keystroke`].
    fn collect_menu_keystrokes(items: &[AppMenuItem], out: &mut Vec<(String, String)>) {
        for item in items {
            if let (Some(command), Some(accel)) =
                (item.command.as_deref(), item.shortcut.as_deref())
            {
                if !command.is_empty() && !accel.is_empty() {
                    if let Some(keystroke) = manifest_accel_to_mac_keystroke(command, accel) {
                        out.push((command.to_string(), keystroke));
                    }
                }
            }
            collect_menu_keystrokes(&item.children, out);
        }
    }

    /// GPUI key bindings that back the macOS menubar's key equivalents. Without
    /// these, `create_menu_item` finds no binding for a `RunMenuCommand` and the
    /// dropdown shows no accelerator. Reflects the default profile's accelerators
    /// (the shared manifest); a non-default keymap profile still dispatches via
    /// the studio's own handler, the printed equivalent just tracks the default.
    fn menu_key_bindings() -> Vec<KeyBinding> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for menu in &MenuManifest::load().menus {
            collect_menu_keystrokes(&menu.items, &mut pairs);
        }
        pairs
            .into_iter()
            .map(|(command, keystroke)| {
                KeyBinding::new(
                    &keystroke,
                    RunMenuCommand {
                        command_id: command.into(),
                    },
                    None,
                )
            })
            .collect()
    }

    pub(super) fn install(cx: &mut App) {
        cx.on_action(|action: &RunMenuCommand, cx: &mut App| {
            let command_id = action.command_id.to_string();
            if let Some(dispatcher) = super::dispatcher_slot().lock().ok().and_then(|g| g.clone()) {
                dispatcher(&command_id, cx);
            } else {
                eprintln!("[macos-menu] no dispatcher for command {command_id}");
            }
        });

        cx.bind_keys(menu_key_bindings());

        let manifest = MenuManifest::load();
        let mut menus: Vec<Menu> = Vec::with_capacity(manifest.menus.len() + 1);
        menus.push(application_menu());
        menus.extend(manifest.menus.iter().map(|menu| Menu {
            name: menu.label.clone().into(),
            items: convert_items(&super::items_without_application_menu_commands(&menu.items)),
            disabled: false,
        }));

        cx.set_menus(menus);
    }

    /// The leading menu AppKit titles with the process name.
    fn application_menu() -> Menu {
        let items = super::application_menu_entries()
            .into_iter()
            .map(|entry| match entry {
                ApplicationMenuEntry::Separator => GpuiMenuItem::separator(),
                ApplicationMenuEntry::Services { label } => {
                    GpuiMenuItem::os_submenu(label, SystemMenuType::Services)
                }
                ApplicationMenuEntry::Command { label, command } => GpuiMenuItem::action(
                    label,
                    RunMenuCommand {
                        command_id: SharedString::new_static(command),
                    },
                ),
            })
            .collect();

        Menu {
            name: APP_WINDOW_TITLE.into(),
            items,
            disabled: false,
        }
    }

    fn convert_items(items: &[AppMenuItem]) -> Vec<GpuiMenuItem> {
        items
            .iter()
            .filter(|item| item.visible)
            .filter_map(convert_item)
            .collect()
    }

    fn convert_item(item: &AppMenuItem) -> Option<GpuiMenuItem> {
        match item.kind {
            MenuItemKind::Separator => Some(GpuiMenuItem::separator()),
            MenuItemKind::Submenu => {
                let label = item.label.clone().unwrap_or_else(|| item.id.clone());
                Some(GpuiMenuItem::submenu(Menu {
                    name: label.into(),
                    items: convert_items(&item.children),
                    disabled: false,
                }))
            }
            MenuItemKind::Normal | MenuItemKind::Checkbox | MenuItemKind::Radio => {
                let command = item.command.as_deref().unwrap_or("noop");
                if command == "noop" && !item.enabled {
                    return None;
                }
                let name = item.label.clone().unwrap_or_else(|| item.id.clone());
                // Ensure the action payload owns its command id ('static).
                let command_id: SharedString = command.to_string().into();
                Some(GpuiMenuItem::action(name, RunMenuCommand { command_id }))
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn install_native_macos_menu_inner(cx: &mut App) {
    macos::install(cx);
}

#[cfg(test)]
mod application_menu_tests {
    use super::*;
    use crate::menu::MenuManifest;

    fn menu_items(id: &str) -> Vec<AppMenuItem> {
        let manifest = MenuManifest::load();
        let menu = manifest
            .menus
            .iter()
            .find(|menu| menu.id == id)
            .unwrap_or_else(|| panic!("manifest menu {id}"));
        items_without_application_menu_commands(&menu.items)
    }

    fn commands(items: &[AppMenuItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|item| item.command.as_deref())
            .collect()
    }

    #[test]
    fn the_application_menu_leads_with_about_and_ends_with_quit() {
        let entries = application_menu_entries();

        assert_eq!(
            entries.first(),
            Some(&ApplicationMenuEntry::Command {
                label: format!("About {APP_WINDOW_TITLE}"),
                command: "app:about",
            })
        );
        assert_eq!(
            entries.last(),
            Some(&ApplicationMenuEntry::Command {
                label: format!("Quit {APP_WINDOW_TITLE}"),
                command: "app:quit",
            })
        );
    }

    #[test]
    fn the_application_menu_covers_every_relocated_command() {
        let entries = application_menu_entries();
        for command in APPLICATION_MENU_COMMANDS {
            assert!(
                entries.iter().any(|entry| matches!(
                    entry,
                    ApplicationMenuEntry::Command { command: id, .. } if id == command
                )),
                "application menu is missing {command}"
            );
        }
    }

    #[test]
    fn the_file_menu_keeps_its_own_items() {
        let items = menu_items("file");
        let commands = commands(&items);

        assert!(commands.contains(&"project:new"));
        assert!(commands.contains(&"project:open"));
        assert!(commands.contains(&"project:save"));
        assert!(commands.contains(&"project:close"));
        assert!(!commands.contains(&"app:quit"));
    }

    #[test]
    fn relocated_items_leave_no_orphan_separator() {
        for id in ["file", "edit", "help"] {
            let items = menu_items(id);
            assert!(
                !commands(&items)
                    .iter()
                    .any(|command| APPLICATION_MENU_COMMANDS.contains(command)),
                "{id} still offers an application-menu command"
            );
            assert_ne!(
                items.last().map(|item| item.kind.clone()),
                Some(MenuItemKind::Separator),
                "{id} ends with a dangling separator"
            );
            assert!(
                !items
                    .windows(2)
                    .any(|pair| pair.iter().all(|item| item.kind == MenuItemKind::Separator)),
                "{id} has consecutive separators"
            );
        }
    }
}
