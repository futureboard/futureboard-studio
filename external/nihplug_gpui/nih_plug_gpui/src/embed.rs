//! Embedding GPUI windows into the window provided by the plugin host.
//!
//! GPUI has no notion of a child window: [`gpui::App::open_window()`] always creates a regular
//! top level window. Plugin editors on the other hand must live inside a window owned by the host.
//! We bridge that gap after the fact by taking the native window GPUI just created and reparenting
//! it into the host's window using the platform's own APIs. GPUI keeps rendering into and
//! receiving events for the same native window, it simply is no longer a top level window.

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

// See [`crate::host`], GPUI can't be hosted inside a plugin on macOS yet so neither of these is
// ever reached. They exist so the crate still compiles as part of the workspace.
#[cfg(target_os = "macos")]
fn embed_raw(
    _child: RawWindowHandle,
    _parent: ParentHandle,
    _size: (u32, u32),
) -> Result<(), String> {
    Err(String::from(
        "Embedding GPUI windows is not supported on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn resize_raw(_child: RawWindowHandle, _size: (u32, u32)) -> Result<(), String> {
    Err(String::from(
        "Embedding GPUI windows is not supported on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn set_scaling_factor_raw(
    _child: RawWindowHandle,
    _scaling_factor: f32,
    _size: (u32, u32),
) -> Result<(), String> {
    Err(String::from(
        "Embedding GPUI windows is not supported on macOS",
    ))
}
