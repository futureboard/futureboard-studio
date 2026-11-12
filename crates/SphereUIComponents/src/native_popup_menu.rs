//! OS-native popup menus: the context menus and the menu-bar dropdowns, drawn
//! and tracked by the platform instead of by GPUI.
//!
//! Callers describe a menu as [`PopupItem`]s and call [`show`] with a point in
//! the window's coordinates. The menu comes up at that point; when it closes,
//! the callback runs on the window with the chosen item's command, or with
//! `None` when it was dismissed.
//!
//! ## Windows
//!
//! `TrackPopupMenuEx` runs a modal loop of its own until the menu closes, and
//! that loop dispatches the window's messages. Run from a GPUI callback, the
//! window procedure would re-enter GPUI while it is mid-update. So [`show`]
//! only queues the menu and posts [`WM_FB_POPUP`] to the window; a subclass in
//! front of GPUI's window procedure takes the message from the top of the
//! message loop, where nothing is borrowed, and tracks the menu there. The
//! choice is handed back through a one-shot slot that a task on the window
//! awaits — the same hand-off the native menu bar uses for `WM_COMMAND`.
//!
//! The menus take the app theme's appearance (dark or light), not the OS
//! setting: see [`crate::native_win32_menu::theme_popups_for`].
//!
//! ## macOS
//!
//! An `NSMenu` popped up with `popUpMenuPositioningItem:atLocation:inView:`
//! in the window's content view. That call runs AppKit's menu-tracking loop
//! until the menu closes, so — as on Windows — [`show`] never tracks inside
//! the GPUI callback that asked for it: it spawns a task on the window, and
//! the task's first poll (from the main dispatch queue, with nothing in GPUI
//! borrowed) tracks the menu and then hands the choice back through
//! `cx.update`. The menu takes the app theme's appearance.
//!
//! ## Elsewhere
//!
//! [`available`] is false and [`show`] declines, so callers keep drawing their
//! own menu.

use gpui::{App, Bounds, Pixels, Point, Window};

/// One row of a native popup menu.
#[derive(Clone, Debug)]
pub enum PopupItem {
    /// Runs `command` when chosen.
    Command {
        label: String,
        command: String,
        shortcut: Option<String>,
        checked: bool,
        enabled: bool,
    },
    /// A group caption: shown, never chosen.
    Header(String),
    Separator,
    Submenu {
        label: String,
        enabled: bool,
        items: Vec<PopupItem>,
    },
}

/// Called once the menu closes: the chosen command, or `None` if dismissed.
pub type PopupResultCb = Box<dyn FnOnce(Option<String>, &mut Window, &mut App) + 'static>;

/// Whether this platform shows popup menus natively.
pub fn available() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

/// Whether a native menu opened by [`show`] is still up (or about to come up)
/// on `window`. A drawn menu's state stays open while its native menu is up,
/// so the render that would show it again checks this first.
pub fn is_open(window: &Window) -> bool {
    #[cfg(target_os = "windows")]
    {
        win32::is_open(window)
    }
    #[cfg(target_os = "macos")]
    {
        appkit::is_open(window)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = window;
        false
    }
}

/// Bring up `items` at `position` (window coordinates). `exclude`, when
/// given, is a rectangle the menu must not cover — the menu-bar label it
/// drops from — and the menu opens below it.
///
/// Returns `false` without calling `on_result` when the menu cannot be shown
/// natively here (another platform, no native handle, nothing to show): the
/// caller draws its own instead.
pub fn show(
    window: &mut Window,
    cx: &mut App,
    position: Point<Pixels>,
    exclude: Option<Bounds<Pixels>>,
    items: Vec<PopupItem>,
    on_result: PopupResultCb,
) -> bool {
    #[cfg(target_os = "windows")]
    {
        win32::show(window, cx, position, exclude, items, on_result)
    }
    #[cfg(target_os = "macos")]
    {
        appkit::show(window, cx, position, exclude, items, on_result)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (window, cx, position, exclude, items, on_result);
        false
    }
}

/// A menu label as the platform reads it: `&` marks a mnemonic on Windows,
/// so a literal one is doubled.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn escape_label(label: &str) -> String {
    label.replace('&', "&&")
}

/// The text of a row: the label, then the shortcut after a tab (Windows
/// right-aligns what follows the tab).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn row_text(label: &str, shortcut: Option<&str>) -> String {
    match shortcut.filter(|shortcut| !shortcut.is_empty()) {
        Some(shortcut) => format!("{}\t{}", escape_label(label), shortcut),
        None => escape_label(label),
    }
}

#[cfg(target_os = "windows")]
mod win32 {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll, Waker};

    use gpui::{App, Bounds, Pixels, Point, Window};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetAncestor, PostMessageW, SetForegroundWindow,
        TrackPopupMenuEx, GA_ROOT, HMENU, MENU_ITEM_FLAGS, MF_CHECKED, MF_DISABLED, MF_GRAYED,
        MF_POPUP, MF_SEPARATOR, MF_STRING, TPMPARAMS, TPM_LEFTALIGN, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, TPM_TOPALIGN, TPM_VERTICAL, WM_APP, WM_NCDESTROY,
    };

    use super::{row_text, PopupItem, PopupResultCb};

    /// Posted to a window to track its queued popup menu.
    pub(super) const WM_FB_POPUP: u32 = WM_APP + 0x4650; // "FP"
    /// This module's subclass id on a window.
    const SUBCLASS_ID: usize = 0x4642_5050; // "FBPP"
    /// Menu command ids start here; 0 means "dismissed".
    const FIRST_ID: usize = 1;

    /// The one-shot hand-off from the message loop back to the window's task.
    #[derive(Default)]
    struct Reply {
        slot: Mutex<(Option<Option<String>>, Option<Waker>)>,
    }

    impl Reply {
        fn send(&self, value: Option<String>) {
            let waker = {
                let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
                slot.0 = Some(value);
                slot.1.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    struct ReplyFuture(Arc<Reply>);

    impl Future for ReplyFuture {
        type Output = Option<String>;
        fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
            let mut slot = self.0.slot.lock().unwrap_or_else(|e| e.into_inner());
            match slot.0.take() {
                Some(value) => Poll::Ready(value),
                None => {
                    slot.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }

    /// A menu waiting for its window to take [`WM_FB_POPUP`].
    struct Request {
        items: Vec<PopupItem>,
        /// Screen point, physical pixels.
        at: POINT,
        /// Screen rectangle to keep clear, physical pixels.
        exclude: Option<RECT>,
        reply: Arc<Reply>,
    }

    thread_local! {
        /// Queued menus, by window.
        static PENDING: RefCell<HashMap<isize, Request>> = RefCell::new(HashMap::new());
        /// Windows with a menu queued or up, from `show` until its result
        /// has been delivered.
        static OPEN: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
        /// Windows this module has subclassed.
        static SUBCLASSED: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    }

    fn hwnd_of(window: &Window) -> Option<HWND> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(raw) => Some(HWND(raw.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    pub(super) fn is_open(window: &Window) -> bool {
        hwnd_of(window)
            .is_some_and(|hwnd| OPEN.with(|open| open.borrow().contains(&(hwnd.0 as isize))))
    }

    fn to_screen(hwnd: HWND, point: Point<Pixels>, scale: f32) -> POINT {
        let x: f32 = point.x.into();
        let y: f32 = point.y.into();
        let mut screen = POINT {
            x: (x * scale).round() as i32,
            y: (y * scale).round() as i32,
        };
        // SAFETY: a point conversion on a live window this thread owns.
        let _ = unsafe { ClientToScreen(hwnd, &mut screen) };
        screen
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
        let Some(hwnd) = hwnd_of(window) else {
            return false;
        };
        let key = hwnd.0 as isize;
        if OPEN.with(|open| open.borrow().contains(&key)) {
            // One menu at a time per window: the one already up wins.
            return true;
        }
        if !ensure_subclass(hwnd) {
            return false;
        }
        let scale = window.scale_factor();
        let at = to_screen(hwnd, position, scale);
        let exclude = exclude.map(|bounds| {
            let top_left = to_screen(hwnd, bounds.origin, scale);
            let bottom_right = to_screen(hwnd, bounds.bottom_right(), scale);
            RECT {
                left: top_left.x,
                top: top_left.y,
                right: bottom_right.x,
                bottom: bottom_right.y,
            }
        });
        let reply = Arc::new(Reply::default());
        PENDING.with(|pending| {
            pending.borrow_mut().insert(
                key,
                Request {
                    items,
                    at,
                    exclude,
                    reply: reply.clone(),
                },
            )
        });
        // SAFETY: posting a private message to a live window this thread owns.
        if unsafe { PostMessageW(Some(hwnd), WM_FB_POPUP, WPARAM(0), LPARAM(0)) }.is_err() {
            PENDING.with(|pending| pending.borrow_mut().remove(&key));
            return false;
        }
        OPEN.with(|open| open.borrow_mut().push(key));
        window
            .spawn(cx, async move |cx| {
                let chosen = ReplyFuture(reply).await;
                let _ = cx.update(|window, cx| on_result(chosen, window, cx));
                OPEN.with(|open| open.borrow_mut().retain(|hwnd| *hwnd != key));
            })
            .detach();
        true
    }

    fn ensure_subclass(hwnd: HWND) -> bool {
        let key = hwnd.0 as isize;
        if SUBCLASSED.with(|done| done.borrow().contains(&key)) {
            return true;
        }
        // SAFETY: installing a subclass on a live window this thread owns;
        // the procedure is removed on `WM_NCDESTROY`.
        let installed = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
        if installed.as_bool() {
            SUBCLASSED.with(|done| done.borrow_mut().push(key));
        }
        installed.as_bool()
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        let key = hwnd.0 as isize;
        match message {
            WM_FB_POPUP => {
                if let Some(request) = PENDING.with(|pending| pending.borrow_mut().remove(&key)) {
                    let chosen = track(hwnd, &request);
                    request.reply.send(chosen);
                }
                LRESULT(0)
            }
            WM_NCDESTROY => {
                if let Some(request) = PENDING.with(|pending| pending.borrow_mut().remove(&key)) {
                    request.reply.send(None);
                }
                SUBCLASSED.with(|done| done.borrow_mut().retain(|h| *h != key));
                // SAFETY: removing this module's own subclass.
                let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
                // SAFETY: forwarding to the next procedure in the chain.
                unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
            }
            // SAFETY: forwarding to the next procedure in the chain.
            _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
        }
    }

    /// Builds the menu, tracks it until it closes, returns the choice.
    fn track(hwnd: HWND, request: &Request) -> Option<String> {
        let mut commands: Vec<String> = Vec::new();
        let menu = build(&request.items, &mut commands)?;
        crate::native_win32_menu::theme_popups_for(hwnd);
        let mut params = TPMPARAMS {
            cbSize: std::mem::size_of::<TPMPARAMS>() as u32,
            rcExclude: request.exclude.unwrap_or_default(),
        };
        // SAFETY: Win32 calls on a live window and the menu built above; the
        // window must be foreground for the menu to close on an outside click.
        // A plug-in editor is a child of the host's window, which cannot be
        // foreground itself: its top-level ancestor is (Studio's own window
        // is its own root).
        let picked = unsafe {
            let root = GetAncestor(hwnd, GA_ROOT);
            let _ = SetForegroundWindow(if root.0.is_null() { hwnd } else { root });
            let picked = TrackPopupMenuEx(
                menu,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_VERTICAL).0,
                request.at.x,
                request.at.y,
                hwnd,
                request.exclude.is_some().then_some(&mut params as *mut _),
            );
            let _ = DestroyMenu(menu);
            picked.0 as usize
        };
        picked
            .checked_sub(FIRST_ID)
            .and_then(|index| commands.get(index).cloned())
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// One popup level. Its submenus are owned by it and go with
    /// `DestroyMenu` on the root.
    fn build(items: &[PopupItem], commands: &mut Vec<String>) -> Option<HMENU> {
        // SAFETY: creating and filling a menu this function owns.
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let mut last_was_separator = true;
            for item in items {
                match item {
                    PopupItem::Separator => {
                        // Never lead with a separator or stack two.
                        if !last_was_separator {
                            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                            last_was_separator = true;
                        }
                    }
                    PopupItem::Header(label) => {
                        let text = wide(&row_text(label, None));
                        let _ = AppendMenuW(
                            menu,
                            MF_STRING | MF_GRAYED | MF_DISABLED,
                            0,
                            PCWSTR(text.as_ptr()),
                        );
                        last_was_separator = false;
                    }
                    PopupItem::Command {
                        label,
                        command,
                        shortcut,
                        checked,
                        enabled,
                    } => {
                        commands.push(command.clone());
                        let id = FIRST_ID + commands.len() - 1;
                        let mut flags: MENU_ITEM_FLAGS = MF_STRING;
                        if *checked {
                            flags |= MF_CHECKED;
                        }
                        if !*enabled {
                            flags |= MF_GRAYED;
                        }
                        let text = wide(&row_text(label, shortcut.as_deref()));
                        let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
                        last_was_separator = false;
                    }
                    PopupItem::Submenu {
                        label,
                        enabled,
                        items,
                    } => {
                        let Some(child) = build(items, commands) else {
                            continue;
                        };
                        let mut flags: MENU_ITEM_FLAGS = MF_POPUP;
                        if !*enabled {
                            flags |= MF_GRAYED;
                        }
                        let text = wide(&row_text(label, None));
                        let _ = AppendMenuW(menu, flags, child.0 as usize, PCWSTR(text.as_ptr()));
                        last_was_separator = false;
                    }
                }
            }
            Some(menu)
        }
    }
}

#[cfg(target_os = "macos")]
mod appkit;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_ampersand_is_not_a_mnemonic() {
        assert_eq!(escape_label("Drums & Bass"), "Drums && Bass");
    }

    #[test]
    fn the_shortcut_follows_a_tab() {
        assert_eq!(row_text("Copy", Some("Ctrl+C")), "Copy\tCtrl+C");
        assert_eq!(row_text("Copy", Some("")), "Copy");
        assert_eq!(row_text("A & B", None), "A && B");
    }
}
