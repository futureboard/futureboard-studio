//! Player controls: vector icons (no icon font needed) and the seek bar.

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Shape,
    Stroke, StrokeKind, Ui,
};
use mkff_player::time::format_time;

pub const ACCENT: Color32 = Color32::from_rgb(79, 141, 255);
pub const FG: Color32 = Color32::from_rgb(235, 235, 240);
pub const FG_DIM: Color32 = Color32::from_rgb(160, 160, 170);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Play,
    Pause,
    Replay,
    Back,
    Forward,
    /// Speaker with 0..=2 waves, or muted.
    Volume(u8),
    Muted,
    Fullscreen,
    ExitFullscreen,
    Loop,
    Info,
    Open,
    Camera,
}

/// A round icon button. `active` tints the icon with the accent colour.
pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32, active: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let resp = resp.on_hover_text(tip);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() {
            p.circle_filled(rect.center(), size * 0.5, Color32::from_white_alpha(28));
        }
        let color = if active { ACCENT } else { FG };
        paint_icon(p, icon, rect.shrink(size * 0.24), color);
    }
    resp
}

pub fn paint_icon(p: &egui::Painter, icon: Icon, r: Rect, color: Color32) {
    let stroke = Stroke::new((r.width() * 0.11).max(1.5), color);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(
                vec![at(0.18, 0.05), at(0.95, 0.5), at(0.18, 0.95)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            for x in [0.14, 0.6] {
                p.rect_filled(
                    Rect::from_min_max(at(x, 0.06), at(x + 0.26, 0.94)),
                    CornerRadius::same(1),
                    color,
                );
            }
        }
        Icon::Replay => {
            arc(p, r.center(), r.width() * 0.42, -60.0, 250.0, stroke);
            let tip = polar(r.center(), r.width() * 0.42, -60.0);
            p.add(Shape::convex_polygon(
                vec![
                    tip + vec2(-r.width() * 0.22, -r.height() * 0.06),
                    tip + vec2(r.width() * 0.06, -r.height() * 0.2),
                    tip + vec2(r.width() * 0.04, r.height() * 0.12),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Back | Icon::Forward => {
            let tri = |x0: f32, x1: f32| {
                if icon == Icon::Forward {
                    vec![at(x0, 0.15), at(x1, 0.5), at(x0, 0.85)]
                } else {
                    vec![at(x1, 0.15), at(x0, 0.5), at(x1, 0.85)]
                }
            };
            p.add(Shape::convex_polygon(tri(0.02, 0.5), color, Stroke::NONE));
            p.add(Shape::convex_polygon(tri(0.48, 0.96), color, Stroke::NONE));
        }
        Icon::Volume(_) | Icon::Muted => {
            p.add(Shape::convex_polygon(
                vec![
                    at(0.0, 0.34),
                    at(0.22, 0.34),
                    at(0.5, 0.08),
                    at(0.5, 0.92),
                    at(0.22, 0.66),
                    at(0.0, 0.66),
                ],
                color,
                Stroke::NONE,
            ));
            match icon {
                Icon::Muted => {
                    let s = Stroke::new(stroke.width, color);
                    p.line_segment([at(0.64, 0.32), at(0.98, 0.68)], s);
                    p.line_segment([at(0.98, 0.32), at(0.64, 0.68)], s);
                }
                Icon::Volume(level) => {
                    let c = at(0.5, 0.5);
                    if level >= 1 {
                        arc(p, c, r.width() * 0.22, -50.0, 50.0, stroke);
                    }
                    if level >= 2 {
                        arc(p, c, r.width() * 0.44, -55.0, 55.0, stroke);
                    }
                }
                _ => {}
            }
        }
        Icon::Fullscreen | Icon::ExitFullscreen => {
            let k = 0.32;
            let corners = [
                (0.0, 0.0, 1.0, 1.0),
                (1.0, 0.0, -1.0, 1.0),
                (0.0, 1.0, 1.0, -1.0),
                (1.0, 1.0, -1.0, -1.0),
            ];
            for (cx, cy, dx, dy) in corners {
                let (x, y, dx, dy) = if icon == Icon::ExitFullscreen {
                    (cx + dx * k, cy + dy * k, -dx, -dy)
                } else {
                    (cx, cy, dx, dy)
                };
                let o = at(x, y);
                p.add(Shape::line(
                    vec![
                        o + vec2(dx * k * r.width(), 0.0),
                        o,
                        o + vec2(0.0, dy * k * r.height()),
                    ],
                    stroke,
                ));
            }
        }
        Icon::Loop => {
            let (w, h) = (r.width(), r.height());
            let rr = Rect::from_min_max(at(0.04, 0.22), at(0.96, 0.78));
            p.rect_stroke(
                rr,
                CornerRadius::same((h * 0.25) as u8),
                stroke,
                StrokeKind::Middle,
            );
            // arrow heads on the top-right and bottom-left
            let head = |tip: Pos2, dir: f32| {
                vec![
                    tip + vec2(0.0, -h * 0.16),
                    tip + vec2(dir * w * 0.2, 0.0),
                    tip + vec2(0.0, h * 0.16),
                ]
            };
            p.add(Shape::convex_polygon(
                head(at(0.62, 0.22), 1.0),
                color,
                Stroke::NONE,
            ));
            p.add(Shape::convex_polygon(
                head(at(0.38, 0.78), -1.0),
                color,
                Stroke::NONE,
            ));
        }
        Icon::Info => {
            p.circle_stroke(r.center(), r.width() * 0.46, stroke);
            p.circle_filled(at(0.5, 0.28), stroke.width * 0.75, color);
            p.line_segment([at(0.5, 0.44), at(0.5, 0.76)], stroke);
        }
        Icon::Open => {
            p.add(Shape::convex_polygon(
                vec![
                    at(0.0, 0.14),
                    at(0.38, 0.14),
                    at(0.48, 0.28),
                    at(1.0, 0.28),
                    at(1.0, 0.88),
                    at(0.0, 0.88),
                ],
                Color32::TRANSPARENT,
                stroke,
            ));
            p.line_segment([at(0.0, 0.42), at(1.0, 0.42)], stroke);
        }
        Icon::Camera => {
            p.rect_stroke(
                Rect::from_min_max(at(0.0, 0.26), at(1.0, 0.9)),
                CornerRadius::same(2),
                stroke,
                StrokeKind::Middle,
            );
            p.line_segment([at(0.3, 0.26), at(0.4, 0.1)], stroke);
            p.line_segment([at(0.4, 0.1), at(0.6, 0.1)], stroke);
            p.line_segment([at(0.6, 0.1), at(0.7, 0.26)], stroke);
            p.circle_stroke(at(0.5, 0.58), r.width() * 0.18, stroke);
        }
    }
}

fn polar(c: Pos2, radius: f32, deg: f32) -> Pos2 {
    let a = deg.to_radians();
    c + vec2(a.cos(), a.sin()) * radius
}

/// An arc from `from` to `to` degrees (0° = right, clockwise in screen space).
fn arc(p: &egui::Painter, c: Pos2, radius: f32, from: f32, to: f32, stroke: Stroke) {
    let n = (((to - from).abs() / 8.0).ceil() as usize).max(2);
    let pts = (0..=n)
        .map(|i| polar(c, radius, from + (to - from) * i as f32 / n as f32))
        .collect();
    p.add(Shape::line(pts, stroke));
}

/// What the seek bar did this frame.
#[derive(Default)]
pub struct SeekOutcome {
    /// The pointer went down on the bar.
    pub started: bool,
    /// Where the user is pointing / dragging (seconds) while the button is held.
    pub scrub_to: Option<f64>,
    /// The button was released over / after dragging the bar, at this position.
    pub released_at: Option<f64>,
    pub hovered: bool,
}

/// A full-width seek bar with hover time preview.
pub fn seek_bar(ui: &mut Ui, position: f64, duration: f64) -> SeekOutcome {
    let height = 18.0;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());
    let mut out = SeekOutcome {
        hovered: resp.hovered() || resp.dragged(),
        ..Default::default()
    };
    let duration = if duration.is_finite() && duration > 0.0 {
        duration
    } else {
        0.0
    };
    let frac_at = |x: f32| ((x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0) as f64;
    let pointer = resp.interact_pointer_pos().or(resp.hover_pos());

    if duration > 0.0 {
        // the press itself (a click starts no drag) or the first frame of a drag
        let pressed_here =
            resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed());
        if pressed_here || resp.drag_started() {
            out.started = true;
        }
        if resp.is_pointer_button_down_on() {
            if let Some(p) = resp.interact_pointer_pos() {
                out.scrub_to = Some(frac_at(p.x) * duration);
            }
        }
        if resp.drag_stopped() || resp.clicked() {
            if let Some(p) = pointer {
                out.released_at = Some(frac_at(p.x) * duration);
            }
        }
    }

    if !ui.is_rect_visible(rect) {
        return out;
    }
    let p = ui.painter();
    let active = out.hovered || resp.is_pointer_button_down_on();
    let track_h = if active { 6.0 } else { 4.0 };
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), track_h));
    let r = CornerRadius::same((track_h / 2.0) as u8);
    p.rect_filled(track, r, Color32::from_white_alpha(60));
    let shown = out.scrub_to.unwrap_or(position);
    let frac = if duration > 0.0 {
        (shown / duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    if let (true, Some(hp)) = (out.hovered, pointer) {
        let hx = hp.x.clamp(track.left(), track.right());
        p.rect_filled(
            Rect::from_min_max(track.left_top(), pos2(hx, track.bottom())),
            r,
            Color32::from_white_alpha(40),
        );
    }
    let played = Rect::from_min_max(
        track.left_top(),
        pos2(track.left() + track.width() * frac, track.bottom()),
    );
    p.rect_filled(played, r, ACCENT);
    if active {
        p.circle_filled(pos2(played.right(), track.center().y), 7.0, ACCENT);
    }
    // time under the pointer
    if let (true, Some(hp), true) = (out.hovered, pointer, duration > 0.0) {
        let t = frac_at(hp.x) * duration;
        let text = format_time(t, false);
        let font = FontId::proportional(13.0);
        let galley = p.layout_no_wrap(text, font, FG);
        let size = galley.size() + vec2(12.0, 6.0);
        let x =
            hp.x.clamp(rect.left() + size.x / 2.0, rect.right() - size.x / 2.0);
        let bubble = Rect::from_center_size(pos2(x, rect.top() - size.y / 2.0 - 6.0), size);
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("mkff-seek-bubble"),
        ));
        painter.rect_filled(
            bubble,
            CornerRadius::same(4),
            Color32::from_black_alpha(220),
        );
        painter.galley(bubble.min + vec2(6.0, 3.0), galley, FG);
    }
    out
}

/// A thin horizontal volume slider (0..=1). Returns the new value when changed.
pub fn volume_slider(ui: &mut Ui, value: f32, width: f32) -> Option<f32> {
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 20.0), Sense::click_and_drag());
    let mut changed = None;
    if resp.is_pointer_button_down_on() {
        if let Some(p) = resp.interact_pointer_pos() {
            changed = Some(((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0));
        }
    }
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let v = changed.unwrap_or(value);
        let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 4.0));
        p.rect_filled(track, CornerRadius::same(2), Color32::from_white_alpha(60));
        let filled = Rect::from_min_max(
            track.left_top(),
            pos2(track.left() + track.width() * v, track.bottom()),
        );
        p.rect_filled(filled, CornerRadius::same(2), FG);
        p.circle_filled(pos2(filled.right(), track.center().y), 6.0, FG);
    }
    resp.on_hover_text(format!("Volume {:.0} %", value * 100.0));
    changed
}

/// Paint a text label with a soft dark backing (for overlays on top of video).
pub fn overlay_text(p: &egui::Painter, pos: Pos2, align: Align2, text: &str, size: f32) -> Rect {
    let galley = p.layout_no_wrap(text.to_string(), FontId::proportional(size), FG);
    let pad = vec2(14.0, 8.0);
    let rect = align.anchor_size(pos, galley.size() + pad * 2.0);
    p.rect_filled(rect, CornerRadius::same(8), Color32::from_black_alpha(170));
    p.galley(rect.min + pad, galley, FG);
    rect
}
