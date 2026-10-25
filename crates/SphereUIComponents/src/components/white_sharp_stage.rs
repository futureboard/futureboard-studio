//! The faceplate's stage, drawn by wgpu every frame: the plate's fields,
//! the pitch band with its scale lines and the voice's trace, and the ring
//! with the dial inside it.
//!
//! [`StageLayout`] is the one transform for the stage: the scene builds its
//! triangles from it and the window's overlay places the band's note names
//! and the dial's readout with it, so a label and its line cannot drift.
//!
//! The visualizer renderer has no multisampling, so every edge here is
//! feathered by a physical pixel of alpha instead.

#[cfg(feature = "gpu-renderer")]
use std::f32::consts::TAU;

#[cfg(feature = "gpu-renderer")]
use gpui::{Hsla, Rgba};

#[cfg(feature = "gpu-renderer")]
use crate::components::visualizer::scene::{Scene, Vertex};
use crate::components::white_sharp_style::{
    self as style, BAND_HEIGHT, BAND_MAX_REL, BAND_MIN_REL, BAND_TOP, ORB, ORB_HOLE, PLATE_W,
};

/// Readings the dial's circular trace keeps.
pub const RING: usize = 96;

/// A rectangle in logical pixels, relative to the stage's origin.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn centre(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
}

/// Where everything on the stage is, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageLayout {
    pub w: f32,
    pub h: f32,
    pub band_top: f32,
    pub band_h: f32,
    /// The band's left margin, where the note names sit.
    pub gutter: f32,
    /// Now, on the band: the trace runs left of it.
    pub now_x: f32,
    pub px_per_sec: f32,
    pub orb: Rect,
}

impl StageLayout {
    /// The stage at `w` × `h`; the ring where the panel laid it out, or
    /// centred at the plate's scale before it has.
    pub fn new(w: f32, h: f32, orb: Option<Rect>) -> Self {
        let scale = style::plate_scale(w, h);
        let orb = orb.filter(|orb| orb.w > 1.0).unwrap_or_else(|| {
            let d = ORB * PLATE_W * scale;
            Rect {
                x: (w - d) * 0.5,
                y: (h - d) * 0.5,
                w: d,
                h: d,
            }
        });
        Self {
            w,
            h,
            band_top: h * BAND_TOP,
            band_h: h * BAND_HEIGHT,
            gutter: (w * 0.055).min(42.0),
            now_x: w * 0.5,
            px_per_sec: w * 0.155,
            orb,
        }
    }

    /// The y of a pitch `rel` semitones above the band's tonic.
    pub fn y_of(&self, rel: f32) -> f32 {
        let pad = self.band_h * 0.1;
        let inner = self.band_h - pad * 2.0;
        let t = (rel - BAND_MIN_REL) / (BAND_MAX_REL - BAND_MIN_REL);
        self.band_top + pad + (1.0 - t) * inner
    }

    pub fn px_per_semitone(&self) -> f32 {
        self.band_h * 0.8 / (BAND_MAX_REL - BAND_MIN_REL)
    }

    /// The dial's unit: its face is 200 units across, as the mockup's SVG.
    pub fn dial_unit(&self) -> f32 {
        self.orb.w * ORB_HOLE / 200.0
    }

    /// A point `radius` dial units from the dial's centre, `deg` clockwise
    /// from twelve o'clock.
    pub fn dial_point(&self, deg: f32, radius: f32) -> (f32, f32) {
        let (cx, cy) = self.orb.centre();
        let u = self.dial_unit();
        let rad = deg.to_radians();
        (cx + rad.sin() * radius * u, cy - rad.cos() * radius * u)
    }
}

/// One reading on the band's trace: its position in the reading stream,
/// what was sung and heard, in MIDI notes, and the note it was pulled to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TracePoint {
    pub index: u64,
    pub input: Option<f32>,
    pub output: Option<f32>,
    pub target: Option<i32>,
}

/// Everything a frame of the stage shows.
pub struct StageFrame<'a> {
    pub layout: StageLayout,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// The band's tonic, as a MIDI note.
    pub anchor: i32,
    pub key: u8,
    /// The notes a voice can be pulled to.
    pub allowed: u16,
    pub bypass: bool,
    pub trace: &'a [TracePoint],
    /// The newest reading's index; the trace is drawn up to it at now.
    pub newest: u64,
    pub hop_seconds: f32,
    /// The note a voice is being pulled to, while it sounds.
    pub target: Option<i32>,
    /// The dial: its trace of cents, newest at `ring_cursor - 1`.
    pub ring: &'a [f32; RING],
    pub ring_cursor: usize,
    pub active: bool,
    /// The sounding note's pitch class.
    pub note_class: i32,
    /// The needle and the tonic's rotation, in degrees.
    pub needle_deg: f32,
    pub tonic_deg: f32,
    /// Each pip's opacity, by pitch class.
    pub pips: &'a [f32; 12],
    pub tick_alpha: f32,
    /// The lock ring's pulse: 0 to 1 while it runs.
    pub lock_pulse: Option<f32>,
    /// The pull arc's length, as a share of the circle, and its opacity.
    pub arc: (f32, f32),
}

#[cfg(feature = "gpu-renderer")]
fn rgba(color: impl Into<Rgba>) -> [f32; 4] {
    let c: Rgba = color.into();
    [c.r, c.g, c.b, c.a]
}

#[cfg(feature = "gpu-renderer")]
fn with_alpha(mut c: [f32; 4], alpha: f32) -> [f32; 4] {
    c[3] *= alpha.clamp(0.0, 1.0);
    c
}

/// Antialiased triangles in physical pixels, from logical coordinates.
#[cfg(feature = "gpu-renderer")]
struct Pen<'a> {
    out: &'a mut Vec<Vertex>,
    s: f32,
}

#[cfg(feature = "gpu-renderer")]
impl Pen<'_> {
    fn push_quad(&mut self, corners: [[f32; 2]; 4], colors: [[f32; 4]; 4]) {
        for i in [0usize, 1, 2, 0, 2, 3] {
            self.out.push(Vertex {
                position: corners[i],
                color: colors[i],
            });
        }
    }

    fn tri(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], ca: [f32; 4], cb: [f32; 4]) {
        self.out.push(Vertex {
            position: a,
            color: ca,
        });
        self.out.push(Vertex {
            position: b,
            color: cb,
        });
        self.out.push(Vertex {
            position: c,
            color: cb,
        });
    }

    /// A rectangle with a vertical gradient, logical.
    fn rect_v(&mut self, x: f32, y: f32, w: f32, h: f32, top: [f32; 4], bottom: [f32; 4]) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let s = self.s;
        let (x0, y0, x1, y1) = (x * s, y * s, (x + w) * s, (y + h) * s);
        self.push_quad(
            [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
            [top, top, bottom, bottom],
        );
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        self.rect_v(x, y, w, h, color, color);
    }

    /// A straight line `width` logical pixels wide, its sides feathered.
    fn line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, color: [f32; 4]) {
        let s = self.s;
        let (ax, ay, bx, by) = (a.0 * s, a.1 * s, b.0 * s, b.1 * s);
        let (dx, dy) = (bx - ax, by - ay);
        let length = (dx * dx + dy * dy).sqrt();
        if length < 1.0e-3 || color[3] <= 0.0 {
            return;
        }
        let w = width * s;
        // Thinner than a pixel: a pixel wide, fainter.
        let (core, color) = if w < 1.0 {
            (0.0, with_alpha(color, w))
        } else {
            ((w - 1.0) * 0.5, color)
        };
        let clear = with_alpha(color, 0.0);
        let (nx, ny) = (-dy / length, dx / length);
        let at = |k: f32| ([ax + nx * k, ay + ny * k], [bx + nx * k, by + ny * k]);
        let (a_out, b_out) = at(core + 1.0);
        let (a_in, b_in) = at(core);
        let (a_in2, b_in2) = at(-core);
        let (a_out2, b_out2) = at(-core - 1.0);
        if core > 0.0 {
            self.push_quad([a_in, b_in, b_in2, a_in2], [color; 4]);
        }
        self.push_quad([a_out, b_out, b_in, a_in], [clear, clear, color, color]);
        self.push_quad([a_in2, b_in2, b_out2, a_out2], [color, color, clear, clear]);
    }

    /// A filled disc, logical, its rim feathered over `soft` logical pixels
    /// (a pixel at least).
    fn disc(&mut self, cx: f32, cy: f32, r: f32, soft: f32, color: [f32; 4]) {
        let s = self.s;
        let (cx, cy, r) = (cx * s, cy * s, r * s);
        let feather = (soft * s).max(1.0);
        let inner = (r - feather * 0.5).max(0.0);
        let outer = r + feather * 0.5;
        let segments = ((outer * 0.9) as usize).clamp(16, 192);
        let clear = with_alpha(color, 0.0);
        let mut prev = None;
        for i in 0..=segments {
            let t = i as f32 / segments as f32 * TAU;
            let (sin, cos) = t.sin_cos();
            let p_in = [cx + cos * inner, cy + sin * inner];
            let p_out = [cx + cos * outer, cy + sin * outer];
            if let Some((q_in, q_out)) = prev {
                if inner > 0.0 {
                    self.tri([cx, cy], q_in, p_in, color, color);
                }
                self.push_quad([q_in, q_out, p_out, p_in], [color, clear, clear, color]);
            }
            prev = Some((p_in, p_out));
        }
    }

    /// An arc of a ring, logical: centre, radius, stroke width, from and to
    /// degrees clockwise from twelve o'clock; round ends when `caps`.
    #[allow(clippy::too_many_arguments)]
    fn arc(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        width: f32,
        from: f32,
        to: f32,
        caps: bool,
        color: [f32; 4],
    ) {
        if (to - from).abs() < 0.01 || color[3] <= 0.0 {
            return;
        }
        let s = self.s;
        let (pcx, pcy, pr) = (cx * s, cy * s, r * s);
        let w = width * s;
        let (core, color) = if w < 1.0 {
            (0.0, with_alpha(color, w))
        } else {
            ((w - 1.0) * 0.5, color)
        };
        let clear = with_alpha(color, 0.0);
        let radii = [pr + core + 1.0, pr + core, pr - core, pr - core - 1.0];
        let span = (to - from).abs();
        let segments = ((span / 360.0 * pr * TAU / 3.0) as usize).clamp(6, 256);
        let at = |deg: f32, radius: f32| {
            let rad = deg.to_radians();
            [pcx + rad.sin() * radius, pcy - rad.cos() * radius]
        };
        for i in 0..segments {
            let d0 = from + (to - from) * i as f32 / segments as f32;
            let d1 = from + (to - from) * (i + 1) as f32 / segments as f32;
            let ring = |k: usize, deg: f32| at(deg, radii[k].max(0.0));
            if core > 0.0 {
                self.push_quad(
                    [ring(1, d0), ring(1, d1), ring(2, d1), ring(2, d0)],
                    [color; 4],
                );
            }
            self.push_quad(
                [ring(0, d0), ring(0, d1), ring(1, d1), ring(1, d0)],
                [clear, clear, color, color],
            );
            self.push_quad(
                [ring(2, d0), ring(2, d1), ring(3, d1), ring(3, d0)],
                [color, color, clear, clear],
            );
        }
        if caps {
            for deg in [from, to] {
                let rad = deg.to_radians();
                self.disc(
                    cx + rad.sin() * r,
                    cy - rad.cos() * r,
                    width * 0.5,
                    1.0 / s,
                    color,
                );
            }
        }
    }

    /// A soft shadow under a disc: solid inside `r - blur / 2`, gone by
    /// `r + blur / 2`.
    fn shadow(&mut self, cx: f32, cy: f32, r: f32, blur: f32, color: [f32; 4]) {
        self.disc(cx, cy, r, blur, color);
    }
}

#[cfg(feature = "gpu-renderer")]
fn hsl(c: Hsla) -> [f32; 4] {
    rgba(c)
}

/// The frame's triangles, into a scene cleared to the plate's top field.
#[cfg(feature = "gpu-renderer")]
pub fn build(frame: &StageFrame) -> Scene {
    let mut scene = Scene {
        clear: Some(rgba(style::color(style::PLATE))),
        ..Default::default()
    };
    let l = frame.layout;
    let mut pen = Pen {
        out: &mut scene.triangles,
        s: frame.scale,
    };

    // The plate: the lower field, and the band with its shadows.
    let band_bottom = l.band_top + l.band_h;
    pen.rect(
        0.0,
        band_bottom,
        l.w,
        l.h - band_bottom,
        rgba(style::color(style::FLOOR)),
    );
    pen.rect(
        0.0,
        l.band_top,
        l.w,
        l.band_h,
        rgba(style::color(style::BAND)),
    );
    let black = [0.0, 0.0, 0.0, 1.0];
    pen.rect_v(
        0.0,
        l.band_top - 7.0,
        l.w,
        7.0,
        with_alpha(black, 0.0),
        with_alpha(black, 0.42),
    );
    pen.rect_v(
        0.0,
        band_bottom,
        l.w,
        18.0,
        with_alpha(black, 0.4),
        with_alpha(black, 0.0),
    );

    band(&mut pen, frame);
    dial(&mut pen, frame);
    scene
}

/// The band: a line per semitone (the scale's in their colours), the sung
/// and heard pitch running in from the left, and the note the voice is
/// pulled to ahead of now.
#[cfg(feature = "gpu-renderer")]
fn band(pen: &mut Pen, frame: &StageFrame) {
    let l = frame.layout;
    let grey = frame.bypass;
    let mut rel = BAND_MIN_REL.ceil() as i32;
    while rel as f32 <= BAND_MAX_REL {
        let class = (frame.anchor + rel).rem_euclid(12);
        let tonic = rel.rem_euclid(12) == 0;
        let y = (l.y_of(rel as f32)).round() + 0.5;
        let in_scale = frame.allowed & (1 << class) != 0;
        let (color, width) = if in_scale {
            (
                hsl(style::pitch_ink_or_grey(
                    class,
                    if tonic { 0.95 } else { 0.8 },
                    70.0,
                    grey,
                )),
                if tonic { 1.6 } else { 1.15 },
            )
        } else {
            ([0.0, 0.0, 0.0, 0.32], 1.15)
        };
        pen.line((l.gutter, y), (l.w, y), width, color);
        rel += 1;
    }

    // The trace, left of now: what was sung in silver, what is heard in the
    // note's colour. Points closer than a pixel and a half are merged.
    let pps = l.px_per_sec;
    let x_of = |index: u64| {
        l.now_x - (frame.newest.saturating_sub(index)) as f32 * frame.hop_seconds * pps
    };
    let rel_of = |midi: f32| midi - frame.anchor as f32;
    let fade_w = l.w * 0.09;
    let silver = [214.0 / 255.0, 220.0 / 255.0, 228.0 / 255.0, 1.0];
    let band_lo = l.band_top;
    let band_hi = l.band_top + l.band_h;
    let clip_y = |y: f32| y.clamp(band_lo, band_hi);
    let mut series =
        |pick: fn(&TracePoint) -> Option<f32>, alpha: f32, width: f32, coloured: bool| {
            let mut last: Option<(f32, f32)> = None;
            for point in frame.trace {
                let x = x_of(point.index);
                if x < l.gutter - 2.0 {
                    continue;
                }
                let Some(midi) = pick(point) else {
                    last = None;
                    continue;
                };
                let y = clip_y(l.y_of(rel_of(midi)));
                if let Some((px, py)) = last {
                    if x - px < 1.5 && (y - py).abs() < 1.5 {
                        continue;
                    }
                    let fade = (((px + x) * 0.5) / fade_w).min(1.0);
                    let color = if coloured {
                        hsl(style::pitch_ink_or_grey(
                            midi.round() as i32,
                            alpha * fade,
                            74.0,
                            grey,
                        ))
                    } else {
                        with_alpha(silver, alpha * fade)
                    };
                    pen.line((px.max(l.gutter), py), (x.min(l.now_x), y), width, color);
                }
                last = Some((x, y));
            }
        };
    if frame.bypass {
        series(|p| p.input, 0.82, 1.7, false);
    } else {
        series(|p| p.input, 0.4, 1.35, false);
        series(|p| p.output, 0.95, 2.1, true);
    }

    // Ahead of now: the note the voice is being pulled to, dashed.
    if let (Some(target), true, false) = (frame.target, frame.active, frame.bypass) {
        let y = l.y_of(rel_of(target as f32));
        if y > band_lo && y < band_hi {
            let color = hsl(style::pitch_ink(target, 0.7, 74.0));
            let end = l.now_x + 0.6 * pps;
            let mut x = l.now_x + l.orb.w * 0.5 + 6.0;
            while x < end {
                pen.line((x, y), ((x + 2.0).min(end), y), 1.35, color);
                x += 6.0;
            }
        }
    }
}

/// The ring and the dial inside it.
#[cfg(feature = "gpu-renderer")]
fn dial(pen: &mut Pen, frame: &StageFrame) {
    let l = frame.layout;
    let orb = l.orb;
    let (cx, cy) = orb.centre();
    let r = orb.w * 0.5;
    let grey = frame.bypass;

    // The ring's drop shadow, the ring, and the face punched through it.
    pen.shadow(cx, cy + 18.0, r, 22.0, [0.0, 0.0, 0.0, 0.42]);
    pen.disc(cx, cy + 1.0, r, 1.0, [0.0, 0.0, 0.0, 0.35]);
    pen.disc(cx, cy, r, 1.0, rgba(style::color(style::BAND)));
    pen.disc(cx, cy, r * ORB_HOLE, 1.0, rgba(style::color(style::PLATE)));

    let u = l.dial_unit();
    // Forty-eight ticks, every fourth longer; they breathe while idle.
    for i in 0..48 {
        let major = i % 4 == 0;
        let deg = i as f32 * 7.5;
        let a = l.dial_point(deg, if major { 90.0 } else { 92.0 });
        let b = l.dial_point(deg, 98.0);
        let color = if major { 0x5c5c5c } else { 0x333333 };
        pen.line(
            a,
            b,
            if major { 1.25 } else { 1.0 },
            rgba(style::color_a(color, frame.tick_alpha)),
        );
    }

    // The cents the voice has wandered, round the dial.
    let ink_trace = hsl(style::pitch_ink_or_grey(
        frame.note_class,
        if frame.active { 0.72 } else { 0.28 },
        72.0,
        grey,
    ));
    let mut points = [(0.0f32, 0.0f32); RING];
    for (i, point) in points.iter_mut().enumerate() {
        let cents = frame.ring[(frame.ring_cursor + i) % RING];
        let deg = i as f32 / RING as f32 * 360.0;
        *point = l.dial_point(deg, 64.0 + cents / 50.0 * 5.0);
    }
    for i in 0..RING {
        pen.line(points[i], points[(i + 1) % RING], 1.15, ink_trace);
    }

    // The twelve pitch classes, the tonic at the top.
    for class in 0..12 {
        let steps = (class - i32::from(frame.key)).rem_euclid(12) as f32;
        let (x, y) = l.dial_point(steps * 30.0 + frame.tonic_deg, 84.0);
        let tonic = class == i32::from(frame.key);
        let light = if class == frame.note_class {
            82.0
        } else if tonic {
            74.0
        } else {
            62.0
        };
        let radius = if tonic { 1.8 } else { 1.35 } * u;
        pen.disc(
            x,
            y,
            radius,
            1.0 / frame.scale,
            hsl(style::pitch_ink_or_grey(
                class,
                frame.pips[class as usize],
                light,
                grey,
            )),
        );
    }

    // The needle on the sounding note.
    let ink = hsl(style::pitch_ink_or_grey(frame.note_class, 1.0, 78.0, grey));
    let a = l.dial_point(frame.needle_deg, 58.0);
    let b = l.dial_point(frame.needle_deg, 70.0);
    pen.line(a, b, 1.4, ink);
    let (hx, hy) = l.dial_point(frame.needle_deg, 72.0);
    pen.disc(hx, hy, 2.1 * u, 1.0 / frame.scale, ink);

    // The lock: a ring that swells and fades as the pitch settles.
    if let Some(p) = frame.lock_pulse {
        let ease = 1.0 - (1.0 - p).powi(3);
        let radius = 48.0 * u * (0.9 + 0.16 * ease);
        pen.arc(
            cx,
            cy,
            radius,
            1.0,
            0.0,
            360.0,
            false,
            with_alpha(ink, 0.75 * (1.0 - ease)),
        );
    }

    // The pull: an arc from the top, as long as the correction is far.
    let (length, alpha) = frame.arc;
    if length > 0.0 && alpha > 0.0 {
        let arc_ink = hsl(style::pitch_hsl(frame.note_class, 68.0, 74.0, alpha));
        pen.arc(
            cx,
            cy,
            orb.w * 0.43,
            orb.w * 0.0115,
            0.0,
            length * 360.0,
            true,
            arc_ink,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_spans_its_window_inside_its_padding() {
        let l = StageLayout::new(1024.0, 640.0, None);
        let top = l.y_of(BAND_MAX_REL);
        let bottom = l.y_of(BAND_MIN_REL);
        assert!((top - (l.band_top + l.band_h * 0.1)).abs() < 1.0e-3);
        assert!((bottom - (l.band_top + l.band_h * 0.9)).abs() < 1.0e-3);
        assert!(l.y_of(0.0) > l.y_of(1.0));
    }

    #[test]
    fn the_ring_is_centred_at_the_plate_scale() {
        let l = StageLayout::new(1024.0, 640.0, None);
        assert!((l.orb.w - 351.0).abs() < 0.5);
        assert_eq!(l.orb.centre(), (512.0, 320.0));
        let (x, y) = l.dial_point(0.0, 100.0);
        assert!((x - 512.0).abs() < 1.0e-3);
        assert!((320.0 - y - l.orb.w * ORB_HOLE * 0.5).abs() < 1.0e-3);
    }

    #[cfg(feature = "gpu-renderer")]
    #[test]
    fn a_frame_builds_without_a_voice() {
        let ring = [0.0; RING];
        let pips = [0.9; 12];
        let frame = StageFrame {
            layout: StageLayout::new(1024.0, 640.0, None),
            scale: 1.0,
            anchor: 60,
            key: 0,
            allowed: whitesharp::scale_mask(0, whitesharp::Scale::Major),
            bypass: false,
            trace: &[],
            newest: 0,
            hop_seconds: 0.004,
            target: None,
            ring: &ring,
            ring_cursor: 0,
            active: false,
            note_class: 0,
            needle_deg: 0.0,
            tonic_deg: 0.0,
            pips: &pips,
            tick_alpha: 1.0,
            lock_pulse: None,
            arc: (0.0, 0.0),
        };
        let scene = build(&frame);
        assert!(!scene.triangles.is_empty());
        assert_eq!(scene.triangles.len() % 3, 0);
    }
}
