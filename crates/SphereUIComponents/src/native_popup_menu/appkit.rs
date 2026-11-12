//! macOS popup menus: an `NSMenu` tracked by AppKit.
//!
//! Untyped `AnyObject` message sends, as in
//! `components/plugin_editor_mac_region/appkit.rs`: the workspace builds
//! `objc2-app-kit` with only `NSGraphics`, so `NSMenu`/`NSView` are not
//! available as typed classes here.
//!
//! The chosen row is reported by a tiny target class
//! (`FutureboardPopupMenuTarget`) whose `fbPick:` action stores the item's tag
//! in [`PICKED`] while the tracking loop runs; tags index the command list
//! built alongside the menu, so nothing but an integer crosses AppKit.

use std::cell::{Cell, RefCell};

use gpui::{App, Bounds, Pixels, Point, Window};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::sel;
use objc2_foundation::{NSPoint, NSRect, NSString};

use super::{PopupItem, PopupResultCb};

/// Tags start here; 0 means "nothing chosen".
const FIRST_TAG: isize = 1;

// `NSEventModifierFlags`.
const MOD_SHIFT: usize = 1 << 17;
const MOD_CONTROL: usize = 1 << 18;
const MOD_OPTION: usize = 1 << 19;
const MOD_COMMAND: usize = 1 << 20;

thread_local! {
    /// Tag of the row chosen in the menu being tracked on this thread.
    static PICKED: Cell<isize> = const { Cell::new(0) };
    /// Views with a menu queued or up, from `show` until its result has been
    /// delivered.
    static OPEN: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn ns_view_of(window: &Window) -> Option<*mut AnyObject> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(raw) => Some(raw.ns_view.as_ptr().cast()),
        _ => None,
    }
}

pub(super) fn is_open(window: &Window) -> bool {
    ns_view_of(window)
        .is_some_and(|view| OPEN.with(|open| open.borrow().contains(&(view as usize))))
}

pub(super) fn show(
    window: &mut Window,
    cx: &mut App,
    position: Point<Pixels>,
    exclude: Option<Bounds<Pixels>>,
    items: Vec<PopupItem>,
    on_result: PopupResultCb,
) -> bool {
    if items.is_empty() {
        return false;
    }
    let Some(view_ptr) = ns_view_of(window) else {
        return false;
    };
    let key = view_ptr as usize;
    if OPEN.with(|open| open.borrow().contains(&key)) {
        // One menu at a time per window: the one already up wins.
        return true;
    }
    if target_class().is_none() {
        return false;
    }
    // Keep the view alive until the menu has been tracked, even if the window
    // closes in between.
    // SAFETY: GPUI reported this pointer for the live window's view, and we
    // are on the main thread that owns it.
    let Some(view) = (unsafe { Retained::retain(view_ptr) }) else {
        return false;
    };
    // A menu-bar dropdown opens below the label it drops from; GPUI points
    // and AppKit points are the same unit, so only the origin needs flipping.
    let anchor = match exclude {
        Some(bounds) => Point {
            x: bounds.origin.x,
            y: bounds.bottom_right().y,
        },
        None => position,
    };
    let dark = crate::theme::is_dark();

    OPEN.with(|open| open.borrow_mut().push(key));
    window
        .spawn(cx, async move |cx| {
            let chosen = track(&view, anchor, &items, dark);
            let _ = cx.update(|window, cx| on_result(chosen, window, cx));
            OPEN.with(|open| open.borrow_mut().retain(|open| *open != key));
        })
        .detach();
    true
}

/// Build the menu, track it until it closes, return the chosen command.
fn track(view: &AnyObject, at: Point<Pixels>, items: &[PopupItem], dark: bool) -> Option<String> {
    let target_class = target_class()?;
    let mut commands: Vec<String> = Vec::new();
    // SAFETY: AppKit calls on the main thread (the task runs on the window's
    // foreground executor) on objects created or retained here.
    unsafe {
        let target: Retained<AnyObject> = msg_send![target_class, new];
        let menu = build(items, &target, &mut commands)?;
        apply_appearance(&menu, dark);

        let x: f32 = at.x.into();
        let y: f32 = at.y.into();
        let flipped: Bool = msg_send![view, isFlipped];
        let y = if flipped.as_bool() {
            y as f64
        } else {
            let bounds: NSRect = msg_send![view, bounds];
            bounds.size.height - y as f64
        };
        let location = NSPoint::new(x as f64, y);

        PICKED.with(|picked| picked.set(0));
        let _: Bool = msg_send![
            &*menu,
            popUpMenuPositioningItem: std::ptr::null::<AnyObject>(),
            atLocation: location,
            inView: view
        ];
        // The action is sent before the tracking call returns; keep the
        // target alive until here (menu items hold it weakly).
        drop(target);
    }
    let tag = PICKED.with(|picked| picked.replace(0));
    tag.checked_sub(FIRST_TAG)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| commands.get(index).cloned())
}

/// One menu level; submenus are attached to their rows.
///
/// # Safety
/// Main thread only.
unsafe fn build(
    items: &[PopupItem],
    target: &AnyObject,
    commands: &mut Vec<String>,
) -> Option<Retained<AnyObject>> {
    let menu_class = AnyClass::get(c"NSMenu")?;
    let menu: Retained<AnyObject> = msg_send![menu_class, new];
    // Enablement comes from the items, not from AppKit's responder-chain
    // validation.
    let _: () = msg_send![&*menu, setAutoenablesItems: false];

    let mut last_was_separator = true;
    for item in items {
        match item {
            PopupItem::Separator => {
                // Never lead with a separator or stack two.
                if !last_was_separator {
                    let Some(item_class) = AnyClass::get(c"NSMenuItem") else {
                        continue;
                    };
                    let separator: Retained<AnyObject> = msg_send![item_class, separatorItem];
                    let _: () = msg_send![&*menu, addItem: &*separator];
                    last_was_separator = true;
                }
            }
            PopupItem::Header(label) => {
                if let Some(row) = new_item(label, "", 0) {
                    let _: () = msg_send![&*row, setEnabled: false];
                    let _: () = msg_send![&*menu, addItem: &*row];
                    last_was_separator = false;
                }
            }
            PopupItem::Command {
                label,
                command,
                shortcut,
                checked,
                enabled,
            } => {
                let (key, modifiers) = shortcut
                    .as_deref()
                    .and_then(key_equivalent)
                    .unwrap_or_default();
                let Some(row) = new_item(label, &key, modifiers) else {
                    continue;
                };
                commands.push(command.clone());
                let tag = FIRST_TAG + commands.len() as isize - 1;
                let _: () = msg_send![&*row, setTag: tag];
                let _: () = msg_send![&*row, setTarget: target];
                let _: () = msg_send![&*row, setAction: sel!(fbPick:)];
                let _: () = msg_send![&*row, setEnabled: *enabled];
                if *checked {
                    // NSControlStateValueOn
                    let _: () = msg_send![&*row, setState: 1isize];
                }
                let _: () = msg_send![&*menu, addItem: &*row];
                last_was_separator = false;
            }
            PopupItem::Submenu {
                label,
                enabled,
                items,
            } => {
                let Some(child) = build(items, target, commands) else {
                    continue;
                };
                let Some(row) = new_item(label, "", 0) else {
                    continue;
                };
                let _: () = msg_send![&*row, setSubmenu: &*child];
                let _: () = msg_send![&*row, setEnabled: *enabled];
                let _: () = msg_send![&*menu, addItem: &*row];
                last_was_separator = false;
            }
        }
    }
    // A trailing separator reads as a stray line.
    if last_was_separator {
        let count: isize = msg_send![&*menu, numberOfItems];
        if count > 0 {
            let _: () = msg_send![&*menu, removeItemAtIndex: count - 1];
        }
    }
    Some(menu)
}

/// # Safety
/// Main thread only.
unsafe fn new_item(label: &str, key: &str, modifiers: usize) -> Option<Retained<AnyObject>> {
    let item_class = AnyClass::get(c"NSMenuItem")?;
    let title = NSString::from_str(label);
    let key = NSString::from_str(key);
    let allocated: *mut AnyObject = msg_send![item_class, alloc];
    let item: *mut AnyObject = msg_send![
        allocated,
        initWithTitle: &*title,
        action: None::<Sel>,
        keyEquivalent: &*key
    ];
    let item = Retained::from_raw(item)?;
    if modifiers != 0 {
        let _: () = msg_send![&*item, setKeyEquivalentModifierMask: modifiers];
    }
    Some(item)
}

/// Match the app theme, not the OS setting, like the Windows menus do.
///
/// # Safety
/// Main thread only.
unsafe fn apply_appearance(menu: &AnyObject, dark: bool) {
    let Some(appearance_class) = AnyClass::get(c"NSAppearance") else {
        return;
    };
    let name = NSString::from_str(if dark {
        "NSAppearanceNameDarkAqua"
    } else {
        "NSAppearanceNameAqua"
    });
    let appearance: *mut AnyObject = msg_send![appearance_class, appearanceNamed: &*name];
    if !appearance.is_null() {
        let _: () = msg_send![menu, setAppearance: appearance];
    }
}

/// The target class whose `fbPick:` records the chosen row's tag.
fn target_class() -> Option<&'static AnyClass> {
    use std::sync::OnceLock;

    static CLASS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        let Some(superclass) = AnyClass::get(c"NSObject") else {
            return 0;
        };
        let Some(mut builder) = ClassBuilder::new(c"FutureboardPopupMenuTarget", superclass) else {
            return AnyClass::get(c"FutureboardPopupMenuTarget")
                .map(|cls| cls as *const _ as usize)
                .unwrap_or(0);
        };
        // SAFETY: an action method: `(id self, SEL _cmd, id sender) -> void`.
        unsafe {
            builder.add_method(sel!(fbPick:), pick as unsafe extern "C-unwind" fn(_, _, _));
        }
        builder.register() as *const _ as usize
    });
    if ptr == 0 {
        return None;
    }
    // SAFETY: registered classes live for the life of the process.
    Some(unsafe { &*(ptr as *const AnyClass) })
}

/// `-[FutureboardPopupMenuTarget fbPick:]`.
unsafe extern "C-unwind" fn pick(_this: *mut AnyObject, _cmd: Sel, sender: *mut AnyObject) {
    if sender.is_null() {
        return;
    }
    // SAFETY: `sender` is the NSMenuItem sending its action.
    let tag: isize = unsafe { msg_send![sender, tag] };
    PICKED.with(|picked| picked.set(tag));
}

/// An `NSMenuItem` key equivalent for a displayed shortcut such as `⇧⌘Z`,
/// `⌘⌫` or `Ctrl+Shift+S`. `None` when the text is not a single keystroke this
/// can express; the row is then shown without one rather than with a wrong one.
fn key_equivalent(shortcut: &str) -> Option<(String, usize)> {
    let shortcut = shortcut.trim();
    if shortcut.is_empty() {
        return None;
    }
    let mut modifiers = 0usize;
    let key_text: String = if shortcut.contains('+') && shortcut.chars().count() > 1 {
        // "Ctrl+Shift+S" style. A trailing "+" key itself is "Ctrl++".
        let (mods, key) = match shortcut.strip_suffix("++") {
            Some(head) => (head, "+"),
            None => shortcut.rsplit_once('+')?,
        };
        for part in mods.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "cmd" | "command" | "⌘" | "super" | "meta" => modifiers |= MOD_COMMAND,
                "ctrl" | "control" | "⌃" => modifiers |= MOD_CONTROL,
                "alt" | "opt" | "option" | "⌥" => modifiers |= MOD_OPTION,
                "shift" | "⇧" => modifiers |= MOD_SHIFT,
                "" => {}
                _ => return None,
            }
        }
        key.trim().to_string()
    } else {
        let mut rest = shortcut;
        loop {
            let mut chars = rest.chars();
            let flag = match chars.next() {
                Some('⌃') => MOD_CONTROL,
                Some('⌥') => MOD_OPTION,
                Some('⇧') => MOD_SHIFT,
                Some('⌘') => MOD_COMMAND,
                _ => break,
            };
            modifiers |= flag;
            rest = chars.as_str();
        }
        rest.to_string()
    };
    let key = named_key(&key_text).or_else(|| {
        let mut chars = key_text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c.to_lowercase().to_string()),
            _ => None,
        }
    })?;
    Some((key, modifiers))
}

fn named_key(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let code: u32 = match lower.as_str() {
        "⌫" | "backspace" => 0x08,
        "⌦" | "delete" | "del" => 0xF728, // NSDeleteFunctionKey
        "↩" | "enter" | "return" => 0x0D,
        "⎋" | "esc" | "escape" => 0x1B,
        "⇥" | "tab" => 0x09,
        "␣" | "space" => 0x20,
        "↑" | "up" => 0xF700,
        "↓" | "down" => 0xF701,
        "←" | "left" => 0xF702,
        "→" | "right" => 0xF703,
        "↖" | "home" => 0xF729,
        "↘" | "end" => 0xF72B,
        "⇞" | "pageup" => 0xF72C,
        "⇟" | "pagedown" => 0xF72D,
        _ => {
            let number = lower.strip_prefix('f')?.parse::<u32>().ok()?;
            if !(1..=24).contains(&number) {
                return None;
            }
            0xF704 + number - 1 // NSF1FunctionKey…
        }
    };
    char::from_u32(code).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_glyph_shortcuts_become_key_equivalents() {
        assert_eq!(
            key_equivalent("⇧⌘Z"),
            Some(("z".into(), MOD_SHIFT | MOD_COMMAND))
        );
        assert_eq!(key_equivalent("⌘⌫"), Some(("\u{8}".into(), MOD_COMMAND)));
        assert_eq!(key_equivalent("F9"), Some(("\u{F70C}".into(), 0)));
    }

    #[test]
    fn spelled_out_shortcuts_parse_too() {
        assert_eq!(
            key_equivalent("Ctrl+Shift+S"),
            Some(("s".into(), MOD_CONTROL | MOD_SHIFT))
        );
        assert_eq!(key_equivalent("Cmd++"), Some(("+".into(), MOD_COMMAND)));
    }

    #[test]
    fn unexpressible_shortcuts_are_left_off() {
        assert_eq!(key_equivalent(""), None);
        assert_eq!(key_equivalent("Hyper+Q"), None);
        assert_eq!(key_equivalent("Ctrl+K Ctrl+S"), None);
    }
}
