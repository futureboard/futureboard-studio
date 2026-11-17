//! "Import Raw PCM" dialog for a dropped `.raw` / `.pcm` file.
//!
//! A headerless file says nothing about itself, so the user picks its sample
//! rate, channel count, sample format and byte order. The dialog shows what
//! those settings make of the file — its duration and any bytes left over —
//! so a wrong guess (stereo read as mono, 16-bit read as 8-bit) is visible
//! before anything is imported. Like the MIDI import dialog it owns only the
//! choice and returns it through `on_import`; the caller converts the file.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{
    fb_button, fb_form_row, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::timeline::raw_pcm_import::{
    format_duration, RawPcmSettings, RawSampleFormat, RAW_CHANNEL_COUNTS, RAW_SAMPLE_RATES,
};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::Colors;

pub const RAW_PCM_IMPORT_DIALOG_WIDTH: f32 = 640.0;
pub const RAW_PCM_IMPORT_DIALOG_HEIGHT: f32 = 350.0;

/// Everything the dialog renders from.
#[derive(Debug, Clone, PartialEq)]
pub struct RawPcmImportDialogSetup {
    /// File name, shown as the subject of the import.
    pub file_name: String,
    /// File size in bytes (what the duration line is computed from).
    pub file_len: u64,
    pub settings: RawPcmSettings,
}

type OnImport = Arc<dyn Fn(RawPcmSettings, &mut Window, &mut App) + Send + Sync>;
type OnClose = Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>;

pub struct RawPcmImportDialog {
    file_name: String,
    file_len: u64,
    settings: RawPcmSettings,
    on_import: OnImport,
    on_close: OnClose,
}

fn position(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (i, n) if i + 1 == n => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

fn rate_label(rate: u32) -> String {
    if rate % 1000 == 0 {
        format!("{}k", rate / 1000)
    } else {
        format!("{}k", rate as f64 / 1000.0)
    }
}

fn channel_label(channels: u32) -> String {
    match channels {
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        n => n.to_string(),
    }
}

impl RawPcmImportDialog {
    pub fn new(setup: RawPcmImportDialogSetup, on_import: OnImport, on_close: OnClose) -> Self {
        Self {
            file_name: setup.file_name,
            file_len: setup.file_len,
            settings: setup.settings,
            on_import,
            on_close,
        }
    }

    pub fn settings(&self) -> RawPcmSettings {
        self.settings
    }

    /// What the current settings make of the file: duration and frame count,
    /// plus any trailing bytes that are not a whole frame.
    pub fn summary_line(&self) -> String {
        let (frames, trailing) = self.settings.frames_in(self.file_len);
        let mut line = format!(
            "{} · {frames} frames",
            format_duration(self.settings.duration_seconds(self.file_len))
        );
        if trailing > 0 {
            line.push_str(&format!(
                " · {trailing} trailing byte{} ignored",
                if trailing == 1 { "" } else { "s" }
            ));
        }
        line
    }

    fn can_import(&self) -> bool {
        self.settings.frames_in(self.file_len).0 > 0
    }

    /// One segmented row: `options` labelled by `label_of`, the active one
    /// matched by `is_active`, a click applying `apply`.
    fn segmented_row<T: Copy + 'static>(
        id: &'static str,
        options: &[T],
        label_of: impl Fn(T) -> String,
        is_active: impl Fn(T) -> bool,
        apply: fn(&mut RawPcmSettings, T),
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let count = options.len();
        fb_segmented_track().children(options.iter().enumerate().map(|(i, &value)| {
            let entity = cx.entity().clone();
            div().flex_1().min_w_0().child(fb_segment(
                (id, i),
                label_of(value),
                is_active(value),
                position(i, count),
                move |_, _window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        apply(&mut this.settings, value);
                        cx.notify();
                    });
                },
            ))
        }))
    }
}

impl Render for RawPcmImportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings;
        let rates = Self::segmented_row(
            "raw-pcm-rate",
            &RAW_SAMPLE_RATES,
            rate_label,
            |r| r == settings.sample_rate,
            |s, r| s.sample_rate = r,
            cx,
        );
        let channels = Self::segmented_row(
            "raw-pcm-channels",
            &RAW_CHANNEL_COUNTS,
            channel_label,
            |c| c == settings.channels,
            |s, c| s.channels = c,
            cx,
        );
        let formats = Self::segmented_row(
            "raw-pcm-format",
            &RawSampleFormat::ALL,
            |f| f.label().to_string(),
            |f| f == settings.format,
            |s, f| s.format = f,
            cx,
        );
        let has_byte_order = settings.format.has_byte_order();
        let byte_order = Self::segmented_row(
            "raw-pcm-endian",
            &[false, true],
            |big| if big { "Big-endian" } else { "Little-endian" }.to_string(),
            |big| has_byte_order && big == settings.big_endian,
            |s, big| s.big_endian = big,
            cx,
        );

        let summary = self.summary_line();
        let can_import = self.can_import();
        let file_name = self.file_name.clone();
        let on_close = self.on_close.clone();
        let cancel_close = self.on_close.clone();
        let on_import = self.on_import.clone();
        let import_entity = cx.entity().clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .child(external_window_titlebar(
                "Import Raw PCM",
                "raw-pcm-import-dialog-close",
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(10.0))
                    .p(px(16.0))
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
                                    .text_color(if can_import {
                                        Colors::text_muted()
                                    } else {
                                        Colors::text_disabled()
                                    })
                                    .child(summary),
                            ),
                    )
                    .child(fb_form_row("Sample rate", rates))
                    .child(fb_form_row("Channels", channels))
                    .child(fb_form_row("Format", formats))
                    .child(fb_form_row(
                        "Byte order",
                        div()
                            .when(!has_byte_order, |this| {
                                this.opacity(crate::theme::state::DISABLED_CONTENT)
                            })
                            .child(byte_order),
                    ))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .line_height(px(15.0))
                            .text_color(Colors::text_faint())
                            .child(
                                "Read from the first byte. Imported as a 32-bit float WAV in \
                                 Assets/Audio; the original file is left untouched.",
                            ),
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
                        "raw-pcm-import-cancel",
                        "Cancel",
                        FbButtonKind::Default,
                        true,
                        move |_, window, cx| {
                            cancel_close(window, cx);
                            window.remove_window();
                        },
                    ))
                    .child(fb_button(
                        "raw-pcm-import-confirm",
                        "Import",
                        FbButtonKind::Primary,
                        can_import,
                        move |_, window, cx| {
                            let settings = import_entity.read_with(cx, |this, _| this.settings);
                            window.remove_window();
                            on_import(settings, window, cx);
                        },
                    )),
            )
    }
}

pub fn open_raw_pcm_import_dialog(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    setup: RawPcmImportDialogSetup,
    on_import: OnImport,
    on_close: OnClose,
    cx: &mut App,
) -> Result<WindowHandle<RawPcmImportDialog>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(
            px(RAW_PCM_IMPORT_DIALOG_WIDTH),
            px(RAW_PCM_IMPORT_DIALOG_HEIGHT),
        ),
        cx,
    );

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|_cx| RawPcmImportDialog::new(setup, on_import, on_close))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(file_len: u64, settings: RawPcmSettings) -> RawPcmImportDialog {
        RawPcmImportDialog::new(
            RawPcmImportDialogSetup {
                file_name: "dump.raw".to_string(),
                file_len,
                settings,
            },
            Arc::new(|_, _, _| {}),
            Arc::new(|_, _| {}),
        )
    }

    #[test]
    fn the_summary_follows_the_settings_and_names_leftover_bytes() {
        let stereo = dialog(4 * 44_100 + 2, RawPcmSettings::default());
        assert_eq!(
            stereo.summary_line(),
            "0:01.0 · 44100 frames · 2 trailing bytes ignored"
        );

        let mono = dialog(
            4 * 44_100 + 2,
            RawPcmSettings {
                channels: 1,
                ..RawPcmSettings::default()
            },
        );
        assert_eq!(mono.summary_line(), "0:02.0 · 88201 frames");
    }

    #[test]
    fn import_is_disabled_for_a_file_shorter_than_one_frame() {
        assert!(!dialog(3, RawPcmSettings::default()).can_import());
        assert!(dialog(4, RawPcmSettings::default()).can_import());
    }

    #[test]
    fn segment_labels() {
        assert_eq!(rate_label(44_100), "44.1k");
        assert_eq!(rate_label(22_050), "22.05k");
        assert_eq!(rate_label(48_000), "48k");
        assert_eq!(channel_label(1), "Mono");
        assert_eq!(channel_label(6), "6");
        assert_eq!(position(0, 1), FbSegment::Only);
        assert_eq!(position(0, 3), FbSegment::First);
        assert_eq!(position(1, 3), FbSegment::Middle);
        assert_eq!(position(2, 3), FbSegment::Last);
    }
}
