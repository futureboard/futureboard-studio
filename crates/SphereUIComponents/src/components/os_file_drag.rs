//! Start an operating-system file drag from the app.
//!
//! The Rigs sidebar is a web page in a native CEF child window: GPUI never sees
//! the mouse there, so a drag that should land on the arrangement cannot be a
//! GPUI drag. Instead the file is downloaded and handed to the platform's own
//! drag-and-drop, and the studio window receives it exactly as it receives a
//! file dragged in from Explorer — through the timeline's `ExternalPaths`
//! drop, which already imports audio and applies `.pst` presets.
//!
//! The call runs a modal drag loop that pumps window messages until the
//! button is released, so it must be made from a foreground task with no GPUI
//! entity or `App` borrow held — the same rule as the CEF pump.

use std::path::Path;

/// How a drag ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutcome {
    /// Dropped on a target that accepted the file.
    Dropped,
    /// Released over nothing that took it, or cancelled with Escape.
    Cancelled,
}

/// Whether this platform can start a file drag from the app.
pub const SUPPORTED: bool = cfg!(windows);

/// Drag one file, starting now, while the mouse button is still held.
///
/// Returns `Err` when the drag could not be started at all (the file is
/// missing, or the shell refused it).
#[cfg(windows)]
pub fn drag_file(path: &Path) -> Result<DragOutcome, String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{IDropSource, DROPEFFECT_COPY, DROPEFFECT_NONE};
    use windows::Win32::UI::Shell::{
        BHID_DataObject, IShellItem, SHCreateItemFromParsingName, SHDoDragDrop,
    };

    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: COM is initialized on the UI thread (OLE drag-and-drop is
    // already in use there for drops onto the studio window). The shell item
    // and its data object are owned COM references released on drop.
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&wide, None)
            .map_err(|error| format!("shell item: {error}"))?;
        // The shell's own data object for the file: CF_HDROP plus the
        // descriptor formats Explorer and other apps understand.
        let data: IDataObject = item
            .BindToHandler(None, &BHID_DataObject)
            .map_err(|error| format!("data object: {error}"))?;
        // No drop source of our own: the shell supplies the standard one,
        // which ends the drag when the left button is released.
        let effect = SHDoDragDrop(None, &data, None::<&IDropSource>, DROPEFFECT_COPY)
            .map_err(|error| format!("drag: {error}"))?;
        Ok(if effect == DROPEFFECT_NONE {
            DragOutcome::Cancelled
        } else {
            DragOutcome::Dropped
        })
    }
}

#[cfg(not(windows))]
pub fn drag_file(_path: &Path) -> Result<DragOutcome, String> {
    Err("dragging a file out of the app is not available on this platform yet".to_string())
}

/// Whether the primary mouse button is down right now, anywhere. A drag can
/// only start while it is; once released, the person has let go and the
/// file should not follow the pointer.
#[cfg(windows)]
pub fn primary_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    // SAFETY: a plain query of global key state.
    (unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000) != 0
}

#[cfg(not(windows))]
pub fn primary_button_down() -> bool {
    false
}
