//! The Voxtune note canvas element.
//!
//! The picture is built once as [`super::scene::Shape`]s and drawn by wgpu
//! ([`super::gpu`]) where the build has the GPU renderer and a device, or by
//! GPUI's quads and paths otherwise (the plug-in, a machine without a usable
//! adapter) — flat colours instead of gradients, otherwise the same notes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{canvas, fill, point, px, Bounds, IntoElement, Pixels, Rgba, Styled, Window};
use sphere_voxtune::engine::CantoNote;
use sphere_voxtune::scale::Scale;

use super::gpu::GpuCanvas;
use super::scene::{shapes, Color, Shape};
use super::viewport::VoxViewport;
use crate::theme::Colors;

/// Min/max peaks of the take in fixed blocks, built once per load.
#[derive(Debug, Default)]
pub struct Peaks {
    pub sample_rate: f32,
    pub block: usize,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

impl Peaks {
    pub const BLOCK: usize = 128;

    pub fn build(samples: &[f32], sample_rate: f32) -> Self {
        let blocks = samples.len().div_ceil(Self::BLOCK);
        let mut min = Vec::with_capacity(blocks);
        let mut max = Vec::with_capacity(blocks);
        for chunk in samples.chunks(Self::BLOCK) {
            let (lo, hi) = chunk
                .iter()
                .fold((0.0f32, 0.0f32), |(lo, hi), &s| (lo.min(s), hi.max(s)));
            min.push(lo);
            max.push(hi);
        }
        Self {
            sample_rate,
            block: Self::BLOCK,
            min,
            max,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.max.is_empty()
    }

    /// Peak range over `[t0, t1)` seconds.
    pub fn range(&self, t0: f32, t1: f32) -> Option<(f32, f32)> {
        if self.is_empty() || self.sample_rate <= 0.0 {
            return None;
        }
        let per_block = self.block as f32 / self.sample_rate;
        let a = (t0 / per_block).floor().max(0.0) as usize;
        let b = ((t1 / per_block).ceil().max(0.0) as usize).max(a + 1);
        if a >= self.max.len() {
            return None;
        }
        let b = b.min(self.max.len());
        let lo = self.min[a..b].iter().copied().fold(0.0f32, f32::min);
        let hi = self.max[a..b].iter().copied().fold(0.0f32, f32::max);
        Some((lo, hi))
    }
}

/// Everything one frame of the canvas needs, captured by value.
#[derive(Clone)]
pub struct CanvasData {
    pub notes: Rc<Vec<CantoNote>>,
    pub view: VoxViewport,
    pub key: u8,
    pub scale: Scale,
    pub bpm: f32,
    pub duration: f32,
    pub selected: Option<usize>,
    pub hovered: Option<usize>,
    /// A/B: draw what the take sang, not the edit.
    pub bypass: bool,
    /// Cut tool: the note under the blade and where it would cut (seconds).
    pub cut: Option<(usize, f32)>,
}

/// Canvas colours from the theme, resolved once per render.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Rgba,
    pub lane_row: Rgba,
    pub grid_bar: Rgba,
    pub grid_major: Rgba,
    pub grid_minor: Rgba,
    pub accent: Rgba,
    pub off_key: Rgba,
    pub out_of_take: Rgba,
}

impl Palette {
    pub fn resolve() -> Self {
        Self {
            background: Colors::timeline_background(),
            lane_row: Colors::with_alpha(Colors::text_primary(), 0.027),
            grid_bar: Colors::timeline_grid_bar(),
            grid_major: Colors::timeline_grid_major(),
            grid_minor: Colors::timeline_grid_minor(),
            accent: Colors::accent_primary(),
            off_key: Colors::status_error(),
            out_of_take: Colors::with_alpha(Colors::surface_canvas(), 0.55),
        }
    }
}

/// The canvas element. `gpu` draws it when it can; otherwise GPUI does.
pub fn note_canvas(
    data: CanvasData,
    capture: Rc<Cell<Option<Bounds<Pixels>>>>,
    gpu: Rc<RefCell<GpuCanvas>>,
) -> impl IntoElement {
    let pal = Palette::resolve();
    canvas(
        move |bounds, _window, _cx| capture.set(Some(bounds)),
        move |bounds: Bounds<Pixels>, (), window, cx| {
            if !gpu.borrow_mut().paint(&data, &pal, bounds, window, cx) {
                paint(&data, &pal, bounds, window);
            }
        },
    )
    .absolute()
    .inset_0()
}

fn to_rgba(c: Color) -> Rgba {
    Rgba {
        r: c[0],
        g: c[1],
        b: c[2],
        a: c[3],
    }
}

/// The GPUI fallback: the same shapes, flat.
fn paint(data: &CanvasData, pal: &Palette, bounds: Bounds<Pixels>, window: &mut Window) {
    let origin = bounds.origin;
    let w = f32::from(bounds.size.width).max(1.0);
    let h = f32::from(bounds.size.height).max(1.0);
    let at = |p: (f32, f32)| origin + point(px(p.0), px(p.1));
    window.paint_quad(fill(bounds, pal.background));
    for shape in shapes(data, pal, w, h) {
        match shape {
            Shape::Rect { x, y, w, h, color } => window.paint_quad(fill(
                Bounds::new(at((x, y)), gpui::size(px(w.max(0.0)), px(h.max(0.0)))),
                to_rgba(color),
            )),
            Shape::Fan { ring, flat, .. } => {
                let pts: Vec<_> = ring.into_iter().map(at).collect();
                let mut path = gpui::PathBuilder::fill();
                path.add_polygon(&pts, true);
                if let Ok(path) = path.build() {
                    window.paint_path(path, to_rgba(flat));
                }
            }
            Shape::Band {
                top,
                bottom,
                top_color,
                bottom_color,
            } => {
                let mut pts: Vec<_> = top.into_iter().map(at).collect();
                pts.extend(bottom.into_iter().rev().map(at));
                let mid = [
                    (top_color[0] + bottom_color[0]) * 0.5,
                    (top_color[1] + bottom_color[1]) * 0.5,
                    (top_color[2] + bottom_color[2]) * 0.5,
                    (top_color[3] + bottom_color[3]) * 0.5,
                ];
                let mut path = gpui::PathBuilder::fill();
                path.add_polygon(&pts, true);
                if let Ok(path) = path.build() {
                    window.paint_path(path, to_rgba(mid));
                }
            }
            Shape::Line {
                points,
                width,
                color,
                closed,
                dash,
            } => {
                if points.len() < 2 {
                    continue;
                }
                let mut path = gpui::PathBuilder::stroke(px(width));
                if let Some((on, off)) = dash {
                    path = path.dash_array(&[px(on), px(off)]);
                }
                let pts: Vec<_> = points.into_iter().map(at).collect();
                path.add_polygon(&pts, closed);
                if let Ok(path) = path.build() {
                    window.paint_path(path, to_rgba(color));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_cover_the_requested_span() {
        let sr = 1_000.0;
        let mut samples = vec![0.0f32; 1_000];
        samples[500] = 0.8;
        samples[700] = -0.6;
        let p = Peaks::build(&samples, sr);
        assert_eq!(p.range(0.0, 0.2), Some((0.0, 0.0)));
        let (lo, hi) = p.range(0.45, 0.75).unwrap();
        assert_eq!((lo, hi), (-0.6, 0.8));
        assert!(p.range(2.0, 3.0).is_none());
    }
}
