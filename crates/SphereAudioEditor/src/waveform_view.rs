//! Waveform peak drawing — min/max columns, plus sample-accurate points.

use std::sync::Arc;

use gpui::{
    Bounds, IntoElement, ParentElement, PathBuilder, PathStyle, Pixels, StrokeOptions, Styled,
    Window, canvas, div, fill, point, px, size,
};

use crate::{AmplitudeScale, AudioEditorTheme, DisplaySmoothing};

#[derive(Debug, Clone, Copy, Default)]
pub struct WaveformColumn {
    pub x: f32,
    pub min: f32,
    pub max: f32,
}

/// One decoded source frame mapped into canvas X.
#[derive(Debug, Clone, Copy, Default)]
pub struct WaveformSample {
    pub x: f32,
    pub value: f32,
    pub frame: i64,
}

#[derive(Debug, Clone)]
pub struct WaveformViewModel {
    pub columns: Vec<WaveformColumn>,
    pub samples: Vec<WaveformSample>,
    pub ready: bool,
    pub status_label: String,
    pub is_error: bool,
    pub show_progress: bool,
}

impl WaveformViewModel {
    pub fn loading(label: impl Into<String>) -> Self {
        Self {
            columns: Vec::new(),
            samples: Vec::new(),
            ready: false,
            status_label: label.into(),
            is_error: false,
            show_progress: true,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            columns: Vec::new(),
            samples: Vec::new(),
            ready: false,
            status_label: message.into(),
            is_error: true,
            show_progress: false,
        }
    }

    pub fn shows_samples(&self) -> bool {
        !self.samples.is_empty()
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "a flat GPUI element builder fed straight from the editor's view state"
)]
pub fn waveform_view(
    view_h: f32,
    viewport_width: f32,
    waveform: &WaveformViewModel,
    theme: &AudioEditorTheme,
    waveform_color: gpui::Rgba,
    amplitude_scale: AmplitudeScale,
    vertical_zoom: f32,
    smoothing: DisplaySmoothing,
) -> impl IntoElement {
    let columns = Arc::new(smoothed_columns(&waveform.columns, smoothing));
    let samples = Arc::new(waveform.samples.clone());
    let mut color = waveform_color;
    color.a = 0.9;
    let fill_color = gpui::Rgba {
        a: color.a * 0.42,
        ..color
    };
    let edge_color = gpui::Rgba {
        a: color.a,
        ..color
    };
    let zero_color = gpui::Rgba {
        a: theme.border_subtle.a * 0.9,
        ..theme.border_subtle
    };
    let view_h = view_h.max(1.0);
    let show_samples = waveform.shows_samples();
    let waveform_canvas = canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            if show_samples {
                paint_samples(
                    bounds,
                    samples.as_ref(),
                    fill_color,
                    edge_color,
                    amplitude_scale,
                    vertical_zoom,
                    window,
                );
            } else {
                paint_waveform(
                    bounds,
                    columns.as_ref(),
                    fill_color,
                    edge_color,
                    amplitude_scale,
                    vertical_zoom,
                    window,
                );
            }
            let center = bounds.size.height / 2.0;
            let zero = Bounds::new(
                bounds.origin + point(px(0.0), center - px(0.5)),
                size(bounds.size.width, px(1.0)),
            );
            window.paint_quad(fill(zero, zero_color));
        },
    )
    .absolute()
    .inset_0();

    let status_overlay = if !waveform.ready {
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .rounded_sm()
                        .border(px(1.0))
                        .border_color(if waveform.is_error {
                            theme.error
                        } else {
                            theme.border_subtle
                        })
                        .bg(gpui::Rgba {
                            a: 0.72,
                            ..theme.surface_base
                        })
                        .px(px(8.0))
                        .py(px(3.0))
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if waveform.is_error {
                            theme.error
                        } else {
                            theme.text_muted
                        })
                        .child(waveform.status_label.clone()),
                ),
        )
    } else {
        None
    };

    let progress_stripe = waveform.show_progress.then(|| {
        div()
            .absolute()
            .left_0()
            .right_0()
            .top(px(0.0))
            .h(px(2.0))
            .bg(gpui::Rgba {
                a: 0.55,
                ..theme.accent
            })
    });

    div()
        .relative()
        .w(px(viewport_width.max(1.0)))
        .h(px(view_h))
        .overflow_hidden()
        .bg(gpui::Rgba {
            a: 0.35,
            ..theme.surface_base
        })
        .child(waveform_canvas)
        .children(progress_stripe)
        .children(status_overlay)
}

fn smoothed_columns(
    columns: &[WaveformColumn],
    smoothing: DisplaySmoothing,
) -> Vec<WaveformColumn> {
    let taps = smoothing.taps();
    if taps < 3 || columns.len() < taps {
        return columns.to_vec();
    }
    let radius = taps / 2;
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let start = index.saturating_sub(radius);
            let end = (index + radius + 1).min(columns.len());
            let n = (end - start) as f32;
            let mut min_sum = 0.0;
            let mut max_sum = 0.0;
            for neighbour in &columns[start..end] {
                min_sum += neighbour.min;
                max_sum += neighbour.max;
            }
            WaveformColumn {
                x: column.x,
                min: min_sum / n,
                max: max_sum / n,
            }
        })
        .collect()
}

fn paint_waveform(
    bounds: Bounds<Pixels>,
    columns: &[WaveformColumn],
    fill_color: gpui::Rgba,
    edge_color: gpui::Rgba,
    amplitude_scale: AmplitudeScale,
    vertical_zoom: f32,
    window: &mut Window,
) {
    let height: f32 = bounds.size.height.into();
    if height < 1.0 {
        return;
    }
    let center = height * 0.5;
    for column in columns {
        if column.min == 0.0 && column.max == 0.0 {
            continue;
        }
        let min = amplitude_coordinate(column.min, amplitude_scale, vertical_zoom);
        let max = amplitude_coordinate(column.max, amplitude_scale, vertical_zoom);
        let top = center - max * center;
        let bottom = center - min * center;
        let x = column.x.round();
        let bar_height = (bottom - top).max(1.0);
        let bar = Bounds::new(
            bounds.origin + point(px(x), px(top)),
            size(px(1.0), px(bar_height)),
        );
        window.paint_quad(fill(bar, fill_color));
        let top_cap = Bounds::new(
            bounds.origin + point(px(x), px(top.round())),
            size(px(1.0), px(1.0)),
        );
        let bottom_cap = Bounds::new(
            bounds.origin + point(px(x), px((bottom - 1.0).round())),
            size(px(1.0), px(1.0)),
        );
        window.paint_quad(fill(top_cap, edge_color));
        window.paint_quad(fill(bottom_cap, edge_color));
    }
}

fn paint_samples(
    bounds: Bounds<Pixels>,
    samples: &[WaveformSample],
    fill_color: gpui::Rgba,
    edge_color: gpui::Rgba,
    amplitude_scale: AmplitudeScale,
    vertical_zoom: f32,
    window: &mut Window,
) {
    let height: f32 = bounds.size.height.into();
    if height < 1.0 || samples.is_empty() {
        return;
    }
    let center = height * 0.5;
    let mut points = Vec::with_capacity(samples.len());
    for sample in samples {
        let amp = amplitude_coordinate(sample.value, amplitude_scale, vertical_zoom);
        let y = center - amp * center;
        points.push((sample.x, y));
    }

    if points.len() >= 2 {
        let options = StrokeOptions::default();
        let mut line = PathBuilder::stroke(px(1.25)).with_style(PathStyle::Stroke(options));
        line.move_to(bounds.origin + point(px(points[0].0), px(points[0].1)));
        for &(x, y) in &points[1..] {
            line.line_to(bounds.origin + point(px(x), px(y)));
        }
        if let Ok(path) = line.build() {
            window.paint_path(path, fill_color);
        }
    }

    for &(x, y) in &points {
        let stem = Bounds::new(
            bounds.origin + point(px(x.round()), px(y.min(center))),
            size(px(1.0), px((y - center).abs().max(1.0))),
        );
        window.paint_quad(fill(
            stem,
            gpui::Rgba {
                a: fill_color.a * 0.45,
                ..fill_color
            },
        ));
        let dot = Bounds::new(
            bounds.origin + point(px(x.round() - 1.0), px(y.round() - 1.0)),
            size(px(3.0), px(3.0)),
        );
        window.paint_quad(fill(dot, edge_color));
    }
}

fn amplitude_coordinate(value: f32, scale: AmplitudeScale, vertical_zoom: f32) -> f32 {
    let signed = value.signum();
    let magnitude = value.abs().clamp(0.0, 1.0);
    let normalized = match scale {
        AmplitudeScale::Linear => magnitude,
        AmplitudeScale::Decibels => {
            ((20.0 * magnitude.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0)
        }
    };
    (signed * normalized * vertical_zoom.clamp(0.25, 16.0)).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_averages_neighbours_without_shifting_x() {
        let columns = vec![
            WaveformColumn {
                x: 0.0,
                min: -1.0,
                max: 1.0,
            },
            WaveformColumn {
                x: 1.0,
                min: 0.0,
                max: 0.0,
            },
            WaveformColumn {
                x: 2.0,
                min: -1.0,
                max: 1.0,
            },
        ];
        let smoothed = smoothed_columns(&columns, DisplaySmoothing::Light);
        assert_eq!(smoothed[1].x, 1.0);
        assert!((smoothed[1].max - 2.0 / 3.0).abs() < 1.0e-5);
        let raw = smoothed_columns(&columns, DisplaySmoothing::Off);
        assert_eq!(raw[1].max, 0.0);
    }
}
