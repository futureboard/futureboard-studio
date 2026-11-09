//! StudioLayout integration for MIDI file import.
//!
//! The drop itself is handled by the Timeline (it owns the lane coordinates and
//! the edit history). When the parsed file carries optional payload — markers,
//! controller lanes, SysEx — the Timeline hands the decision here instead of
//! importing it all silently; a song file with hundreds of markers otherwise
//! floods the ruler on a single drag.
//!
//! The dialog holds no timeline entity: it gets a plain snapshot of the counts,
//! and on confirm the Timeline imports the parse the drop already made (read
//! once, off the UI thread) at the placement the drop resolved — unless the
//! project has changed since, when nothing is imported (#385).

use std::sync::Arc;

use gpui::Context;

use super::StudioLayout;
use crate::components::midi_import_dialog::{open_midi_import_dialog, MidiImportDialogSetup};
use crate::components::timeline::midi_import::MidiImportOptions;
use crate::components::timeline::timeline::TimelineMidiImportPrompt;

impl StudioLayout {
    /// Ask what a dropped MIDI file should bring in besides its notes.
    ///
    /// A second drop while the dialog is open imports with the defaults rather
    /// than stacking dialogs — the first one still owns the answer for its own
    /// file, and dropping a folder of MIDI should not queue a dialog per file.
    pub(crate) fn open_midi_import_dialog_for_drop(
        &mut self,
        request: &TimelineMidiImportPrompt,
        cx: &mut Context<Self>,
    ) {
        if self.external_windows.import_midi.is_some() {
            self.import_midi_with_options(request, MidiImportOptions::default(), cx);
            return;
        }

        let setup = MidiImportDialogSetup {
            file_name: request.file_name.clone(),
            summary: request.summary,
            options: MidiImportOptions::default(),
        };

        let confirmed = cx.entity().clone();
        let confirmed_request = request.clone();
        let on_import = Arc::new(
            move |options: MidiImportOptions, _window: &mut gpui::Window, cx: &mut gpui::App| {
                let request = confirmed_request.clone();
                let _ = confirmed.update(cx, |layout, cx| {
                    layout.external_windows.import_midi = None;
                    layout.import_midi_with_options(&request, options, cx);
                });
            },
        );
        let closed = cx.entity().clone();
        let on_close = Arc::new(move |_window: &mut gpui::Window, cx: &mut gpui::App| {
            let _ = closed.update(cx, |layout, _cx| {
                layout.external_windows.import_midi = None;
            });
        });

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        match open_midi_import_dialog(owner_bounds, setup, on_import, on_close, cx) {
            Ok(handle) => self.external_windows.import_midi = Some(handle),
            Err(err) => {
                // Never lose the drop to a window that would not open: import
                // it with the defaults, which is what a drop did before the
                // dialog existed.
                eprintln!("[MidiImport] failed to open import dialog: {err}");
                self.import_midi_with_options(request, MidiImportOptions::default(), cx);
            }
        }
    }

    fn import_midi_with_options(
        &mut self,
        request: &TimelineMidiImportPrompt,
        options: MidiImportOptions,
        cx: &mut Context<Self>,
    ) {
        let imported = self.timeline.update(cx, |timeline, cx| {
            timeline.import_midi_tracks_with_options(
                &request.path,
                &request.tracks,
                options,
                &request.placement,
                request.project_generation,
                cx,
            )
        });
        if !imported {
            eprintln!(
                "[MidiImport] nothing imported from {}",
                request.path.display()
            );
            return;
        }
        cx.notify();
    }
}
