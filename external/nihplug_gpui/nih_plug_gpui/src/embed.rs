//! Embedding GPUI windows into the window provided by the plugin host.
//!
//! GPUI has no notion of a child window: [`gpui::App::open_window()`] always creates a regular
//! top level window. Plugin editors on the other hand must live inside a window owned by the host.
//! We bridge that gap after the fact by taking the native window GPUI just created and reparenting
//! it into the host's window using the platform's own APIs. GPUI keeps rendering into and
//! receiving events for the same native window, it simply is no longer a top level window.
//!
//! On macOS the unit that moves is the window's `NSView`: it is taken out of GPUI's own (never
//! shown) window and added to the host's view, through GPUI's embedding support in `gpui_macos`.
//! Sizes there are in points, which is what both the host and GPUI's geometry use, so no scaling
//! factor is ever applied by hand.

use gpui::Window;
use nih_plug::prelude::ParentWindowHandle;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// A [`ParentWindowHandle`] reduced to a plain integer so it can be sent to the GPUI host thread.
/// `ParentWindowHandle` itself contains raw pointers and is therefore neither `Send` nor `Sync`.
// Only the variant for the platform being compiled for is ever read
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ParentHandle {
    X11Window(u32),
    AppKitNsView(usize),
    Win32Hwnd(isize),
}

impl From<ParentWindowHandle> for ParentHandle {
    fn from(handle: ParentWindowHandle) -> Self {
        match handle {
            ParentWindowHandle::X11Window(window) => ParentHandle::X11Window(window),
            ParentWindowHandle::AppKitNsView(ns_view) => {
                ParentHandle::AppKitNsView(ns_view as usize)
            }
            ParentWindowHandle::Win32Hwnd(hwnd) => ParentHandle::Win32Hwnd(hwnd as isize),
        }
    }
}

/// Reparent `window`'s native window into `parent` and resize it to `size` physical pixels.
///
/// This is called right after the window has been created and before it is shown.
pub(crate) fn embed(window: &Window, parent: ParentHandle, size: (u32, u32)) -> Result<(), String> {
    // Disambiguated because `Window` also has an inherent `window_handle()` returning GPUI's own
    // window handle
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|err| format!("GPUI did not provide a window handle: {err}"))?;

    embed_raw(handle.as_raw(), parent, size)
}

/// Resize `window`'s native window to `size` physical pixels. GPUI's own resizing works in terms of
/// top level window geometry, which the window manager ignores for an embedded window, so this goes
/// through the platform APIs as well.
pub(crate) fn resize(window: &Window, size: (u32, u32)) -> Result<(), String> {
    // Disambiguated because `Window` also has an inherent `window_handle()` returning GPUI's own
    // window handle
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|err| format!("GPUI did not provide a window handle: {err}"))?;

    resize_raw(handle.as_raw(), size)
}

/// Make GPUI draw `window` at `scaling_factor` instead of at the scaling factor it derived from the
/// display it created the window on, resizing the window to `size` physical pixels in the process.
///
/// The host is the one deciding how large the editor's window is: it takes the editor's size in
/// logical pixels and multiplies it by the scaling factor it reported to the plugin. The window
/// GPUI draws into is only the size the plugin asked for if GPUI applies that same scaling factor,
/// which it does not necessarily do. It picks up the scaling factor of the display its window was
/// created on, and since an embedded window never becomes a top level window again the platform
/// won't tell it about the display it actually ends up on either.
///
/// This is called before the window is embedded, while it is still a regular top level window.
#[cfg(not(target_os = "macos"))]
pub(crate) fn set_scaling_factor(
    window: &Window,
    scaling_factor: f32,
    size: (u32, u32),
) -> Result<(), String> {
    // Disambiguated because `Window` also has an inherent `window_handle()` returning GPUI's own
    // window handle
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|err| format!("GPUI did not provide a window handle: {err}"))?;

    // GPUI only learns a new scale from `WM_DPICHANGED`. When the scale is already the one the
    // host asked for, the window can still be the wrong number of physical pixels (it was created
    // at 1x, or a panel opened). Resize it either way.
    if (window.scale_factor() - scaling_factor).abs() > 0.001 {
        set_scaling_factor_raw(handle.as_raw(), scaling_factor, size)
    } else {
        resize_raw(handle.as_raw(), size)
    }
}

/// AppKit derives the backing scale from the screen the host's window is on, and GPUI follows it.
/// Sizes are in points, which are the same at every scale, so there is nothing to apply.
#[cfg(target_os = "macos")]
pub(crate) fn set_scaling_factor(
    _window: &Window,
    _scaling_factor: f32,
    _size: (u32, u32),
) -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn embed_raw(child: RawWindowHandle, parent: ParentHandle, size: (u32, u32)) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt};

    let child = match child {
        RawWindowHandle::Xcb(handle) => handle.window.get(),
        // Wayland has no equivalent of X11's window reparenting. Hosts hand out an X11 window here,
        // so plugins are expected to use XWayland.
        other => {
            return Err(format!(
                "Unsupported GPUI window handle for embedding: {other:?}"
            ))
        }
    };
    let parent = match parent {
        ParentHandle::X11Window(window) => window,
        other => {
            return Err(format!(
                "Unsupported parent window handle on X11: {other:?}"
            ))
        }
    };

    // A second connection to the same display is fine here. Reparenting is a regular X11 request
    // and doesn't need to originate from the connection that created the window.
    let (connection, _screen) =
        x11rb::connect(None).map_err(|err| format!("Could not connect to the X server: {err}"))?;

    // GPUI always maps its windows on creation, so unmap first. While the window is a direct child
    // of the root window it is managed by the window manager, and unmapping it makes the window
    // manager let go of it before we move it into the host's window.
    connection
        .unmap_window(child)
        .map_err(|err| format!("X11 UnmapWindow failed: {err}"))?;
    connection
        .reparent_window(child, parent, 0, 0)
        .map_err(|err| format!("X11 ReparentWindow failed: {err}"))?;
    connection
        .configure_window(
            child,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(size.0)
                .height(size.1),
        )
        .map_err(|err| format!("X11 ConfigureWindow failed: {err}"))?;
    connection
        .map_window(child)
        .map_err(|err| format!("X11 MapWindow failed: {err}"))?;
    connection
        .flush()
        .map_err(|err| format!("Could not flush the X11 connection: {err}"))?;

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn resize_raw(child: RawWindowHandle, size: (u32, u32)) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt};

    let child = match child {
        RawWindowHandle::Xcb(handle) => handle.window.get(),
        other => {
            return Err(format!(
                "Unsupported GPUI window handle for resizing: {other:?}"
            ))
        }
    };

    let (connection, _screen) =
        x11rb::connect(None).map_err(|err| format!("Could not connect to the X server: {err}"))?;
    connection
        .configure_window(
            child,
            &ConfigureWindowAux::new().width(size.0).height(size.1),
        )
        .map_err(|err| format!("X11 ConfigureWindow failed: {err}"))?;
    connection
        .flush()
        .map_err(|err| format!("Could not flush the X11 connection: {err}"))?;

    Ok(())
}

/// GPUI's X11 backend keeps a single scaling factor for the entire application, taken from the X
/// resource database. There is no per window equivalent of the message used on Windows, so the
/// window is left at whatever GPUI computed. Hosts that report a different scaling factor than the
/// one X11 advertises will end up with a differently sized GUI.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn set_scaling_factor_raw(
    _child: RawWindowHandle,
    _scaling_factor: f32,
    _size: (u32, u32),
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "windows")]
fn embed_raw(child: RawWindowHandle, parent: ParentHandle, size: (u32, u32)) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetParent, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, HWND_TOP, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_SHOWWINDOW, WS_CHILD, WS_VISIBLE,
    };

    let child = match child {
        RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut std::ffi::c_void),
        other => {
            return Err(format!(
                "Unsupported GPUI window handle for embedding: {other:?}"
            ))
        }
    };
    let parent = match parent {
        ParentHandle::Win32Hwnd(hwnd) => HWND(hwnd as *mut std::ffi::c_void),
        other => {
            return Err(format!(
                "Unsupported parent window handle on Windows: {other:?}"
            ))
        }
    };

    unsafe {
        // A window can only be a child window if it has the `WS_CHILD` style, and it must lose the
        // top level window styles it was created with or it will keep drawing a title bar and a
        // border inside of the host's window.
        SetWindowLongPtrW(child, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
        SetParent(child, Some(parent)).map_err(|err| format!("Win32 SetParent failed: {err}"))?;
        SetWindowPos(
            child,
            Some(HWND_TOP),
            0,
            0,
            size.0 as i32,
            size.1 as i32,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        .map_err(|err| format!("Win32 SetWindowPos failed: {err}"))?;
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn resize_raw(child: RawWindowHandle, size: (u32, u32)) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    };

    let child = match child {
        RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut std::ffi::c_void),
        other => {
            return Err(format!(
                "Unsupported GPUI window handle for resizing: {other:?}"
            ))
        }
    };

    unsafe {
        SetWindowPos(
            child,
            None,
            0,
            0,
            size.0 as i32,
            size.1 as i32,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER,
        )
        .map_err(|err| format!("Win32 SetWindowPos failed: {err}"))?;
    }

    Ok(())
}

/// Give `window` the keyboard, or hand it back to the window it is embedded in.
///
/// Windows sends key and IME messages to the one window that has keyboard focus. A top level window
/// gets it by being activated, which is how GPUI's backend expects to receive it, but an embedded
/// window is a child window and child windows are never activated: clicking one leaves the focus
/// with the host. GPUI then shows a focused text field the keyboard never reaches. The editor asks
/// for the keyboard while it has a text field to type into, and gives it back afterwards so the
/// host's own shortcuts work again.
#[cfg(target_os = "windows")]
pub(crate) fn set_keyboard_focus(window: &Window, focused: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
    use windows::Win32::UI::WindowsAndMessaging::GetParent;

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let child = HWND(handle.hwnd.get() as *mut std::ffi::c_void);

    unsafe {
        if focused {
            if GetFocus() != child {
                let _ = SetFocus(Some(child));
            }
        } else if GetFocus() == child {
            // A standalone editor is a top level window with no parent to hand the keyboard to
            if let Ok(parent) = GetParent(child) {
                if !parent.is_invalid() {
                    let _ = SetFocus(Some(parent));
                }
            }
        }
    }
}

/// On X11 and macOS the host's window system already routes keys to the embedded editor.
#[cfg(not(target_os = "windows"))]
pub(crate) fn set_keyboard_focus(_window: &Window, _focused: bool) {}

/// GPUI's Windows backend keeps track of the scaling factor for its window itself, and it only ever
/// changes it in response to a `WM_DPICHANGED` message. Windows sends that message to top level
/// windows only, so this synthesizes one. Aside from the scaling factor the message also carries the
/// window's new size, which GPUI applies for us.
#[cfg(target_os = "windows")]
fn set_scaling_factor_raw(
    child: RawWindowHandle,
    scaling_factor: f32,
    size: (u32, u32),
) -> Result<(), String> {
    use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageW, USER_DEFAULT_SCREEN_DPI, WM_DPICHANGED,
    };

    let child = match child {
        RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut std::ffi::c_void),
        other => {
            return Err(format!(
                "Unsupported GPUI window handle for scaling: {other:?}"
            ))
        }
    };

    // The message contains the horizontal DPI in its low word and the vertical DPI in its high word.
    // Plugin hosts deal in scaling factors, and only in uniform ones.
    let dpi = (scaling_factor * USER_DEFAULT_SCREEN_DPI as f32).round() as usize;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: size.0 as i32,
        bottom: size.1 as i32,
    };

    unsafe {
        SendMessageW(
            child,
            WM_DPICHANGED,
            Some(WPARAM(dpi | (dpi << 16))),
            Some(LPARAM(&mut rect as *mut RECT as isize)),
        );
    }

    Ok(())
}

/// The `NSView` GPUI draws `child` into.
#[cfg(target_os = "macos")]
fn ns_view(child: RawWindowHandle) -> Result<*mut std::ffi::c_void, String> {
    match child {
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr()),
        other => Err(format!(
            "Unsupported GPUI window handle on macOS: {other:?}"
        )),
    }
}

#[cfg(target_os = "macos")]
fn embed_raw(child: RawWindowHandle, parent: ParentHandle, size: (u32, u32)) -> Result<(), String> {
    let view = ns_view(child)?;
    let parent = match parent {
        ParentHandle::AppKitNsView(ns_view) => ns_view as *mut std::ffi::c_void,
        other => {
            return Err(format!(
                "Unsupported parent window handle on macOS: {other:?}"
            ))
        }
    };

    // Runs on the main thread: the GPUI application lives there on macOS
    unsafe { gpui_macos::embed_in_parent_view(view, parent, size.0 as f64, size.1 as f64) }
}

#[cfg(target_os = "macos")]
fn resize_raw(child: RawWindowHandle, size: (u32, u32)) -> Result<(), String> {
    let view = ns_view(child)?;

    unsafe { gpui_macos::resize_embedded_view(view, size.0 as f64, size.1 as f64) }
}

/// The view an embedded `window` draws into, as an integer so it can be kept by the editor's
/// handle for [`detach_view()`].
#[cfg(target_os = "macos")]
pub(crate) fn view_of(window: &Window) -> Option<usize> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    ns_view(handle.as_raw()).ok().map(|view| view as usize)
}

/// Take an embedded view out of the host's view right now. See
/// [`gpui_macos::detach_embedded_view()`].
#[cfg(target_os = "macos")]
pub(crate) fn detach_view(view: usize) {
    unsafe { gpui_macos::detach_embedded_view(view as *mut std::ffi::c_void) }
}

/// The HWND an embedded `window` draws into, as an integer so it can be kept by the editor's
/// handle for [`detach_hwnd()`].
#[cfg(target_os = "windows")]
pub(crate) fn hwnd_of(window: &Window) -> Option<usize> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
        _ => None,
    }
}

/// Detach the child HWND from the host's parent window before teardown.
#[cfg(target_os = "windows")]
pub(crate) fn detach_hwnd(child: usize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindow, SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_STYLE,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, WS_POPUP,
    };

    if child == 0 {
        return;
    }
    let child = HWND(child as *mut std::ffi::c_void);
    unsafe {
        if !IsWindow(Some(child)).as_bool() {
            return;
        }
        let _ = ShowWindow(child, SW_HIDE);
        let _ = SetParent(child, None);
        let _ = SetWindowLongPtrW(child, GWL_STYLE, WS_POPUP.0 as isize);
        let _ = SetWindowPos(
            child,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetParent, RegisterClassW, WNDCLASSW,
        WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    unsafe extern "system" fn test_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    #[test]
    fn detach_hwnd_clears_parent() {
        unsafe {
            let class_name = w!("NihPlugGpuiDetachTest");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(test_wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);

            // Create a parent window
            let parent = CreateWindowExW(
                Default::default(),
                class_name,
                w!("Parent"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
            .expect("failed to create parent window");

            // Create a child window
            let child = CreateWindowExW(
                Default::default(),
                class_name,
                w!("Child"),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                50,
                50,
                Some(parent),
                None,
                None,
                None,
            )
            .expect("failed to create child window");

            assert_eq!(GetParent(child).unwrap(), parent);

            // Detach child
            detach_hwnd(child.0 as usize);

            // Child's parent should now be null/desktop
            assert!(GetParent(child).is_err() || GetParent(child).unwrap().0 as usize == 0);

            // Parent can be destroyed without destroying the detached child
            let _ = DestroyWindow(parent);
            let _ = DestroyWindow(child);
        }
    }
}
