//! macOS native menubar — maps [`MenuManifest`] to GPUI `cx.set_menus`.
//!
//! Command dispatch uses shared manifest command IDs so keyboard shortcuts and
//! GPUI dropdowns stay aligned with the system menu.
//!
//! Labels go through the same Fluent keys as the in-window menu bar
//! ([`I18n::tr_menu`]), and the menu is rebuilt whenever the UI language or a
//! checkable item's state changes — AppKit menus are static once installed, so
//! installing them once at startup left them in English and without ticks.

use std::sync::{Arc, Mutex, OnceLock};

use gpui::App;

use crate::i18n::I18n;
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
        label: String,
    },
}

/// Rows of the macOS application menu, in AppKit order, labelled in `i18n`.
///
/// AppKit renders the first menu of the main menu as the application menu: it
/// replaces the title with the process name and owns the About / Settings /
/// Quit slots. Without this leading menu the manifest's first menu — File —
/// becomes the application menu, so its items are reachable only under the app
/// name and every later menu shifts one place to the left.
pub fn application_menu_entries(i18n: I18n) -> Vec<ApplicationMenuEntry> {
    let app = || vec![("app", APP_WINDOW_TITLE.to_string())];
    vec![
        ApplicationMenuEntry::Command {
            label: i18n.tr_vars("menu.app-about", &app()),
            command: "app:about",
        },
        ApplicationMenuEntry::Command {
            label: i18n.tr("menu.app-check_updates"),
            command: "app:check-for-updates",
        },
        ApplicationMenuEntry::Separator,
        ApplicationMenuEntry::Command {
            label: i18n.tr("menu.app-settings"),
            command: "app:preferences",
        },
        ApplicationMenuEntry::Separator,
        ApplicationMenuEntry::Services {
            label: i18n.tr("menu.app-services"),
        },
        ApplicationMenuEntry::Separator,
        ApplicationMenuEntry::Command {
            label: i18n.tr_vars("menu.app-quit", &app()),
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

/// What the native menu currently shows: its language and the state of every
/// checkable item. The menu is rebuilt only when this changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativeMenuState {
    pub language: String,
    /// `(manifest item id, checked)`.
    pub checks: Vec<(&'static str, bool)>,
}

static INSTALLED_STATE: OnceLock<Mutex<Option<NativeMenuState>>> = OnceLock::new();

fn installed_state() -> &'static Mutex<Option<NativeMenuState>> {
    INSTALLED_STATE.get_or_init(|| Mutex::new(None))
}

/// Install the application menu from the shared manifest in the current UI
/// language. No-op off macOS.
pub fn install_native_macos_menu(cx: &mut App) {
    #[cfg(target_os = "macos")]
    {
        if !crate::platform_chrome::PlatformChromePolicy::current().use_native_macos_menubar {
            return;
        }
        let state = NativeMenuState {
            language: I18n::from_app(cx).locale().code().to_string(),
            checks: Vec::new(),
        };
        macos::install(cx);
        macos::set_menus(cx, &state);
        *installed_state().lock().expect("native menu state lock") = Some(state);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

/// Rebuild the menu if the language or a checkable item changed since it was
/// last installed. Cheap to call every frame: the comparison is all it does
/// until something changes, and the rebuild itself is deferred out of the
/// caller's update. No-op off macOS or before the menu is installed.
pub fn refresh_native_macos_menu(cx: &mut App, state: NativeMenuState) {
    #[cfg(target_os = "macos")]
    {
        let mut installed = installed_state().lock().expect("native menu state lock");
        match installed.as_ref() {
            None => return,
            Some(current) if *current == state => return,
            Some(_) => {}
        }
        *installed = Some(state.clone());
        drop(installed);
        cx.defer(move |cx| macos::set_menus(cx, &state));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (cx, state);
}

#[cfg(target_os = "macos")]
mod macos {
    use gpui::{App, KeyBinding, Menu, MenuItem as GpuiMenuItem, SharedString, SystemMenuType};

    use super::{ApplicationMenuEntry, NativeMenuState, APP_WINDOW_TITLE};
    use crate::i18n::I18n;
    use crate::menu::{MenuItem as AppMenuItem, MenuItemKind, MenuManifest};

    #[derive(Clone, PartialEq, gpui::Action)]
    #[action(no_json)]
    pub(super) struct RunMenuCommand {
        pub command_id: SharedString,
    }

    /// Translate a shared-manifest accelerator (authored Windows-first, e.g.
    /// `"Ctrl+Shift+S"`, `"Alt+F4"`) into a GPUI keystroke string for the macOS
    /// menubar, where the primary modifier is Cmd. Returns `None` for anything
    /// that must not become an AppKit key equivalent:
    ///   * bare keys and Shift-only chords (would swallow a plain keypress),
    ///   * keys we can't map.
    ///
    /// Note: focus-gated edit commands (Ctrl+C/X/V/A/D/Delete) DO receive key
    /// equivalents. Their `RunMenuCommand` action routes through
    /// `dispatch_command_id` which applies focus routing internally (MIDI editor
    /// vs timeline), so AppKit firing the menu action is safe — it never touches
    /// NSTextView's cut:/copy:/paste: selectors.
    ///
    /// `app:quit` ships as `Alt+F4` for Windows; macOS quit is Cmd+Q.
    pub(super) fn manifest_accel_to_mac_keystroke(command: &str, accel: &str) -> Option<String> {
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
        // AppKit key equivalents run before GPUI's focused control. Bare and
        // Shift-only accelerators would therefore steal ordinary text entry
        // (Space, R, V, arrows, etc.) from a focused GPUI field.
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
                let text_edit_command = matches!(
                    command,
                    "edit:select-all" | "edit:copy" | "edit:cut" | "edit:paste"
                );
                if !text_edit_command && !command.is_empty() && !accel.is_empty() {
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
    }

    /// Build and install the menus for `state` (language and ticks).
    pub(super) fn set_menus(cx: &mut App, state: &NativeMenuState) {
        let i18n = I18n::new(&state.language);
        let manifest = MenuManifest::load();
        let mut menus: Vec<Menu> = Vec::with_capacity(manifest.menus.len() + 1);
        menus.push(application_menu(i18n));
        menus.extend(manifest.menus.iter().map(|menu| {
            let mut items = super::items_without_application_menu_commands(&menu.items);
            crate::menu::patch_checkbox_states(&mut items, &state.checks);
            Menu {
                name: i18n.tr_menu(&menu.id, &menu.label).into(),
                items: convert_items(&items, i18n),
                disabled: false,
            }
        }));
        cx.set_menus(menus);
    }

    /// The leading menu AppKit titles with the process name.
    fn application_menu(i18n: I18n) -> Menu {
        let items = super::application_menu_entries(i18n)
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

    fn convert_items(items: &[AppMenuItem], i18n: I18n) -> Vec<GpuiMenuItem> {
        items
            .iter()
            .filter(|item| item.visible)
            .filter_map(|item| convert_item(item, i18n))
            .collect()
    }

    fn label(item: &AppMenuItem, i18n: I18n) -> SharedString {
        let fallback = item.label.clone().unwrap_or_else(|| item.id.clone());
        i18n.tr_menu(&item.id, &fallback).into()
    }

    fn convert_item(item: &AppMenuItem, i18n: I18n) -> Option<GpuiMenuItem> {
        match item.kind {
            MenuItemKind::Separator => Some(GpuiMenuItem::separator()),
            MenuItemKind::Submenu => Some(GpuiMenuItem::submenu(Menu {
                name: label(item, i18n),
                items: convert_items(&item.children, i18n),
                disabled: false,
            })),
            MenuItemKind::Normal | MenuItemKind::Checkbox | MenuItemKind::Radio => {
                let command = item.command.as_deref().unwrap_or("noop");
                if command == "noop" && !item.enabled {
                    return None;
                }
                let command_id: SharedString = command.to_string().into();
                let action = GpuiMenuItem::action(label(item, i18n), RunMenuCommand { command_id });
                Some(if item.kind == MenuItemKind::Checkbox {
                    action.checked(item.checked)
                } else {
                    action
                })
            }
        }
    }
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
        let entries = application_menu_entries(I18n::new("en"));

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
        let entries = application_menu_entries(I18n::new("en"));
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
    fn the_application_menu_follows_the_ui_language() {
        let thai = application_menu_entries(I18n::new("th"));
        let Some(ApplicationMenuEntry::Command { label, .. }) = thai.first() else {
            panic!("about entry");
        };
        assert_ne!(label, &format!("About {APP_WINDOW_TITLE}"));
        assert!(label.contains(APP_WINDOW_TITLE), "{label}");
        assert!(
            thai.iter().any(|entry| matches!(
                entry,
                ApplicationMenuEntry::Services { label } if label != "Services"
            )),
            "Services is translated"
        );
    }

    /// Every manifest label must resolve to a real translation in every
    /// shipped locale — a missing key falls back to English silently, which
    /// is exactly how the macOS menu bar ended up half-translated.
    #[test]
    fn every_menu_label_is_translated_in_every_locale() {
        fn walk(items: &[AppMenuItem], out: &mut Vec<String>) {
            for item in items {
                if item.kind != MenuItemKind::Separator && item.label.is_some() {
                    out.push(I18n::menu_key(&item.id));
                }
                walk(&item.children, out);
            }
        }
        let mut keys = Vec::new();
        for menu in &MenuManifest::load().menus {
            keys.push(I18n::menu_key(&menu.id));
            walk(&menu.items, &mut keys);
        }
        for key in [
            "menu.app-about",
            "menu.app-check_updates",
            "menu.app-settings",
            "menu.app-services",
            "menu.app-quit",
        ] {
            keys.push(key.to_string());
        }
        for locale in crate::i18n::Locale::ALL {
            let i18n = I18n::new(locale.code());
            let missing: Vec<&String> = keys.iter().filter(|key| !i18n.has_own(key)).collect();
            assert!(
                missing.is_empty(),
                "{} is missing {missing:?}",
                locale.code()
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
