//! Native Win32 menu bar for the external tool windows (the Mixer, the MIDI
//! Editor), which take the platform's own frame on Windows
//! ([`crate::platform_chrome::native_tool_window_frame`]).
//!
//! GPUI has no menu-bar or `WM_COMMAND` support of its own, so the bar is an
//! `HMENU` set on the window and served by a window subclass installed in
//! front of GPUI's window procedure. The subclass runs inside the Win32 message
//! loop, where no GPUI context exists: a chosen command is queued on
//! [`MenuCommands`], and the window's own task picks it up and dispatches it
//! like any other command.
//!
//! The frame and the menus follow the app theme's appearance
//! ([`crate::theme::is_dark`]), not the OS setting. Windows has no dark menu
//! bar, so a dark one is drawn through the undocumented `WM_UAHDRAWMENU*`
//! messages (as Explorer and Notepad do); light keeps the system's own.
//! Popup menus take the theme through uxtheme's process-wide app mode.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

use gpui::Window;
use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetWindowDC, MapWindowPoints, OffsetRect,
    RedrawWindow, ReleaseDC, SetBkMode, SetTextColor, DRAW_TEXT_FORMAT, DT_CENTER, DT_HIDEPREFIX,
    DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, RDW_FRAME, RDW_INVALIDATE, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ODS_DISABLED, ODS_GRAYED, ODS_HOTLIGHT, ODS_INACTIVE, ODS_NOACCEL, ODS_SELECTED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F10, VK_MENU};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DefWindowProcW, DrawMenuBar, GetClientRect,
    GetMenuBarInfo, GetMenuItemInfoW, GetWindowRect, SetMenu, SetWindowPos, HMENU, MENUBARINFO,
    MENUITEMINFOW, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_STRING, OBJID_MENU, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, WM_COMMAND, WM_NCACTIVATE, WM_NCDESTROY, WM_NCLBUTTONDBLCLK,
    WM_NCLBUTTONDOWN, WM_NCPAINT, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::theme::Colors;

/// Undocumented messages the menu bar sends its window so it can be drawn
/// by the application (see the module docs).
const WM_UAHDRAWMENU: u32 = 0x0091;
const WM_UAHDRAWMENUITEM: u32 = 0x0092;
/// `HTMENU`: a non-client hit on the menu bar.
const HTMENU: usize = 5;
/// This module's subclass id on a window.
const SUBCLASS_ID: usize = 0x4642_4D4E; // "FBMN"
/// Menu command ids start here; `WM_COMMAND` carries the id in 16 bits.
const FIRST_COMMAND_ID: usize = 0x100;

/// One top-level menu of the bar.
pub struct NativeMenu {
    pub label: String,
    pub items: Vec<NativeMenuItem>,
}

/// One entry of a [`NativeMenu`].
pub enum NativeMenuItem {
    /// Runs `command` (a command id, as the Studio dispatches it).
    Command {
        label: String,
        command: String,
    },
    Separator,
}

impl NativeMenuItem {
    /// A command entry, with the shortcut the active keymap binds to it.
    pub fn command(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Command {
            label: label.into(),
            command: command.into(),
        }
    }

    /// The entry the shared menu manifest has for `command`, translated like
    /// the Studio's own menus; `fallback` labels a command the manifest
    /// does not list.
    pub fn manifest(i18n: crate::i18n::I18n, command: &str, fallback: &str) -> Self {
        fn find<'a>(
            items: &'a [crate::menu::MenuItem],
            command: &str,
        ) -> Option<&'a crate::menu::MenuItem> {
            items.iter().find_map(|item| {
                if item.command.as_deref() == Some(command) {
                    Some(item)
                } else {
                    find(&item.children, command)
                }
            })
        }
        let label = crate::menu::MenuManifest::load()
            .menus
            .iter()
            .find_map(|menu| find(&menu.items, command))
            .map(|item| i18n.tr_menu(&item.id, item.label.as_deref().unwrap_or(fallback)))
            .unwrap_or_else(|| fallback.to_string());
        Self::command(label, command)
    }
}

/// Commands chosen from one window's menu bar, waiting for that window's task
/// to dispatch them.
#[derive(Default)]
pub struct MenuCommands {
    queue: Mutex<(VecDeque<String>, Option<Waker>)>,
}

impl MenuCommands {
    fn push(&self, command: String) {
        let waker = {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.0.push_back(command);
            queue.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The next command chosen from the menu bar.
    pub async fn next(&self) -> String {
        std::future::poll_fn(|cx| {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            match queue.0.pop_front() {
                Some(command) => Poll::Ready(command),
                None => {
                    queue.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }
}

/// What a window's subclass owns, freed when the window is destroyed.
struct Attached {
    commands: Arc<MenuCommands>,
    /// Command id per menu id, from [`FIRST_COMMAND_ID`].
    ids: Vec<String>,
}

thread_local! {
    /// Windows that carry a bar from here, for re-theming them all.
    static ATTACHED: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(raw) => Some(HWND(raw.hwnd.get() as *mut _)),
        _ => None,
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Gives `window` a native menu bar of `menus`, themed like the app, and
/// grows the window by the bar's height so its content keeps its size.
/// Returns where the chosen commands arrive, or `None` when the window has
/// no Win32 handle or the bar could not be built.
pub fn attach(window: &Window, menus: &[NativeMenu]) -> Option<Arc<MenuCommands>> {
    let hwnd = hwnd_of(window)?;
    let mut ids = Vec::new();
    // SAFETY: plain Win32 calls on a window this thread owns; every string
    // outlives the call it is passed to.
    unsafe {
        let bar = CreateMenu().ok()?;
        for menu in menus {
            let popup = CreatePopupMenu().ok()?;
            for item in &menu.items {
                match item {
                    NativeMenuItem::Separator => {
                        let _ = AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null());
                    }
                    NativeMenuItem::Command { label, command } => {
                        let id = FIRST_COMMAND_ID + ids.len();
                        let text = match crate::keymap::global_keymap_shortcut(command, None) {
                            Some(shortcut) => format!("{label}\t{shortcut}"),
                            None => label.clone(),
                        };
                        let text = wide(&text);
                        let _ = AppendMenuW(popup, MF_STRING, id, PCWSTR(text.as_ptr()));
                        ids.push(command.clone());
                    }
                }
            }
            let label = wide(&menu.label);
            let _ = AppendMenuW(bar, MF_POPUP, popup.0 as usize, PCWSTR(label.as_ptr()));
        }

        let mut before = RECT::default();
        let _ = GetClientRect(hwnd, &mut before);
        SetMenu(hwnd, Some(bar)).ok()?;
        let state = Box::new(Attached {
            commands: Arc::new(MenuCommands::default()),
            ids,
        });
        let commands = state.commands.clone();
        let data = Box::into_raw(state) as usize;
        if !SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, data).as_bool() {
            drop(Box::from_raw(data as *mut Attached));
            let _ = SetMenu(hwnd, None);
            return None;
        }
        ATTACHED.with(|attached| attached.borrow_mut().push(hwnd.0 as isize));

        // The bar took its height out of the client area; give it back.
        let mut after = RECT::default();
        let _ = GetClientRect(hwnd, &mut after);
        let lost = (before.bottom - before.top) - (after.bottom - after.top);
        if lost > 0 {
            let mut frame = RECT::default();
            let _ = GetWindowRect(hwnd, &mut frame);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                frame.right - frame.left,
                frame.bottom - frame.top + lost,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        apply_theme(hwnd);
        Some(commands)
    }
}

/// Re-themes every window carrying a bar, after the app theme changed.
pub fn apply_theme_to_all() {
    let windows: Vec<isize> = ATTACHED.with(|attached| attached.borrow().clone());
    for hwnd in windows {
        apply_theme(HWND(hwnd as *mut _));
    }
}

/// Frame, popup menus and bar of `hwnd` in the app theme's appearance.
fn apply_theme(hwnd: HWND) {
    let dark = crate::theme::is_dark();
    // SAFETY: Win32 calls on a live window this thread owns.
    unsafe {
        let value: i32 = dark.into();
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&value as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        uxtheme::set_app_dark(dark);
        uxtheme::allow_dark_for_window(hwnd, dark);
        uxtheme::flush_menu_themes();
        let _ = DrawMenuBar(hwnd);
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_FRAME | RDW_INVALIDATE);
    }
}

/// uxtheme's dark-mode entry points, exported by ordinal only (Windows 10
/// 1809 and later). Each is looked up once; a missing one is skipped.
mod uxtheme {
    use super::*;
    use std::sync::OnceLock;

    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();

    fn ordinal(index: u16) -> Option<unsafe extern "system" fn() -> isize> {
        static MODULE: OnceLock<isize> = OnceLock::new();
        let module = *MODULE.get_or_init(|| {
            // SAFETY: loading a system library by name.
            unsafe { LoadLibraryW(w!("uxtheme.dll")) }
                .map(|module| module.0 as isize)
                .unwrap_or(0)
        });
        if module == 0 {
            return None;
        }
        // SAFETY: an ordinal is passed as the low word of the name pointer
        // (MAKEINTRESOURCEA).
        unsafe {
            GetProcAddress(
                windows::Win32::Foundation::HMODULE(module as *mut _),
                PCSTR(index as usize as *const u8),
            )
        }
    }

    /// Ordinal 135: `SetPreferredAppMode`. 2 forces dark, 3 forces light.
    pub(super) fn set_app_dark(dark: bool) {
        static FUNCTION: OnceLock<Option<SetPreferredAppMode>> = OnceLock::new();
        if let Some(function) = FUNCTION.get_or_init(|| {
            // SAFETY: the documented-by-convention signature of ordinal 135.
            ordinal(135).map(|f| unsafe { std::mem::transmute::<_, SetPreferredAppMode>(f) })
        }) {
            // SAFETY: see above.
            unsafe { function(if dark { 2 } else { 3 }) };
        }
    }

    /// Ordinal 133: `AllowDarkModeForWindow`.
    pub(super) fn allow_dark_for_window(hwnd: HWND, dark: bool) {
        static FUNCTION: OnceLock<Option<AllowDarkModeForWindow>> = OnceLock::new();
        if let Some(function) = FUNCTION.get_or_init(|| {
            // SAFETY: the documented-by-convention signature of ordinal 133.
            ordinal(133).map(|f| unsafe { std::mem::transmute::<_, AllowDarkModeForWindow>(f) })
        }) {
            // SAFETY: see above.
            unsafe { function(hwnd, dark.into()) };
        }
    }

    /// Ordinal 136: `FlushMenuThemes`, so open-able popups pick up the mode.
    pub(super) fn flush_menu_themes() {
        static FUNCTION: OnceLock<Option<FlushMenuThemes>> = OnceLock::new();
        if let Some(function) = FUNCTION.get_or_init(|| {
            // SAFETY: the documented-by-convention signature of ordinal 136.
            ordinal(136).map(|f| unsafe { std::mem::transmute::<_, FlushMenuThemes>(f) })
        }) {
            // SAFETY: see above.
            unsafe { function() };
        }
    }
}

/// `UAHMENU`: the bar being drawn.
#[repr(C)]
struct UahMenu {
    hmenu: HMENU,
    hdc: HDC,
    flags: u32,
}

/// `UAHMENUITEM`: which item, plus metrics this module does not read.
#[repr(C)]
struct UahMenuItem {
    position: i32,
    item_metrics: [u32; 8],
    popup_metrics: [u32; 5],
}

/// `UAHDRAWMENUITEM`, the payload of [`WM_UAHDRAWMENUITEM`].
#[repr(C)]
struct UahDrawMenuItem {
    dis: DRAWITEMSTRUCT,
    um: UahMenu,
    umi: UahMenuItem,
}

fn colorref(color: gpui::Rgba) -> COLORREF {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF(channel(color.r) | (channel(color.g) << 8) | (channel(color.b) << 16))
}

/// Fills `rect` of `hdc` with `color`.
///
/// # Safety
/// `hdc` must be a live device context.
unsafe fn fill(hdc: HDC, rect: &RECT, color: gpui::Rgba) {
    let brush: HBRUSH = CreateSolidBrush(colorref(color));
    FillRect(hdc, rect, brush);
    let _ = DeleteObject(brush.into());
}

/// The menu bar's rectangle in window coordinates.
///
/// # Safety
/// `hwnd` must be a live window with a menu bar.
unsafe fn menu_bar_rect(hwnd: HWND) -> Option<RECT> {
    let mut info = MENUBARINFO {
        cbSize: std::mem::size_of::<MENUBARINFO>() as u32,
        ..Default::default()
    };
    GetMenuBarInfo(hwnd, OBJID_MENU, 0, &mut info).ok()?;
    let mut frame = RECT::default();
    GetWindowRect(hwnd, &mut frame).ok()?;
    let mut rect = info.rcBar;
    let _ = OffsetRect(&mut rect, -frame.left, -frame.top);
    Some(rect)
}

/// Paints over the one-pixel light line the system draws under the bar,
/// which no message lets the application draw itself.
///
/// # Safety
/// `hwnd` must be a live window.
unsafe fn paint_bar_underline(hwnd: HWND) {
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let mut corners = [
        POINT {
            x: client.left,
            y: client.top,
        },
        POINT {
            x: client.right,
            y: client.bottom,
        },
    ];
    MapWindowPoints(Some(hwnd), None, &mut corners);
    let mut frame = RECT::default();
    if GetWindowRect(hwnd, &mut frame).is_err() {
        return;
    }
    let line = RECT {
        left: corners[0].x - frame.left,
        right: corners[1].x - frame.left,
        top: corners[0].y - frame.top - 1,
        bottom: corners[0].y - frame.top,
    };
    let hdc = GetWindowDC(Some(hwnd));
    if !hdc.is_invalid() {
        fill(hdc, &line, Colors::surface_titlebar());
        ReleaseDC(Some(hwnd), hdc);
    }
}

/// Draws one top-level item of a dark bar.
///
/// # Safety
/// `item` must be the payload of a live [`WM_UAHDRAWMENUITEM`].
unsafe fn draw_bar_item(item: &UahDrawMenuItem) {
    let mut text = [0u16; 256];
    let mut info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING,
        dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
        cch: (text.len() - 1) as u32,
        ..Default::default()
    };
    if GetMenuItemInfoW(item.um.hmenu, item.umi.position as u32, true, &mut info).is_err() {
        return;
    }
    let state = item.dis.itemState;
    let hot = state.0 & (ODS_HOTLIGHT.0 | ODS_SELECTED.0) != 0;
    let muted = state.0 & (ODS_INACTIVE.0 | ODS_GRAYED.0 | ODS_DISABLED.0) != 0;
    let background = if hot {
        Colors::surface_control_hover()
    } else {
        Colors::surface_titlebar()
    };
    let foreground = if muted && !hot {
        Colors::text_secondary()
    } else {
        Colors::text_primary()
    };
    let hdc = item.um.hdc;
    let mut rect = item.dis.rcItem;
    fill(hdc, &rect, background);
    let mut format = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
    if state.0 & ODS_NOACCEL.0 != 0 {
        format |= DT_HIDEPREFIX;
    }
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, colorref(foreground));
    let len = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    DrawTextW(hdc, &mut text[..len], &mut rect, DRAW_TEXT_FORMAT(format.0));
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    let state = &*(data as *const Attached);
    match msg {
        // A menu choice: high word 0 and no control handle.
        WM_COMMAND if (wparam.0 >> 16) & 0xFFFF == 0 && lparam.0 == 0 => {
            let id = wparam.0 & 0xFFFF;
            if let Some(command) = id
                .checked_sub(FIRST_COMMAND_ID)
                .and_then(|index| state.ids.get(index))
            {
                state.commands.push(command.clone());
                return LRESULT(0);
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // GPUI takes non-client presses for its own hit areas and can end
        // them there; a press on the bar has to open the menu.
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 == HTMENU => {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // GPUI keeps Alt and F10 from the default procedure, which is what
        // opens the bar from the keyboard.
        WM_SYSKEYDOWN | WM_SYSKEYUP
            if wparam.0 == VK_MENU.0 as usize || wparam.0 == VK_F10.0 as usize =>
        {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            let _ = DefWindowProcW(hwnd, msg, wparam, lparam);
            result
        }
        WM_UAHDRAWMENU if crate::theme::is_dark() => {
            let menu = &*(lparam.0 as *const UahMenu);
            if let Some(mut rect) = menu_bar_rect(hwnd) {
                rect.top -= 1;
                fill(menu.hdc, &rect, Colors::surface_titlebar());
            }
            LRESULT(1)
        }
        WM_UAHDRAWMENUITEM if crate::theme::is_dark() => {
            draw_bar_item(&*(lparam.0 as *const UahDrawMenuItem));
            LRESULT(1)
        }
        WM_NCPAINT | WM_NCACTIVATE => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            if crate::theme::is_dark() {
                paint_bar_underline(hwnd);
            }
            result
        }
        // GPUI puts the frame back to the OS appearance on a colour change;
        // the app theme wins.
        WM_SETTINGCHANGE => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            apply_theme(hwnd);
            result
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
            ATTACHED.with(|attached| {
                attached
                    .borrow_mut()
                    .retain(|attached| *attached != hwnd.0 as isize)
            });
            drop(Box::from_raw(data as *mut Attached));
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}
