//! "Import MIDI File" options dialog for a dropped `.mid` / `.midi` file.
//!
//! A song file routinely carries far more than notes — hundreds of markers, a
//! dense CC / pitch-bend stream, SysEx dumps — and importing all of it unasked
//! buries the arrangement. This dialog names how much of each the file holds
//! and returns the choice through `on_import`; the notes themselves are never
//! optional, so there is no way to end up importing nothing.
//!
//! Like the export dialog it owns only the choices: the caller passes a plain
//! snapshot in and re-reads the file when the user confirms, so nothing is held
//! against a project edited while the dialog is open.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, fb_checkbox, fb_section_label, FbButtonKind};
use crate::components::timeline::midi_import::{MidiImportOptions, MidiImportSummary};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::Colors;

pub const MIDI_IMPORT_DIALOG_WIDTH: f32 = 420.0;
pub const MIDI_IMPORT_DIALOG_HEIGHT: f32 = 380.0;

/// Everything the dialog renders from, snapshotted when the drop was parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiImportDialogSetup {
    /// File stem, shown as the subject of the import.
    pub file_name: String,
    pub summary: MidiImportSummary,
    pub options: MidiImportOptions,
}

pub struct MidiImportDialog {
    file_name: String,
    summary: MidiImportSummary,
    options: MidiImportOptions,
    on_import: Arc<dyn Fn(MidiImportOptions, &mut Window, &mut App) + Send + Sync>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
}

impl MidiImportDialog {
    pub fn new(
        setup: MidiImportDialogSetup,
        on_import: Arc<dyn Fn(MidiImportOptions, &mut Window, &mut App) + Send + Sync>,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    ) -> Self {
        Self {
            file_name: setup.file_name,
            summary: setup.summary,
            options: setup.options,
            on_import,
            on_close,
        }
    }

    pub fn options(&self) -> MidiImportOptions {
        self.options
    }

    /// What the file holds, in one line under the title.
    pub fn contents_line(&self) -> String {
        let mut parts = vec![count_label(self.summary.notes, "note")];
        if self.summary.tracks > 1 {
            parts.push(count_label(self.summary.tracks, "track"));
        }
        parts.join(" · ")
    }

    /// (id, label, hint, getter, setter) for every optional payload the parsed
    /// file actually carries. A row is omitted rather than shown at zero, so
    /// the dialog only ever asks about real choices.
    fn include_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        type Get = fn(&MidiImportOptions) -> bool;
        type Set = fn(&mut MidiImportOptions, bool);
        let mut rows: Vec<(&'static str, String, String, Get, Set)> = Vec::new();

        if self.summary.markers > 0 {
            rows.push((
                "midi-import-markers",
                format!("Markers ({})", self.summary.markers),
                "Added to the arrangement ruler".to_string(),
                |o| o.include_markers,
                |o, v| o.include_markers = v,
            ));
        }
        if self.summary.controller_lanes > 0 {
            rows.push((
                "midi-import-controllers",
                format!(
                    "Controller lanes ({})",
                    count_label(self.summary.controller_lanes, "lane")
                ),
                format!(
                    "CC, pitch bend, aftertouch — {}",
                    count_label(self.summary.controller_points, "point")
                ),
                |o| o.include_controllers,
                |o, v| o.include_controllers = v,
            ));
        }
        if self.summary.sysex_events > 0 {
            rows.push((
                "midi-import-sysex",
                format!("SysEx ({})", self.summary.sysex_events),
                "Device-specific data, kept with the clip".to_string(),
                |o| o.include_sysex,
                |o, v| o.include_sysex = v,
            ));
        }

        rows.into_iter()
            .map(|(id, label, hint, get, set)| {
                let entity = cx.entity().clone();
                let checked = get(&self.options);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(fb_checkbox(
                        id,
                        label,
                        checked,
                        true,
                        move |_, _window, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                let next = !get(&this.options);
                                set(&mut this.options, next);
                                cx.notify();
                            });
                        },
                    ))
                    .when(!hint.is_empty(), |this| {
                        this.child(
                            div()
                                .pl(px(22.0))
                                .text_size(px(10.0))
                                .text_color(Colors::text_muted())
                                .child(hint),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for MidiImportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let include_rows = self.include_rows(cx);
        let contents_line = self.contents_line();
        let file_name = self.file_name.clone();

        let on_close = self.on_close.clone();
        let cancel_close = self.on_close.clone();
        let on_import = self.on_import.clone();
        let import_entity = cx.entity().clone();
        let all_entity = cx.entity().clone();
        let none_entity = cx.entity().clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .child(external_window_titlebar(
                "Import MIDI File",
                "midi-import-dialog-close",
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .id("midi-import-body-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(14.0))
                    .p(px(16.0))
                    .overflow_y_scroll()
                    .bg(Colors::surface_base())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::text_primary())
                                    .truncate()
                                    .child(file_name),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(Colors::text_muted())
                                    .child(contents_line),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(fb_section_label("Also import"))
                                    .child(div().flex_1())
                                    .child(fb_button(
                                        "midi-import-all",
                                        "All",
                                        FbButtonKind::Default,
                                        true,
                                        move |_, _window, cx| {
                                            let _ = all_entity.update(cx, |this, cx| {
                                                this.options = MidiImportOptions::default();
                                                cx.notify();
                                            });
                                        },
                                    ))
                                    .child(fb_button(
                                        "midi-import-none",
                                        "None",
                                        FbButtonKind::Default,
                                        true,
                                        move |_, _window, cx| {
                                            let _ = none_entity.update(cx, |this, cx| {
                                                this.options = MidiImportOptions::NOTES_ONLY;
                                                cx.notify();
                                            });
                                        },
                                    )),
                            )
                            .children(include_rows),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .line_height(px(15.0))
                            .text_color(Colors::text_faint())
                            .child("Notes are always imported."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(16.0))
                    .py(px(10.0))
                    .border_t(px(1.0))
                    .border_color(Colors::panel_border())
                    .bg(Colors::surface_panel())
                    .child(div().flex_1())
                    .child(fb_button(
                        "midi-import-cancel",
                        "Cancel",
                        FbButtonKind::Default,
                        true,
                        move |_, window, cx| {
                            cancel_close(window, cx);
                            window.remove_window();
                        },
                    ))
                    .child(fb_button(
                        "midi-import-confirm",
                        "Import",
                        FbButtonKind::Primary,
                        true,
                        move |_, window, cx| {
                            let options = import_entity.read_with(cx, |this, _| this.options);
                            window.remove_window();
                            on_import(options, window, cx);
                        },
                    )),
            )
    }
}

/// "1 marker" / "214 markers" — the count and its unit, pluralized.
fn count_label(count: usize, unit: &str) -> String {
    if count == 1 {
        format!("{count} {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

pub fn open_midi_import_dialog(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    setup: MidiImportDialogSetup,
    on_import: Arc<dyn Fn(MidiImportOptions, &mut Window, &mut App) + Send + Sync>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<MidiImportDialog>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(MIDI_IMPORT_DIALOG_WIDTH), px(MIDI_IMPORT_DIALOG_HEIGHT)),
        cx,
    );

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|_cx| MidiImportDialog::new(setup, on_import, on_close))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(summary: MidiImportSummary) -> MidiImportDialog {
        MidiImportDialog::new(
            MidiImportDialogSetup {
                file_name: "Song".to_string(),
                summary,
                options: MidiImportOptions::default(),
            },
            Arc::new(|_, _, _| {}),
            Arc::new(|_, _| {}),
        )
    }

    #[test]
    fn none_keeps_the_notes_and_drops_every_extra() {
        let mut d = dialog(MidiImportSummary {
            markers: 214,
            controller_lanes: 3,
            sysex_events: 1,
            ..MidiImportSummary::default()
        });
        d.options = MidiImportOptions::NOTES_ONLY;
        let options = d.options();
        assert!(!options.include_markers);
        assert!(!options.include_controllers);
        assert!(!options.include_sysex);
    }

    #[test]
    fn the_contents_line_names_tracks_only_when_there_is_more_than_one() {
        let single = dialog(MidiImportSummary {
            tracks: 1,
            notes: 1,
            ..MidiImportSummary::default()
        });
        assert_eq!(single.contents_line(), "1 note");

        let multi = dialog(MidiImportSummary {
            tracks: 4,
            notes: 128,
            ..MidiImportSummary::default()
        });
        assert_eq!(multi.contents_line(), "128 notes · 4 tracks");
    }
}
