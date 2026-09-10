//! The plug-in editor's chrome strip — the tab strip and control row that sit
//! above a plug-in's own view — on the platforms where the editor window
//! belongs to the plug-in host process.
//!
//! # What this is not
//!
//! Not a second design, and not a second source of truth. On Windows the
//! plug-in's view is embedded in the studio's own window and GPUI draws exactly
//! this strip above it; macOS cannot put a view inside another process's
//! window, so the strip has to be drawn where the window is. This type is the
//! Rust side of that: it carries the studio's already-decided state across, and
//! carries presses back.
//!
//! Nothing here formats or decides. Labels arrive finished (`12%`, `3.2 ms`),
//! colours arrive resolved, and a press goes back out as an integer for the
//! studio to turn into the same action its own chrome would have queued. A
//! second implementation of "what a latency reads as" is a second answer
//! waiting to disagree with the first.
//!
//! # Addressed by window, not by plug-in
//!
//! Every format opens an editor window of its own — VST3 through `IPlugView`,
//! VST2 through `effEditOpen`, CLAP through `clap.gui`, an Audio Unit through
//! its Cocoa view factory — and none of their instance types can name the
//! others'. What they share is the window, so that is what a strip is attached
//! to and that is what addresses it here.
//!
//! [`EditorChrome::for_window`] returns `None` for a handle of 0, which is how
//! a platform with no host-owned window and an instance with no editor open
//! both say so. That is the whole platform check: a caller pushes chrome
//! wherever it has a window and gets nothing where it does not, with no
//! `cfg!(target_os)` of its own.
//!
//! # Threading
//!
//! Main thread only — the thread that owns the window. In the plug-in host
//! process that is the IPC loop, which is also where AppKit is pumped.

use std::ffi::CString;
use std::os::raw::c_char;

use crate::plugin_backend::editor_chrome as ffi;

/// One editor window's chrome strip.
///
/// Cheap to make and not worth storing: it is a window handle and nothing else,
/// and the handle is only valid for as long as that editor is open. Ask for one
/// per update rather than keeping it across them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorChrome {
    window: u64,
}

impl EditorChrome {
    /// The strip in this editor window, or `None` when there is not one.
    ///
    /// `None` for a 0 handle, which covers both "this platform draws its chrome
    /// in the studio's window" and "this instance has no editor open" — the
    /// caller does not have to tell them apart, because there is nothing to
    /// push in either case.
    pub fn for_window(native_window: u64) -> Option<Self> {
        (native_window != 0).then_some(Self {
            window: native_window,
        })
    }

    /// Begin an update, discarding any half-staged one.
    ///
    /// An update is staged across the calls below and becomes visible only at
    /// [`Self::commit`], so a half-sent one never reaches the screen.
    pub fn begin(&self) {
        // SAFETY: a window handle a bridge reported, used on the thread that
        // owns it. The C side treats an unknown window as a no-op.
        unsafe { ffi::begin(self.window) };
    }

    /// The parts of the strip that are not lists.
    ///
    /// `active` is the insert's on/off — bypassed and disabled both read as
    /// `false`, because from the editor's side they say the same thing. The
    /// labels are finished, and `active_tab` is the insert id this window is
    /// showing.
    ///
    /// `shows_controls` is false for an editor with no insert behind it. An ARA
    /// plug-in is bound to a clip rather than to a slot, so it has no bypass, no
    /// per-slot CPU or latency and no insert-keyed preset list; the control row
    /// is dropped and the window gives its height back to the plug-in, because a
    /// row of controls that cannot do what they say is worse than no row.
    pub fn set_header(
        &self,
        active: bool,
        preset_label: &str,
        cpu_label: &str,
        latency_label: &str,
        active_tab: &str,
        shows_controls: bool,
    ) {
        // A label with an interior NUL is not a label the studio produced, so
        // dropping the update beats truncating it into a lie.
        let (Ok(preset), Ok(cpu), Ok(latency), Ok(tab)) = (
            CString::new(preset_label),
            CString::new(cpu_label),
            CString::new(latency_label),
            CString::new(active_tab),
        ) else {
            return;
        };
        // SAFETY: a live window handle and four NUL-terminated strings that
        // outlive the call.
        unsafe {
            ffi::set_header(
                self.window,
                i32::from(active),
                preset.as_ptr(),
                cpu.as_ptr(),
                latency.as_ptr(),
                tab.as_ptr(),
                i32::from(shows_controls),
            );
        }
    }

    /// Append one preset menu row, in menu order.
    pub fn add_preset(&self, name: &str, selected: bool) {
        let Ok(name) = CString::new(name) else {
            return;
        };
        // SAFETY: a live window handle and a NUL-terminated string.
        unsafe { ffi::add_preset(self.window, name.as_ptr(), i32::from(selected)) };
    }

    /// Append one tab, in slot order.
    pub fn add_tab(&self, insert_id: &str, display_name: &str, insert_number: u32) {
        let (Ok(insert_id), Ok(display_name)) =
            (CString::new(insert_id), CString::new(display_name))
        else {
            return;
        };
        // SAFETY: a live window handle and two NUL-terminated strings.
        unsafe {
            ffi::add_tab(
                self.window,
                insert_id.as_ptr(),
                display_name.as_ptr(),
                insert_number as i32,
            );
        }
    }

    /// Resolved theme colours, packed `0xRRGGBBAA`.
    ///
    /// Resolved rather than named because several are composites — a hovered
    /// control is its rest fill lifted by a state layer — and working that out
    /// needs the studio's theme.
    pub fn set_palette(&self, colors: &[u32]) {
        if colors.is_empty() {
            return;
        }
        // SAFETY: a live window handle, and a pointer/length pair taken from one
        // slice that outlives the call.
        unsafe {
            ffi::set_palette(
                self.window,
                colors.as_ptr(),
                colors.len().min(i32::MAX as usize) as i32,
            );
        }
    }

    /// Apply the staged update and repaint, retitling the window with it.
    pub fn commit(&self, window_title: &str) {
        let Ok(title) = CString::new(window_title) else {
            return;
        };
        // SAFETY: a live window handle and a NUL-terminated string.
        unsafe { ffi::commit(self.window, title.as_ptr()) };
    }

    /// Drain one queued press: `(kind, value, insert_id)`.
    ///
    /// `kind` is the wire discriminant the strip queues; the caller maps it back
    /// to the same action the studio's own chrome would have produced.
    /// `insert_id` is empty for every control that is not per-tab. `None` when
    /// the queue is empty.
    pub fn take_action(&self) -> Option<(i32, i32, String)> {
        let mut kind = 0i32;
        let mut value = 0i32;
        // An insert id is a track/slot pair; this is far more than any of them
        // needs, and the C side truncates rather than overruns regardless.
        let mut id = [0u8; 512];
        // SAFETY: a live window handle, two out-pointers to locals, and a buffer
        // whose true capacity is passed alongside it.
        let taken = unsafe {
            ffi::take_action(
                self.window,
                &mut kind,
                &mut value,
                id.as_mut_ptr().cast::<c_char>(),
                id.len() as i32,
            )
        };
        if taken == 0 {
            return None;
        }
        let end = id.iter().position(|b| *b == 0).unwrap_or(id.len());
        Some((
            kind,
            value,
            String::from_utf8_lossy(&id[..end]).into_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole platform check, and the reason no caller needs one of its own.
    #[test]
    fn a_window_that_is_not_there_has_no_chrome() {
        assert_eq!(EditorChrome::for_window(0), None);
        assert!(EditorChrome::for_window(1).is_some());
    }
}
