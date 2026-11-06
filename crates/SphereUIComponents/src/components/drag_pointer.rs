//! The pointer hidden while a fader or send level is dragged.
//!
//! A console control is moved by how far the hand travels, not by where the
//! pointer is (see `fader::drag_to`), so the arrow sitting off to one side of
//! the cap — or running ahead of it on a short fader — only shows the two
//! disagreeing. The pointer is hidden for the drag and put back where the press
//! landed when it ends, the way a hardware-style DAW fader behaves.
//!
//! GPUI's own `hide_cursor_until_mouse_moves` cannot do this: a drag is the
//! mouse moving. On Windows the arrow is hidden with `ShowCursor`, whose
//! display count is per thread; every call here is on the UI thread and the
//! hide is never taken twice, so the count stays balanced. Elsewhere this is a
//! no-op and the pointer shows as before.

#[cfg(windows)]
mod imp {
    use std::cell::Cell;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos, ShowCursor};

    thread_local! {
        /// Where the press landed, while the pointer is hidden.
        static HIDDEN_AT: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    }

    pub fn hide() {
        HIDDEN_AT.with(|hidden| {
            if hidden.get().is_some() {
                return;
            }
            let mut point = POINT::default();
            // SAFETY: plain Win32 calls on the UI thread with a valid out-pointer.
            unsafe {
                let _ = GetCursorPos(&mut point);
                ShowCursor(false);
            }
            hidden.set(Some((point.x, point.y)));
        });
    }

    pub fn show() {
        HIDDEN_AT.with(|hidden| {
            let Some((x, y)) = hidden.take() else {
                return;
            };
            // SAFETY: as above; this undoes the one `ShowCursor(false)`.
            unsafe {
                let _ = SetCursorPos(x, y);
                ShowCursor(true);
            }
        });
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn hide() {}
    pub fn show() {}
}

/// Hide the pointer for a drag that has just started. Taken once: a second
/// call while hidden does nothing.
pub fn hide_for_drag() {
    imp::hide();
}

/// Show the pointer again where the drag started. Does nothing when it is not
/// hidden, so every release may call it.
pub fn show_after_drag() {
    imp::show();
}
