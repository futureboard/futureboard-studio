//! StudioLayout integration for headerless (raw) PCM import.
//!
//! The Timeline parks a dropped `.raw` / `.pcm` file with the placement and
//! project of its drop; this asks the user how the file is laid out, converts
//! it off the UI thread (MKFF decode → 32-bit float WAV in the project's
//! `Assets/Audio`), and hands the WAV back to the Timeline, which imports it
//! like any dropped audio file — unless the project changed meanwhile.
//!
//! Several raw files dropped together are asked about one at a time: each
//! needs its own answer, and a stack of identical dialogs would hide which
//! file each one is for.

use std::sync::Arc;

use gpui::Context;

use super::StudioLayout;
use crate::components::raw_pcm_import_dialog::{
    open_raw_pcm_import_dialog, RawPcmImportDialogSetup,
};
use crate::components::timeline::raw_pcm_import::{
    conversion_folder, convert_raw_pcm, RawPcmSettings,
};
use crate::components::timeline::timeline::TimelineRawPcmImportPrompt;

impl StudioLayout {
    /// Ask how a dropped raw PCM file is laid out, or queue it behind the
    /// dialog that is already open.
    pub(crate) fn open_raw_pcm_import_dialog_for_drop(
        &mut self,
        request: TimelineRawPcmImportPrompt,
        cx: &mut Context<Self>,
    ) {
        if self.external_windows.import_raw_pcm.is_some() {
            self.external_windows
                .queued_raw_pcm_imports
                .push_back(request);
            return;
        }

        let setup = RawPcmImportDialogSetup {
            file_name: request.file_name.clone(),
            file_len: request.file_len,
            settings: self.last_raw_pcm_settings.unwrap_or_default(),
        };

        let confirmed = cx.entity().clone();
        let confirmed_request = request.clone();
        let on_import = Arc::new(
            move |settings: RawPcmSettings, _window: &mut gpui::Window, cx: &mut gpui::App| {
                let request = confirmed_request.clone();
                let _ = confirmed.update(cx, |layout, cx| {
                    layout.external_windows.import_raw_pcm = None;
                    layout.last_raw_pcm_settings = Some(settings);
                    layout.convert_and_import_raw_pcm(request, settings, cx);
                    layout.open_next_queued_raw_pcm_import(cx);
                });
            },
        );
        let closed = cx.entity().clone();
        let on_close = Arc::new(move |_window: &mut gpui::Window, cx: &mut gpui::App| {
            let _ = closed.update(cx, |layout, cx| {
                layout.external_windows.import_raw_pcm = None;
                layout.open_next_queued_raw_pcm_import(cx);
            });
        });

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        match open_raw_pcm_import_dialog(owner_bounds, setup, on_import, on_close, cx) {
            Ok(handle) => self.external_windows.import_raw_pcm = Some(handle),
            Err(err) => {
                // No layout to assume for a headerless file: say so rather
                // than import it with guessed settings.
                eprintln!("[RawPcmImport] failed to open import dialog: {err}");
                let message = format!(
                    "{} was not imported: the Import Raw PCM dialog could not open",
                    request.file_name
                );
                self.timeline.update(cx, |timeline, cx| {
                    timeline.report_raw_pcm_import_failure(message, cx)
                });
            }
        }
    }

    fn open_next_queued_raw_pcm_import(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.external_windows.queued_raw_pcm_imports.pop_front() {
            self.open_raw_pcm_import_dialog_for_drop(next, cx);
        }
    }

    fn convert_and_import_raw_pcm(
        &mut self,
        request: TimelineRawPcmImportPrompt,
        settings: RawPcmSettings,
        cx: &mut Context<Self>,
    ) {
        let folder = conversion_folder(self.project_folder.as_deref());
        let source = request.path.clone();
        let task = cx
            .background_executor()
            .spawn(async move { convert_raw_pcm(&source, settings, &folder) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |layout, cx| {
                layout.timeline.update(cx, |timeline, cx| match result {
                    Ok(wav) => {
                        if !timeline.import_converted_raw_pcm(&request, &wav, cx) {
                            eprintln!(
                                "[RawPcmImport] converted {} but did not import it",
                                wav.display()
                            );
                        }
                    }
                    Err(message) => {
                        eprintln!("[RawPcmImport] {message}");
                        timeline.report_raw_pcm_import_failure(message, cx);
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }
}
