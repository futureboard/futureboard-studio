use std::sync::Arc;

use crate::components::title_bar::{status_item, STATUSBAR_HEIGHT};
use crate::components::{
    background_task_button, background_task_panel, BackgroundTaskCancelCb, BackgroundTaskStore,
    BackgroundTaskToggleCb,
};
use crate::theme::Colors;
use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window,
};

#[derive(Debug, Clone)]
pub struct StatusBarPerfMetrics {
    pub pill_label: String,
    pub renderer: String,
    pub display_sync: String,
    pub fps: f32,
    pub frame_ms: f32,
    pub peak_ms: f32,
    pub has_sample: bool,
}

#[derive(Debug, Clone)]
pub struct StatusBarContent {
    pub left: String,
    pub audio: String,
    pub perf: Option<StatusBarPerfMetrics>,
    /// While Virtual Speaker is on, what it is simulating: the monitoring is
    /// not the mix as it is, and that must stay in sight.
    pub listening: Option<String>,
}

pub type PerfMetricsToggleCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;
/// Open the Virtual Speaker window from its status pill.
pub type ListeningOpenCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;

pub fn status_bar(left: impl Into<String>, right: impl Into<String>) -> impl IntoElement {
    status_bar_inner(
        StatusBarContent {
            left: left.into(),
            audio: right.into(),
            perf: None,
            listening: None,
        },
        None,
        None,
        None,
        None,
        false,
        None,
    )
}

pub fn status_bar_with_background_tasks(
    content: StatusBarContent,
    tasks: &BackgroundTaskStore,
    on_toggle_tasks: BackgroundTaskToggleCb,
    on_cancel_task: BackgroundTaskCancelCb,
    perf_popover_open: bool,
    on_toggle_perf_popover: Option<PerfMetricsToggleCb>,
    on_open_listening: Option<ListeningOpenCb>,
) -> impl IntoElement {
    status_bar_inner(
        content,
        Some(tasks),
        Some(on_toggle_tasks),
        Some(on_cancel_task),
        on_toggle_perf_popover,
        perf_popover_open,
        on_open_listening,
    )
}

fn status_bar_inner(
    content: StatusBarContent,
    tasks: Option<&BackgroundTaskStore>,
    on_toggle_tasks: Option<BackgroundTaskToggleCb>,
    on_cancel_task: Option<BackgroundTaskCancelCb>,
    on_toggle_perf_popover: Option<PerfMetricsToggleCb>,
    perf_popover_open: bool,
    on_open_listening: Option<ListeningOpenCb>,
) -> impl IntoElement {
    let listening_pill = content
        .listening
        .clone()
        .map(|label| listening_pill(label, on_open_listening).into_any_element());
    let task_button = match (tasks, on_toggle_tasks.clone()) {
        (Some(tasks), Some(on_toggle)) => {
            Some(background_task_button(tasks, on_toggle).into_any_element())
        }
        _ => None,
    };
    let task_panel = match (tasks, on_toggle_tasks, on_cancel_task) {
        (Some(tasks), Some(on_close), Some(on_cancel)) if tasks.panel_open => {
            Some(background_task_panel(tasks, on_close, on_cancel).into_any_element())
        }
        _ => None,
    };
    let perf_pill = content.perf.as_ref().and_then(|metrics| {
        on_toggle_perf_popover.clone().map(|on_toggle| {
            perf_metrics_pill(metrics, perf_popover_open, on_toggle).into_any_element()
        })
    });
    let perf_panel = match (content.perf.as_ref(), perf_popover_open) {
        (Some(metrics), true) => Some(perf_metrics_panel(metrics).into_any_element()),
        _ => None,
    };

    div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(STATUSBAR_HEIGHT))
        .px(px(6.0))
        .gap(px(8.0))
        .bg(Colors::statusbar_bg())
        .border_t(px(1.0))
        .border_color(Colors::panel_border())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(status_item(content.left, true)),
        )
        .child(
            div()
                .flex_none()
                .min_w(px(0.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .children(listening_pill)
                .children(task_button)
                .children(perf_pill)
                .child(
                    div()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .child(status_item(content.audio, false)),
                ),
        )
        .children(task_panel)
        .children(perf_panel)
}

/// Virtual Speaker is on: in the accent, so it is not mistaken for the plain
/// mix. Clicking it opens the window to switch it off or change system.
fn listening_pill(label: String, on_open: Option<ListeningOpenCb>) -> impl IntoElement {
    div()
        .id("status-virtual-speaker")
        .h(px(18.0))
        .max_w(px(220.0))
        .flex()
        .flex_row()
        .items_center()
        .px(px(7.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_accent())
        .bg(Colors::accent_muted())
        .when_some(on_open, |pill, on_open| {
            pill.cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_hover()))
                .on_click(move |_, w, cx| on_open(w, cx))
        })
        .child(
            div()
                .truncate()
                .text_size(px(10.0))
                .text_color(Colors::text_primary())
                .child(label),
        )
}

fn perf_metrics_pill(
    metrics: &StatusBarPerfMetrics,
    open: bool,
    on_toggle: PerfMetricsToggleCb,
) -> impl IntoElement {
    div()
        .id("status-perf-metrics")
        .h(px(18.0))
        .max_w(px(160.0))
        .flex()
        .flex_row()
        .items_center()
        .px(px(7.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(if open {
            Colors::border_accent()
        } else {
            Colors::border_subtle()
        })
        .bg(if open {
            Colors::accent_muted()
        } else {
            Colors::surface_input()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_hover()))
        .on_click(move |_, w, cx| on_toggle(&(), w, cx))
        .child(
            div()
                .truncate()
                .text_size(px(10.0))
                .text_color(Colors::text_secondary())
                .child(metrics.pill_label.clone()),
        )
}

fn perf_metrics_panel(metrics: &StatusBarPerfMetrics) -> impl IntoElement {
    let fps = if metrics.has_sample {
        format!("{:.0}", metrics.fps)
    } else {
        "—".to_string()
    };
    let frame = if metrics.has_sample {
        format!("{:.1} ms", metrics.frame_ms)
    } else {
        "—".to_string()
    };
    let peak = if metrics.has_sample {
        format!("{:.1} ms", metrics.peak_ms)
    } else {
        "—".to_string()
    };

    div()
        .absolute()
        .right(px(12.0))
        .bottom(px(26.0))
        .w(px(220.0))
        .flex()
        .flex_col()
        .rounded(px(crate::theme::radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .shadow(vec![gpui::BoxShadow {
            color: Colors::surface_overlay().into(),
            offset: gpui::point(px(0.0), px(16.0)),
            blur_radius: px(34.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .occlude()
        .child(
            div()
                .h(px(32.0))
                .flex()
                .flex_row()
                .items_center()
                .px(px(10.0))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("Performance"),
                ),
        )
        .child(div().flex().flex_col().gap(px(4.0)).p(px(10.0)).children([
            perf_detail_row("Renderer", &metrics.renderer),
            perf_detail_row("Display Sync", &metrics.display_sync),
            perf_detail_row("FPS", &fps),
            perf_detail_row("Frame", &frame),
            perf_detail_row("Peak", &peak),
        ]))
}

fn perf_detail_row(label: &'static str, value: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .truncate()
                .text_size(px(10.0))
                .text_color(Colors::text_secondary())
                .child(value.to_string()),
        )
}
