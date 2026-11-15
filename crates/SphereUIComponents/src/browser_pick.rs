//! "Browse…" in a plug-in or the Sampler Rack picks the file in the Browser
//! sidebar, not in a system file dialog.
//!
//! The button posts one request: what it is for, which file types it takes,
//! and how to hand the file over. The studio shows and focuses the Browser
//! with a bar naming the request; double-clicking (or Enter on) a matching
//! file there delivers it, and Escape or Cancel drops it. One request at a
//! time: a second Browse replaces the first.
//!
//! Plug-in editors are windows of their own, and a Browser drag cannot leave
//! the studio window, so picking in the Browser and delivering by callback
//! is what lets every one of them take a file from it.

use std::path::{Path, PathBuf};

use gpui::{App, Global};

/// How the file reaches whoever asked. Holds a weak handle at most, so a
/// window closed since simply never gets it.
pub type Deliver = Box<dyn FnOnce(PathBuf, &mut App)>;

/// The one pending request.
pub struct BrowserPick {
    /// What the pick is for, shown in the Browser's bar: "Drum Sampler · Pad 3".
    pub label: String,
    /// Lower-case extensions it takes.
    pub extensions: Vec<&'static str>,
    deliver: Deliver,
}

impl BrowserPick {
    /// Whether `path` is a file this request takes.
    pub fn accepts(&self, path: &Path) -> bool {
        accepts(&self.extensions, path)
    }

    /// Whether the files it takes are audio (the Browser opens its Samples
    /// tab for those, its Files tab otherwise).
    pub fn wants_audio(&self) -> bool {
        self.extensions
            .iter()
            .any(|ext| matches!(*ext, "wav" | "flac" | "mp3" | "ogg" | "aif" | "aiff"))
    }
}

fn accepts(extensions: &[&str], path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| extensions.contains(&ext.as_str()))
}

/// The request slot, a global so every window can post to it and the studio
/// can watch it.
#[derive(Default)]
pub struct PendingBrowserPick(pub Option<BrowserPick>);

impl Global for PendingBrowserPick {}

/// Ask the Browser for a file. Replaces any request still pending.
pub fn request(
    cx: &mut App,
    label: impl Into<String>,
    extensions: &[&'static str],
    deliver: impl FnOnce(PathBuf, &mut App) + 'static,
) {
    cx.set_global(PendingBrowserPick(Some(BrowserPick {
        label: label.into(),
        extensions: extensions.to_vec(),
        deliver: Box::new(deliver),
    })));
}

/// The pending request's label and whether it wants audio, if there is one.
pub fn pending(cx: &App) -> Option<(String, bool)> {
    cx.try_global::<PendingBrowserPick>()
        .and_then(|pending| pending.0.as_ref())
        .map(|pick| (pick.label.clone(), pick.wants_audio()))
}

/// Drop the pending request, if any. Returns whether there was one.
pub fn cancel(cx: &mut App) -> bool {
    if pending(cx).is_none() {
        return false;
    }
    cx.set_global(PendingBrowserPick(None));
    true
}

/// What a Browser activation (double-click, Enter) does while a request may
/// be pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// No request: the Browser does what it always does.
    Normal,
    /// The file went to the request, which is done.
    Delivered,
    /// A request is pending and this file is not one it takes: nothing
    /// happens (the Browser must not import it while you are picking).
    Ignored,
}

/// Offer an activated file to the pending request.
pub fn activate(cx: &mut App, path: &Path) -> Activation {
    let takes = match cx
        .try_global::<PendingBrowserPick>()
        .and_then(|p| p.0.as_ref())
    {
        None => return Activation::Normal,
        Some(pick) => pick.accepts(path),
    };
    if !takes {
        return Activation::Ignored;
    }
    let pick = cx.global_mut::<PendingBrowserPick>().0.take();
    // `set_global` again so observers hear the slot emptied.
    cx.set_global(PendingBrowserPick(None));
    if let Some(pick) = pick {
        // Deferred: Enter arrives inside the studio's own update, and a
        // delivery may update the studio (the Sampler Rack's does).
        let path = path.to_path_buf();
        cx.defer(move |cx| (pick.deliver)(path, cx));
        Activation::Delivered
    } else {
        Activation::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[gpui::test]
    fn a_pick_takes_one_matching_file_and_is_done(cx: &mut gpui::TestAppContext) {
        let got: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        cx.update(|cx| {
            let sink = got.clone();
            request(
                cx,
                "Drum Sampler · Pad 3",
                &["wav", "flac"],
                move |path, _| {
                    sink.borrow_mut().push(path);
                },
            );
            assert_eq!(
                pending(cx),
                Some(("Drum Sampler · Pad 3".to_string(), true))
            );
            // Not a file it takes: nothing happens, the request stays.
            assert_eq!(
                activate(cx, Path::new("C:/a/readme.txt")),
                Activation::Ignored
            );
            assert!(pending(cx).is_some());
            assert_eq!(
                activate(cx, Path::new("C:/a/Kick.WAV")),
                Activation::Delivered
            );
            assert!(pending(cx).is_none());
            // Done: the next activation is the Browser's own again.
            assert_eq!(
                activate(cx, Path::new("C:/a/snare.wav")),
                Activation::Normal
            );
        });
        assert_eq!(*got.borrow(), vec![PathBuf::from("C:/a/Kick.WAV")]);
    }

    #[gpui::test]
    fn cancel_and_replace(cx: &mut gpui::TestAppContext) {
        let got: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        cx.update(|cx| {
            let first = got.clone();
            request(cx, "Quick Sampler", &["wav"], move |_, _| {
                first.borrow_mut().push("first")
            });
            let second = got.clone();
            request(cx, "Rodhareist · Amp", &["nam"], move |_, _| {
                second.borrow_mut().push("second")
            });
            assert_eq!(pending(cx), Some(("Rodhareist · Amp".to_string(), false)));
            assert_eq!(activate(cx, Path::new("amp.nam")), Activation::Delivered);
            request(cx, "Slicer", &["wav"], |_, _| {});
            assert!(cancel(cx));
            assert!(!cancel(cx));
            assert_eq!(activate(cx, Path::new("x.wav")), Activation::Normal);
        });
        assert_eq!(*got.borrow(), vec!["second"]);
    }
}
