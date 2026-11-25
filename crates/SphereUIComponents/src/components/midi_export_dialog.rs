//! "Export MIDI File" options dialog for the arrangement.
//!
//! Owns only the choices — the range, what the conductor track carries, and
//! which tracks are written. It holds no timeline entity: the caller passes a
//! plain snapshot of the exportable tracks in, and gets a
//! [`MidiExportOptions`] back through `on_export`, which then builds and saves.
//! That keeps the whole dialog testable and stops it going stale against a
//! project edited while it is open.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, fb_checkbox, fb_section_label, FbButtonKind};
use crate::components::timeline::midi_export::{MidiExportOptions, MidiExportSpan};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::Colors;

pub const MIDI_EXPORT_DIALOG_WIDTH: f32 = 420.0;
pub const MIDI_EXPORT_DIALOG_HEIGHT: f32 = 520.0;

/// One selectable track, captured when the dialog opens.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiExportTrackChoice {
    pub id: String,
    pub name: String,
    pub selected: bool,
}

/// Everything the dialog needs to render, snapshotted by the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiExportDialogSetup {
    pub options: MidiExportOptions,
    pub tracks: Vec<MidiExportTrackChoice>,
    /// `false` disables the loop-range option: there is no loop to export.
    pub has_loop_range: bool,
    /// Shown next to the range choice, e.g. "bars 5–9".
    pub loop_range_label: Option<String>,
}

pub struct MidiExportDialog {
    options: MidiExportOptions,
    tracks: Vec<MidiExportTrackChoice>,
    has_loop_range: bool,
    loop_range_label: Option<String>,
    on_export: Arc<dyn Fn(MidiExportOptions, &mut Window, &mut App) + Send + Sync>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
}

impl MidiExportDialog {
    pub fn new(
        setup: MidiExportDialogSetup,
        on_export: Arc<dyn Fn(MidiExportOptions, &mut Window, &mut App) + Send + Sync>,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    ) -> Self {
        Self {
            options: setup.options,
            tracks: setup.tracks,
            has_loop_range: setup.has_loop_range,
            loop_range_label: setup.loop_range_label,
            on_export,
            on_close,
        }
    }

    /// Options with the track selection folded in.
    ///
    /// All tracks selected sends `None` ("everything") rather than an explicit
    /// list, so a track added after the dialog opened is still exported instead
    /// of being silently dropped by a stale id list.
    pub fn resolved_options(&self) -> MidiExportOptions {
        let mut options = self.options.clone();
        options.track_ids = if self.tracks.iter().all(|track| track.selected) {
            None
        } else {
            Some(
                self.tracks
                    .iter()
                    .filter(|track| track.selected)
                    .map(|track| track.id.clone())
                    .collect(),
            )
        };
        options
    }

    /// Nothing to write: every track deselected, or the project has no MIDI.
    pub fn export_enabled(&self) -> bool {
        !self.tracks.is_empty() && self.tracks.iter().any(|track| track.selected)
    }

    fn set_all_tracks(&mut self, selected: bool) {
        for track in &mut self.tracks {
            track.selected = selected;
        }
    }

    fn include_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // (id, label, hint, getter, setter)
        type Get = fn(&MidiExportOptions) -> bool;
        type Set = fn(&mut MidiExportOptions, bool);
        let rows: Vec<(&'static str, &'static str, &'static str, Get, Set)> = vec![
            (
                "midi-export-tempo",
                "Tempo map",
                "Off follows the receiving DAW's tempo",
                |o| o.include_tempo_map,
                |o, v| o.include_tempo_map = v,
            ),
            (
                "midi-export-timesig",
                "Time signatures",
                "",
                |o| o.include_time_signatures,
                |o, v| o.include_time_signatures = v,
            ),
            (
                "midi-export-markers",
                "Markers",
                "",
                |o| o.include_markers,
                |o, v| o.include_markers = v,
            ),
            (
                "midi-export-controllers",
                "Controller lanes",
                "CC, pitch bend, aftertouch",
                |o| o.include_controllers,
                |o, v| o.include_controllers = v,
            ),
            (
                "midi-export-sysex",
                "SysEx",
                "",
                |o| o.include_sysex,
                |o, v| o.include_sysex = v,
            ),
        ];

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

    fn range_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let loop_label = match (&self.loop_range_label, self.has_loop_range) {
            (Some(label), true) => format!("Loop range ({label})"),
            (_, true) => "Loop range".to_string(),
            _ => "Loop range (none set)".to_string(),
        };
        let whole_entity = cx.entity().clone();
        let loop_entity = cx.entity().clone();
        let has_loop = self.has_loop_range;
        vec![
            // Mutually exclusive, so selecting one clears the other — a radio
            // pair drawn with the shared checkbox control.
            fb_checkbox(
                "midi-export-range-whole",
                "Whole arrangement",
                self.options.span == MidiExportSpan::WholeArrangement,
                true,
                move |_, _window, cx| {
                    let _ = whole_entity.update(cx, |this, cx| {
                        this.options.span = MidiExportSpan::WholeArrangement;
                        cx.notify();
                    });
                },
            )
            .into_any_element(),
            fb_checkbox(
                "midi-export-range-loop",
                loop_label,
                self.options.span == MidiExportSpan::LoopRange && has_loop,
                has_loop,
                move |_, _window, cx| {
                    let _ = loop_entity.update(cx, |this, cx| {
                        this.options.span = MidiExportSpan::LoopRange;
                        cx.notify();
                    });
                },
            )
            .into_any_element(),
        ]
    }

    fn track_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.tracks.is_empty() {
            return vec![div()
                .text_size(px(11.0))
                .text_color(Colors::text_muted())
                .child("This project has no MIDI clips.")
                .into_any_element()];
        }
        self.tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                let entity = cx.entity().clone();
                fb_checkbox(
                    gpui::ElementId::Integer(index as u64),
                    track.name.clone(),
                    track.selected,
                    true,
                    move |_, _window, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            if let Some(track) = this.tracks.get_mut(index) {
                                track.selected = !track.selected;
                            }
                            cx.notify();
                        });
                    },
                )
                .into_any_element()
            })
            .collect()
    }
}

impl Render for MidiExportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let include_rows = self.include_rows(cx);
        let range_rows = self.range_rows(cx);
        let track_rows = self.track_rows(cx);
        let export_enabled = self.export_enabled();

        let on_close = self.on_close.clone();
        let cancel_close = self.on_close.clone();
        let on_export = self.on_export.clone();
        let export_entity = cx.entity().clone();
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
                "Export MIDI File",
                "midi-export-dialog-close",
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .id("midi-export-body-scroll")
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
                            .gap(px(6.0))
                            .child(fb_section_label("Range"))
                            .children(range_rows),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(fb_section_label("Include"))
                            .children(include_rows),
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
                                    .child(fb_section_label("Tracks"))
                                    .child(div().flex_1())
                                    .child(fb_button(
                                        "midi-export-tracks-all",
                                        "All",
                                        FbButtonKind::Default,
                                        true,
                                        move |_, _window, cx| {
                                            let _ = all_entity.update(cx, |this, cx| {
                                                this.set_all_tracks(true);
                                                cx.notify();
                                            });
                                        },
                                    ))
                                    .child(fb_button(
                                        "midi-export-tracks-none",
                                        "None",
                                        FbButtonKind::Default,
                                        true,
                                        move |_, _window, cx| {
                                            let _ = none_entity.update(cx, |this, cx| {
                                                this.set_all_tracks(false);
                                                cx.notify();
                                            });
                                        },
                                    )),
                            )
                            .children(track_rows),
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
                        "midi-export-cancel",
                        "Cancel",
                        FbButtonKind::Default,
                        true,
                        move |_, window, cx| {
                            cancel_close(window, cx);
                            window.remove_window();
                        },
                    ))
                    .child(fb_button(
                        "midi-export-confirm",
                        "Export...",
                        FbButtonKind::Primary,
                        export_enabled,
                        move |_, window, cx| {
                            let options =
                                export_entity.read_with(cx, |this, _| this.resolved_options());
                            // Close first: the save dialog is the next step, and
                            // leaving this window behind it reads as a stuck UI.
                            window.remove_window();
                            on_export(options, window, cx);
                        },
                    )),
            )
    }
}

pub fn open_midi_export_dialog(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    setup: MidiExportDialogSetup,
    on_export: Arc<dyn Fn(MidiExportOptions, &mut Window, &mut App) + Send + Sync>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<MidiExportDialog>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(MIDI_EXPORT_DIALOG_WIDTH), px(MIDI_EXPORT_DIALOG_HEIGHT)),
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
        cx.new(|_cx| MidiExportDialog::new(setup, on_export, on_close))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tracks: &[(&str, bool)]) -> MidiExportDialogSetup {
        MidiExportDialogSetup {
            options: MidiExportOptions::default(),
            tracks: tracks
                .iter()
                .map(|(id, selected)| MidiExportTrackChoice {
                    id: (*id).to_string(),
                    name: (*id).to_string(),
                    selected: *selected,
                })
                .collect(),
            has_loop_range: false,
            loop_range_label: None,
        }
    }

    fn dialog(setup: MidiExportDialogSetup) -> MidiExportDialog {
        MidiExportDialog::new(setup, Arc::new(|_, _, _| {}), Arc::new(|_, _| {}))
    }

    #[test]
    fn all_tracks_selected_exports_everything_rather_than_a_stale_id_list() {
        let d = dialog(setup(&[("a", true), ("b", true)]));
        assert_eq!(d.resolved_options().track_ids, None);
    }

    #[test]
    fn a_partial_selection_names_the_tracks() {
        let d = dialog(setup(&[("a", true), ("b", false)]));
        assert_eq!(d.resolved_options().track_ids, Some(vec!["a".to_string()]));
    }

    #[test]
    fn export_is_disabled_when_nothing_would_be_written() {
        assert!(!dialog(setup(&[])).export_enabled());
        assert!(!dialog(setup(&[("a", false)])).export_enabled());
        assert!(dialog(setup(&[("a", false), ("b", true)])).export_enabled());
    }

    #[test]
    fn select_all_and_none_move_every_track() {
        let mut d = dialog(setup(&[("a", false), ("b", true)]));
        d.set_all_tracks(true);
        assert!(d.tracks.iter().all(|t| t.selected));
        assert_eq!(d.resolved_options().track_ids, None);
        d.set_all_tracks(false);
        assert!(!d.export_enabled());
        assert_eq!(d.resolved_options().track_ids, Some(Vec::new()));
    }
}
