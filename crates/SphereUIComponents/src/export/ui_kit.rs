//! Layout primitives and formatting shared by the Export and Render dialogs,
//! so both read as one family: the same section cards, readout rows, footer
//! band and number formats.

use std::sync::Arc;

use gpui::{
    div, px, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled,
};

use crate::components::controls::fb_section_header;
use crate::i18n::I18n;
use crate::theme::{radius, size, space, typography, Colors};

use DirectAudio::ExportStage;

use super::export_settings::{ExportEstimate, ExportSettingsError};

/// Horizontal inset shared by the body, the status strip and the footer, so
/// every band in a dialog starts on the same line.
pub(super) const BODY_PAD_X: f32 = space::LOOSE;
/// Footer band: one primary button plus its breathing room, top and bottom.
pub(super) const FOOTER_HEIGHT: f32 = size::PROMINENT + 2.0 * space::BASE;
/// Ceiling for a right-aligned readout value before it truncates. A layout
/// constant (a measured column), not spacing.
pub(super) const READOUT_VALUE_MAX: f32 = 200.0;
/// Ceiling for a form section's width. Maximized, an uncapped form would stretch
/// an 86 px label away from a 1800 px control and stop reading as one row.
pub(super) const FORM_MAX_WIDTH: f32 = 720.0;
/// Left rail that gives a status band a second, non-colour channel.
pub(super) const STATUS_RAIL_WIDTH: f32 = 2.0;

// ── Layout primitives ────────────────────────────────────────────────────────

/// The dialog's single scroll owner and horizontal clip owner.
pub(super) fn body_scroll() -> gpui::Stateful<gpui::Div> {
    div()
        .id("export-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        // Long paths and track names truncate; a dialog that scrolls sideways
        // is a layout bug, not a feature.
        .overflow_x_hidden()
        .gap(px(space::LOOSE))
        .px(px(BODY_PAD_X))
        .py(px(space::LOOSE))
}

/// Caption plus a bordered card of rows. This is the dialog card idiom (the
/// Add Track dialog's `form_panel`), not the right dock's inspector card.
pub(super) fn form_section(title: String, rows: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .max_w(px(FORM_MAX_WIDTH))
        .flex_shrink_0()
        .gap(px(space::SNUG))
        .child(fb_section_header(title))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .rounded(px(radius::SURFACE))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_panel_alt())
                .p(px(space::BASE))
                .children(rows),
        )
}

pub(super) fn form_row(label: String, control: impl IntoElement) -> gpui::AnyElement {
    crate::components::controls::fb_form_row(label, control).into_any_element()
}

/// Right-aligned readout. Values take tabular figures so a column of numbers
/// lines up on one grid, and truncate rather than wrap.
pub(super) fn readout_row(label: String, value: String) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::DEFAULT))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(READOUT_VALUE_MAX))
                .truncate()
                .font_features(tabular_figures())
                .text_size(px(typography::UI_XS))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(value),
        )
        .into_any_element()
}

/// One written file: the name leads, the measurement stays quiet behind it.
pub(super) fn file_row(name: String, meta: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::DEFAULT))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_primary())
                .child(name),
        )
        .child(
            div()
                .flex_shrink_0()
                .font_features(tabular_figures())
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(meta),
        )
}

/// Full-width path line: the value needs the whole row, so it does not share
/// one with a label column.
pub(super) fn path_row(label: String, path: String) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(path),
        )
}

/// A quiet full-width explanation inside a section. Wraps: it is prose, not a
/// chrome label.
pub(super) fn note_row(text: String) -> gpui::AnyElement {
    div()
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_muted())
        .child(text)
        .into_any_element()
}

/// Read-only, recessed surface for a value the user cannot type into.
pub(super) fn readout_surface(text: String) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .h(px(size::COMFORTABLE))
        .px(px(space::BASE))
        .flex()
        .items_center()
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_secondary())
        .truncate()
        .child(text)
}

/// The action band. One accent primary action, an explicit Cancel, and a fixed
/// height so the footer never moves between dialog states.
pub(super) fn footer_band() -> gpui::Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(space::BASE))
        .h(px(FOOTER_HEIGHT))
        .px(px(BODY_PAD_X))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_titlebar())
}

/// Tabular (fixed-advance) figures for readouts, per DESIGN.md's numeric rules.
/// A no-op on faces without the feature, so it can never make text worse.
pub(super) fn tabular_figures() -> gpui::FontFeatures {
    gpui::FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]))
}

// ── Formatting ───────────────────────────────────────────────────────────────

/// `m:ss.mmm` — the arrangement's own time language.
pub(super) fn format_duration(seconds: f64) -> String {
    let clamped = if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        0.0
    };
    let total_ms = (clamped * 1000.0).round() as u64;
    let minutes = total_ms / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let ms = total_ms % 1000;
    format!("{minutes}:{secs:02}.{ms:03}")
}

pub(super) fn format_beats(beats: f64) -> String {
    let value = if beats.is_finite() { beats } else { 0.0 };
    format!("{value:.3}")
}

pub(super) fn format_db(db: f32) -> String {
    // U+2212 MINUS SIGN: a hyphen at 11 px reads as a dash, not a sign.
    if db < 0.0 {
        format!("\u{2212}{:.1}", -db)
    } else {
        format!("{db:.1}")
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.1} MB", value / (KIB * KIB))
    } else {
        format!("{:.2} GB", value / (KIB * KIB * KIB))
    }
}

/// Thousands-grouped integer. The value div is `whitespace_nowrap`, so a plain
/// space cannot break the number across lines.
pub(super) fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (len - index) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

pub(super) fn channel_label(channels: u16, i18n: I18n) -> String {
    if channels == 1 {
        i18n.tr_or("export.channels.mono", "Mono")
    } else {
        i18n.tr_or("export.channels.stereo", "Stereo")
    }
}

/// Rate · channels · resolution. MP3 has no meaningful bit depth — the settings
/// model only carries one because the container API wants a field — so it shows
/// the bitrate it is actually encoded at.
pub(super) fn output_spec(estimate: &ExportEstimate, i18n: I18n) -> String {
    let channels = channel_label(estimate.channels, i18n);
    match estimate.mp3_bitrate_kbps {
        Some(kbps) => format!("{} Hz · {channels} · {kbps} kbps", estimate.sample_rate),
        None => format!(
            "{} Hz · {channels} · {}-bit",
            estimate.sample_rate,
            estimate.sample_format.bits()
        ),
    }
}

pub(super) fn stage_key(stage: ExportStage) -> &'static str {
    match stage {
        ExportStage::Preparing => "export.stage.preparing",
        ExportStage::Rendering => "export.stage.rendering",
        ExportStage::AnalyzingPeak => "export.stage.analyzing-peak",
        ExportStage::Encoding => "export.stage.encoding",
        ExportStage::Finalizing => "export.stage.finalizing",
        ExportStage::Complete => "export.stage.complete",
        ExportStage::Failed => "export.stage.failed",
        ExportStage::Cancelled => "export.stage.cancelled",
    }
}

pub(super) fn peak_option_id(db: f32) -> String {
    format!("peak:{db:.1}")
}

pub(super) fn parse_peak_option(value: &str) -> Option<f32> {
    value.strip_prefix("peak:")?.parse::<f32>().ok()
}

pub(super) fn localized_error(language: &str, error: &ExportSettingsError) -> String {
    I18n::new(language).tr_or(error.message_key(), &error.user_message())
}

/// `I18n::tr_vars` falls back to the *key* when a message is missing, which
/// would print `export.readout.n-of-m` in the UI. This keeps the English
/// fallback and applies the same `{ $name }` substitution to whichever string
/// wins.
pub(super) fn tr_vars_or(i18n: I18n, key: &str, fallback: &str, vars: &[(&str, String)]) -> String {
    let mut text = i18n.tr_or(key, fallback);
    for (name, value) in vars {
        text = text.replace(&format!("{{ ${name} }}"), value);
    }
    text
}

pub(super) fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

pub(super) fn open_in_file_manager(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(dir).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(dir).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(())
}
