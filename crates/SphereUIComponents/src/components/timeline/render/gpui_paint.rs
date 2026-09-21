//! GPUI paint fallback — existing quad/div grid path driven by snapshots.

use std::sync::Arc;

use gpui::{canvas, fill, point, px, size, Bounds, IntoElement, Pixels, Styled};

use super::renderer::{TimelineRenderOutput, TimelineRenderer};
use super::snapshot::TimelineRenderSnapshot;
use crate::components::timeline::timeline_state::GridLineLevel;
use crate::theme::Colors;

/// `FUTUREBOARD_TIMELINE_LAYER_DEBUG=1` — trace grid paint ordering. Cached:
/// this is read inside the `paint_grid` canvas closure, which runs on every
/// timeline repaint.
fn timeline_layer_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_TIMELINE_LAYER_DEBUG").is_some())
}

/// Renders the arrangement grid via GPUI `canvas` + `paint_quad`.
pub struct GpuiPaintTimelineRenderer;

impl GpuiPaintTimelineRenderer {
    pub fn new() -> Self {
        Self
    }

    fn paint_grid(
        snapshot: &TimelineRenderSnapshot,
        bounds: Bounds<Pixels>,
        window: &mut gpui::Window,
    ) {
        // The canvas is inset to the current arrangement body, so its measured
        // bounds are authoritative. `snapshot.viewport.height` is updated by
        // layout state and can briefly retain the pre-resize height while a
        // dock/panel or expanded track changes the available workspace. Using
        // that stale value leaves every vertical grid line cut off mid-canvas.
        let (grid_width, grid_height) = arrangement_canvas_size(bounds);

        // IMPORTANT: do NOT use `window.paint_layer` here.
        // `paint_layer` is promoted into a separate compositor layer which can
        // appear above later GPUI elements (clips/playhead/selection). We want
        // strict DOM child ordering: grid/regions must stay behind content.
        if timeline_layer_debug_enabled() {
            eprintln!(
                "[timeline paint] base->regions->grid (gpui_paint) w={grid_width:.1} h={grid_height:.1}"
            );
        }

        for shade in &snapshot.bar_shades {
            let bar_bounds = local_bounds(bounds, shade.x, 0.0, shade.width, grid_height);
            window.paint_quad(fill(bar_bounds, Colors::timeline_region_background()));
        }

        for line in &snapshot.grid_lines {
            let color = match line.level {
                GridLineLevel::Bar => Colors::timeline_grid_bar(),
                GridLineLevel::Beat => Colors::timeline_grid_major(),
                GridLineLevel::Sub => Colors::timeline_grid_minor(),
            };
            let line_bounds = local_bounds(bounds, line.x, 0.0, 1.0, grid_height);
            window.paint_quad(fill(line_bounds, color));
        }
    }
}

impl TimelineRenderer for GpuiPaintTimelineRenderer {
    fn backend_name(&self) -> &'static str {
        "gpui-paint"
    }

    fn render_arrangement(&mut self, snapshot: TimelineRenderSnapshot) -> TimelineRenderOutput {
        let _s = crate::perf::PerfScope::enter("GpuiPaintTimelineRenderer");
        crate::perf::count("grid_lines", snapshot.grid_lines.len() as u64);
        crate::perf::count("visible_clips", snapshot.clips.len() as u64);

        let snapshot = Arc::new(snapshot);
        let element = canvas(
            |_bounds, _window, _cx| {},
            move |bounds, (), window, _cx| {
                GpuiPaintTimelineRenderer::paint_grid(snapshot.as_ref(), bounds, window);
            },
        )
        .absolute()
        .inset_0()
        .into_any_element();

        TimelineRenderOutput::Gpui(element)
    }
}

fn local_bounds(parent: Bounds<Pixels>, x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
    Bounds::new(
        parent.origin + point(px(x), px(y)),
        size(px(width.max(0.0)), px(height.max(0.0))),
    )
}

fn arrangement_canvas_size(bounds: Bounds<Pixels>) -> (f32, f32) {
    (
        f32::from(bounds.size.width).max(0.0),
        f32::from(bounds.size.height).max(0.0),
    )
}

/// Dev-only: log snapshot stats when WGPU path runs in parallel with GPUI display.
pub fn log_snapshot_stats(snapshot: &TimelineRenderSnapshot, backend: &str) {
    if std::env::var_os("FUTUREBOARD_GPU_RENDERER_DEBUG").is_some() {
        eprintln!(
            "[gpu-renderer] snapshot backend={backend} grid={} clips={} lanes={} tracks={}..{}",
            snapshot.grid_lines.len(),
            snapshot.clips.len(),
            snapshot.lanes.len(),
            snapshot.visible_tracks.start_index,
            snapshot.visible_tracks.end_index,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrangement_grid_uses_current_canvas_height_after_resize() {
        let bounds = Bounds::new(point(px(12.0), px(24.0)), size(px(900.0), px(466.0)));
        assert_eq!(arrangement_canvas_size(bounds), (900.0, 466.0));
    }
}
