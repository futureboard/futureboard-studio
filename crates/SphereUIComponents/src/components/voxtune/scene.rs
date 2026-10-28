//! The note canvas as shapes — the Android editor's look (`lib/draw.dart`),
//! drawn on the desktop by wgpu ([`super::gpu`]) or, without a GPU, by GPUI
//! ([`super::paint`]).
//!
//! What is drawn, back to front:
//!
//! - the lane: scale rows tinted (the tonic in accent), the selected note's
//!   row, semitone lines, the beat grid;
//! - an edited note's *ghost*: where and how it was sung;
//! - the note itself, a capsule coloured by how far it sits from the scale
//!   (accent within 15 cents, error colour beyond), its height following the
//!   note's level, with the sung energy as a silhouette inside, a sheen and an
//!   outline;
//! - threads joining notes sung legato, then each note's played pitch over a
//!   dark halo;
//! - the selected note's target guide, the cut line, accent marks.
//!
//! [`Shape`]s are in logical pixels; the GPU path triangulates them with
//! per-vertex colour (gradients) and a one-pixel feather on lines, the GPUI
//! path fills and strokes them flat. One builder, so both draw the same notes.

use gpui::Rgba;
use sphere_voxtune::engine::{note_energy, pitch_at, CantoNote, ORN_NONE};
use sphere_voxtune::scale::scale_mask;

use super::paint::{CanvasData, Palette};
use super::viewport::{grid_lines, VoxViewport};

/// Cents from the scale beyond which a note reads as off-key (as the phone).
pub const OFF_KEY_CENTS: f32 = 15.0;

pub type Color = [f32; 4];

fn rgba(c: Rgba) -> Color {
    [c.r, c.g, c.b, c.a]
}

fn with_alpha(c: Rgba, a: f32) -> Color {
    [c.r, c.g, c.b, a]
}

fn lighten(c: Color, amount: f32) -> Color {
    [
        c[0] + (1.0 - c[0]) * amount,
        c[1] + (1.0 - c[1]) * amount,
        c[2] + (1.0 - c[2]) * amount,
        c[3],
    ]
}

fn scale_rgb(c: Color, r: f32, g: f32, b: f32, a: f32) -> Color {
    [c[0] * r, c[1] * g, c[2] * b, a]
}

/// One drawable, logical pixels.
#[derive(Debug, Clone)]
pub enum Shape {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
    },
    /// A convex polygon around `center`, coloured per ring point (vertical
    /// gradients); GPUI fills it with `flat`.
    Fan {
        center: (f32, f32),
        center_color: Color,
        ring: Vec<(f32, f32)>,
        colors: Vec<Color>,
        flat: Color,
    },
    /// A band between `top[i]` and `bottom[i]`, `top_color` → `bottom_color`.
    Band {
        top: Vec<(f32, f32)>,
        bottom: Vec<(f32, f32)>,
        top_color: Color,
        bottom_color: Color,
    },
    Line {
        points: Vec<(f32, f32)>,
        width: f32,
        color: Color,
        closed: bool,
        /// `(on, off)` dash lengths.
        dash: Option<(f32, f32)>,
    },
}

/// A note's capsule: the rounded box the phone draws.
#[derive(Debug, Clone, Copy)]
pub struct Capsule {
    pub x0: f32,
    pub x1: f32,
    pub cy: f32,
    pub half: f32,
    pub r: f32,
}

impl Capsule {
    /// The capsule `note` draws in `v` (height follows its level).
    pub fn of(note: &CantoNote, v: &VoxViewport) -> Self {
        let x0 = v.time_to_x(note.start);
        let x1 = v.time_to_x(note.start + note.dur).max(x0 + 3.0);
        let level = 10f32.powf(note.gain_db / 20.0).clamp(0.55, 1.35);
        let half = (v.row_h * 0.37 * level).clamp(6.0, 14.5);
        let r = half.min((x1 - x0) * 0.5);
        Self {
            x0,
            x1,
            cy: v.pitch_to_y(note.pitch),
            half,
            r,
        }
    }

    pub fn top(&self) -> f32 {
        self.cy - self.half
    }

    pub fn bottom(&self) -> f32 {
        self.cy + self.half
    }

    /// Half height of the capsule at `x` (narrower in the rounded ends).
    pub fn profile(&self, x: f32) -> f32 {
        let dx = if x < self.x0 + self.r {
            self.x0 + self.r - x
        } else if x > self.x1 - self.r {
            x - (self.x1 - self.r)
        } else {
            0.0
        };
        let corner = (self.r * self.r - dx * dx).max(0.0).sqrt();
        (self.half - self.r + corner).max(0.0)
    }

    /// The outline, clockwise from the top-left corner.
    pub fn ring(&self, inflate: f32) -> Vec<(f32, f32)> {
        let (x0, x1) = (self.x0 - inflate, self.x1 + inflate);
        let (top, bottom) = (self.top() - inflate, self.bottom() + inflate);
        let r = self.r + inflate;
        let corners = [
            (x1 - r, top + r, -90.0f32),
            (x1 - r, bottom - r, 0.0),
            (x0 + r, bottom - r, 90.0),
            (x0 + r, top + r, 180.0),
        ];
        let mut out = Vec::with_capacity(4 * 7);
        for (cx, cy, start) in corners {
            for k in 0..=6 {
                let a = (start + 90.0 * k as f32 / 6.0).to_radians();
                out.push((cx + r * a.cos(), cy + r * a.sin()));
            }
        }
        out
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && (y - self.cy).abs() <= self.profile(x)
    }
}

/// Points of `f` (pitch over the note's `0..1`) across `x0..x1`, skipping
/// what lies outside `0..view_w`.
pub fn curve_points(
    f: &dyn Fn(f32) -> f32,
    v: &VoxViewport,
    x0: f32,
    x1: f32,
    view_w: f32,
    fine: bool,
) -> Vec<(f32, f32)> {
    let width = (x1 - x0).max(1.0);
    let px_step = if fine { 1.5 } else { 3.0 };
    let steps = (width / px_step).ceil().clamp(2.0, 2_000.0) as usize;
    (0..=steps)
        .filter_map(|k| {
            let t = k as f32 / steps as f32;
            let x = x0 + width * t;
            (x >= -8.0 && x <= view_w + 8.0).then(|| (x, v.pitch_to_y(f(t))))
        })
        .collect()
}

/// Cents from the nearest note of the scale (the phone's `centsFromTarget`).
pub fn cents_from_scale(pitch: f32, key: u8, scale: sphere_voxtune::scale::Scale) -> (i32, f32) {
    let target = sphere_voxtune::edit::nearest_scale_note(pitch, key, scale);
    (target, (pitch - target as f32) * 100.0)
}

/// Whether the note, as played, is off the scale.
pub fn off_key(note: &CantoNote, key: u8, scale: sphere_voxtune::scale::Scale) -> bool {
    !note.unpitched
        && !note.muted
        && cents_from_scale(note.pitch, key, scale).1.abs() >= OFF_KEY_CENTS
}

/// Whether an edit moved this note away from what was sung.
pub fn edited(raw: &CantoNote) -> bool {
    (raw.pitch - raw.detected).abs() > 0.045
        || (raw.start - raw.detected_start).abs() > 0.012
        || (raw.dur - raw.detected_dur).abs() > 0.012
        || (raw.modulation - 1.0).abs() > 0.05
        || raw.orn_style != ORN_NONE
}

fn dashed_h(y: f32, x0: f32, x1: f32, color: Color) -> Shape {
    Shape::Line {
        points: vec![(x0, y), (x1, y)],
        width: 1.0,
        color,
        closed: false,
        dash: Some((2.0, 3.0)),
    }
}

/// The canvas for one frame of `data` in a `w` × `h` logical area.
pub fn shapes(data: &CanvasData, pal: &Palette, w: f32, h: f32) -> Vec<Shape> {
    let mut out = Vec::with_capacity(512);
    let v = data.view;
    let rect = |x: f32, y: f32, rw: f32, rh: f32, color: Color| Shape::Rect {
        x,
        y,
        w: rw,
        h: rh,
        color,
    };

    // ── Lane ──────────────────────────────────────────────────────────────
    let mask = scale_mask(data.key, data.scale);
    let tonic = (data.key % 12) as i32;
    let (lo, hi) = v.visible_pitches(h);
    let selected_row = data
        .selected
        .and_then(|i| data.notes.get(i))
        .filter(|n| !n.unpitched)
        .map(|n| n.present(data.bypass).pitch.round() as i32);
    for p in lo..=hi {
        let pc = p.rem_euclid(12);
        let top = v.row_top(p);
        if mask & (1 << pc) != 0 {
            let color = if pc == tonic {
                with_alpha(pal.accent, 0.06)
            } else {
                rgba(pal.lane_row)
            };
            out.push(rect(0.0, top, w, v.row_h, color));
        }
        if selected_row == Some(p) {
            out.push(rect(0.0, top, w, v.row_h, with_alpha(pal.accent, 0.07)));
        }
    }
    for line in grid_lines(&v, data.bpm, w) {
        if line.x < 0.0 || line.x > w {
            continue;
        }
        let color = if line.bar.is_some() {
            pal.grid_bar
        } else {
            pal.grid_major
        };
        out.push(rect(line.x.floor(), 0.0, 1.0, h, rgba(color)));
    }
    for p in lo..=hi {
        let pc = p.rem_euclid(12);
        let color = if pc == tonic {
            with_alpha(pal.accent, 0.32)
        } else if pc == 0 {
            rgba(pal.grid_major)
        } else {
            rgba(pal.grid_minor)
        };
        out.push(rect(0.0, v.pitch_to_y(p as f32).floor(), w, 1.0, color));
    }
    let start_x = v.time_to_x(0.0);
    if start_x > 0.0 {
        out.push(rect(0.0, 0.0, start_x.min(w), h, rgba(pal.out_of_take)));
    }
    let end_x = v.time_to_x(data.duration);
    if end_x < w {
        out.push(rect(
            end_x.max(0.0),
            0.0,
            w - end_x.max(0.0),
            h,
            rgba(pal.out_of_take),
        ));
    }

    // ── Notes ─────────────────────────────────────────────────────────────
    let (t_lo, t_hi) = v.visible_time(w);
    let visible = |n: &CantoNote| n.start + n.dur >= t_lo - 0.5 && n.start <= t_hi + 0.5;
    // Selected last, so it draws on top.
    let mut order: Vec<usize> = (0..data.notes.len()).collect();
    order.sort_by_key(|&i| data.selected == Some(i));
    let white = [1.0, 1.0, 1.0, 1.0];
    let accent = rgba(pal.accent);

    // Ghosts: where the edited notes were sung.
    for &i in &order {
        let raw = &data.notes[i];
        let shown = raw.present(data.bypass);
        if raw.unpitched || data.bypass || !edited(raw) || !visible(&shown) {
            continue;
        }
        let selected = data.selected == Some(i);
        let cap = Capsule::of(&shown, &v);
        let original = raw.present(true);
        let moved = (shown.pitch - raw.detected).abs() > 0.045
            || (shown.start - raw.detected_start).abs() > 0.012
            || (shown.dur - raw.detected_dur).abs() > 0.012;
        if moved {
            let gx0 = v.time_to_x(raw.detected_start);
            let gx1 = v
                .time_to_x(raw.detected_start + raw.detected_dur)
                .max(gx0 + 3.0);
            let gy = v.pitch_to_y(raw.detected);
            let gh = (v.row_h * 0.34).clamp(5.5, 13.0);
            let ghost = Capsule {
                x0: gx0,
                x1: gx1,
                cy: gy,
                half: gh,
                r: gh.min(4.0).min((gx1 - gx0) * 0.5),
            };
            let mid = (cap.x0 + cap.x1) * 0.5;
            if (cap.cy - gy).abs() > 3.0 {
                out.push(Shape::Line {
                    points: vec![(mid, gy), (mid, cap.cy)],
                    width: 1.0,
                    color: if selected {
                        with_alpha(pal.accent, 0.36)
                    } else {
                        [1.0, 1.0, 1.0, 0.14]
                    },
                    closed: false,
                    dash: None,
                });
            }
            let fill = if selected {
                with_alpha(pal.accent, 0.09)
            } else {
                [1.0, 1.0, 1.0, 0.045]
            };
            let ring = ghost.ring(0.0);
            out.push(Shape::Fan {
                center: ((gx0 + gx1) * 0.5, gy),
                center_color: fill,
                colors: vec![fill; ring.len()],
                ring: ring.clone(),
                flat: fill,
            });
            out.push(Shape::Line {
                points: ring,
                width: 1.0,
                color: if selected {
                    with_alpha(pal.accent, 0.34)
                } else {
                    [1.0, 1.0, 1.0, 0.16]
                },
                closed: true,
                dash: None,
            });
        }
        let gx0 = v.time_to_x(original.start);
        let gx1 = v.time_to_x(original.start + original.dur);
        out.push(Shape::Line {
            points: curve_points(&|t| pitch_at(&original, t), &v, gx0, gx1, w, false),
            width: 1.0,
            color: [1.0, 1.0, 1.0, if selected { 0.28 } else { 0.15 }],
            closed: false,
            dash: None,
        });
    }

    // Capsules.
    for &i in &order {
        let raw = &data.notes[i];
        let note = raw.present(data.bypass);
        if !visible(&note) {
            continue;
        }
        let selected = data.selected == Some(i);
        let hovered = data.hovered == Some(i) && !selected;
        let cap = Capsule::of(&note, &v);
        let neutral = note.unpitched || note.muted;
        let mut base = if neutral {
            [0.42, 0.42, 0.42, 1.0]
        } else if off_key(&note, data.key, data.scale) {
            rgba(pal.off_key)
        } else {
            accent
        };
        if selected {
            base = lighten(base, 0.12);
        } else if hovered {
            base = lighten(base, 0.06);
        }
        if selected && !neutral {
            let ring = cap.ring(1.5);
            let halo = with_alpha(pal.accent, 0.16);
            out.push(Shape::Fan {
                center: ((cap.x0 + cap.x1) * 0.5, cap.cy),
                center_color: halo,
                colors: vec![halo; ring.len()],
                ring,
                flat: halo,
            });
        }
        // Body: a vertical gradient, lighter on top.
        let (top_c, bottom_c) = if neutral {
            ([0.42, 0.42, 0.42, 0.36], [0.31, 0.31, 0.31, 0.36])
        } else {
            (
                [
                    base[0],
                    base[1],
                    base[2],
                    if selected { 0.46 } else { 0.34 },
                ],
                scale_rgb(base, 0.72, 0.68, 0.64, if selected { 0.56 } else { 0.42 }),
            )
        };
        let ring = cap.ring(0.0);
        let span = (cap.bottom() - cap.top()).max(1.0);
        let grad = |y: f32| {
            let t = ((y - cap.top()) / span).clamp(0.0, 1.0);
            [
                top_c[0] + (bottom_c[0] - top_c[0]) * t,
                top_c[1] + (bottom_c[1] - top_c[1]) * t,
                top_c[2] + (bottom_c[2] - top_c[2]) * t,
                top_c[3] + (bottom_c[3] - top_c[3]) * t,
            ]
        };
        out.push(Shape::Fan {
            center: ((cap.x0 + cap.x1) * 0.5, cap.cy),
            center_color: grad(cap.cy),
            colors: ring.iter().map(|p| grad(p.1)).collect(),
            ring: ring.clone(),
            flat: grad(cap.cy),
        });

        // Inside: the sung energy, centred, never past the capsule's edge.
        let steps = ((cap.x1 - cap.x0) / 3.0).ceil().clamp(2.0, 120.0) as usize;
        let mut top = Vec::with_capacity(steps + 1);
        let mut bottom = Vec::with_capacity(steps + 1);
        for k in 0..=steps {
            let t = k as f32 / steps as f32;
            let x = cap.x0 + (cap.x1 - cap.x0) * t;
            let half = (cap.half * (0.16 + 0.76 * note_energy(&note, t))).min(cap.profile(x));
            top.push((x, cap.cy - half));
            bottom.push((x, cap.cy + half));
        }
        let (e_top, e_bottom) = if neutral {
            ([0.55, 0.55, 0.55, 0.45], [0.44, 0.44, 0.44, 0.45])
        } else {
            (
                [
                    (base[0] + 0.07).min(1.0),
                    (base[1] + 0.06).min(1.0),
                    (base[2] + 0.05).min(1.0),
                    if selected { 0.94 } else { 0.86 },
                ],
                scale_rgb(base, 0.78, 0.74, 0.70, if selected { 0.94 } else { 0.86 }),
            )
        };
        out.push(Shape::Band {
            top,
            bottom,
            top_color: e_top,
            bottom_color: e_bottom,
        });
        // Centre line and the top sheen.
        out.push(Shape::Line {
            points: vec![
                (cap.x0 + cap.r * 0.3, cap.cy),
                (cap.x1 - cap.r * 0.3, cap.cy),
            ],
            width: 1.0,
            color: [1.0, 1.0, 1.0, 0.16],
            closed: false,
            dash: None,
        });
        let sheen_h = cap.half * 2.0 * 0.38;
        let mut s_top = Vec::with_capacity(steps + 1);
        let mut s_bottom = Vec::with_capacity(steps + 1);
        for k in 0..=steps {
            let t = k as f32 / steps as f32;
            let x = cap.x0 + (cap.x1 - cap.x0) * t;
            let edge = cap.cy - cap.profile(x);
            s_top.push((x, edge));
            s_bottom.push((x, (cap.top() + sheen_h).max(edge)));
        }
        out.push(Shape::Band {
            top: s_top,
            bottom: s_bottom,
            top_color: [1.0, 1.0, 1.0, if selected { 0.30 } else { 0.18 }],
            bottom_color: [1.0, 1.0, 1.0, 0.0],
        });
        // Outline.
        out.push(Shape::Line {
            points: ring,
            width: if selected { 1.5 } else { 1.0 },
            color: if neutral {
                [1.0, 1.0, 1.0, 0.10]
            } else if selected {
                [0.91, 0.91, 0.91, 0.96]
            } else if raw.pinned {
                [1.0, 1.0, 1.0, 0.62]
            } else {
                [0.04, 0.04, 0.04, 0.55]
            },
            closed: true,
            dash: None,
        });
        if note.muted {
            out.push(Shape::Line {
                points: vec![
                    (cap.x0 + cap.r * 0.6, cap.bottom() - 1.0),
                    (cap.x1 - cap.r * 0.6, cap.top() + 1.0),
                ],
                width: 1.5,
                color: [1.0, 1.0, 1.0, 0.45],
                closed: false,
                dash: None,
            });
        }
        if note.accent > 0.0 && !note.unpitched {
            let (x, y) = (cap.x0 + 7.0, cap.bottom() + 8.0);
            out.push(Shape::Line {
                points: vec![(x - 4.0, y - 3.5), (x + 4.0, y), (x - 4.0, y + 3.5)],
                width: 1.8,
                color: [0.91, 0.91, 0.91, 0.45 + 0.55 * note.accent.clamp(0.0, 1.0)],
                closed: false,
                dash: None,
            });
        }
    }

    // Threads between notes sung legato.
    let mut timed: Vec<usize> = (0..data.notes.len()).collect();
    timed.sort_by(|&a, &b| data.notes[a].start.total_cmp(&data.notes[b].start));
    for pair in timed.windows(2) {
        let a = data.notes[pair[0]].present(data.bypass);
        let b = data.notes[pair[1]].present(data.bypass);
        if a.unpitched || b.unpitched || !visible(&a) && !visible(&b) {
            continue;
        }
        let gap = b.start - (a.start + a.dur);
        if !(-0.02..=0.16).contains(&gap) {
            continue;
        }
        let p0 = (
            v.time_to_x(a.start + a.dur),
            v.pitch_to_y(pitch_at(&a, 1.0)),
        );
        let p1 = (v.time_to_x(b.start), v.pitch_to_y(pitch_at(&b, 0.0)));
        let mid = (p0.0 + p1.0) * 0.5;
        let pts = (0..=12)
            .map(|k| {
                let t = k as f32 / 12.0;
                let u = 1.0 - t;
                // Cubic with both handles at the midpoint's x.
                let x = u * u * u * p0.0
                    + 3.0 * u * u * t * mid
                    + 3.0 * u * t * t * mid
                    + t * t * t * p1.0;
                let y = u * u * u * p0.1
                    + 3.0 * u * u * t * p0.1
                    + 3.0 * u * t * t * p1.1
                    + t * t * t * p1.1;
                (x, y)
            })
            .collect();
        out.push(Shape::Line {
            points: pts,
            width: 1.0,
            color: [1.0, 1.0, 1.0, 0.28],
            closed: false,
            dash: None,
        });
    }

    // Played pitch over a dark halo.
    for &i in &order {
        let note = data.notes[i].present(data.bypass);
        if note.unpitched || !visible(&note) {
            continue;
        }
        let selected = data.selected == Some(i);
        let cap = Capsule::of(&note, &v);
        let pts = curve_points(
            &|t| pitch_at(&note, t),
            &v,
            cap.x0,
            cap.x1,
            w,
            note.orn_style != ORN_NONE,
        );
        if note.muted {
            out.push(Shape::Line {
                points: pts,
                width: 1.0,
                color: [1.0, 1.0, 1.0, 0.3],
                closed: false,
                dash: None,
            });
            continue;
        }
        out.push(Shape::Line {
            points: pts.clone(),
            width: if selected { 3.0 } else { 2.4 },
            color: [0.0, 0.0, 0.0, 0.52],
            closed: false,
            dash: None,
        });
        out.push(Shape::Line {
            points: pts,
            width: if selected { 1.65 } else { 1.25 },
            color: if selected {
                [1.0, 1.0, 1.0, 0.98]
            } else {
                [0.91, 0.91, 0.91, 0.86]
            },
            closed: false,
            dash: None,
        });
    }

    // The selected note's target: where the scale wants it.
    if let Some(note) = data
        .selected
        .and_then(|i| data.notes.get(i))
        .map(|n| n.present(data.bypass))
        .filter(|n| !n.unpitched)
    {
        let (target, cents) = cents_from_scale(note.pitch, data.key, data.scale);
        if cents.abs() >= 6.0 {
            let cap = Capsule::of(&note, &v);
            out.push(dashed_h(
                v.pitch_to_y(target as f32).floor() + 0.5,
                cap.x0,
                cap.x1,
                with_alpha(pal.accent, 0.55),
            ));
        }
    }

    // Cut tool: a line through the note with markers above and below.
    if let Some((i, t)) = data.cut {
        if let Some(note) = data.notes.get(i).map(|n| n.present(data.bypass)) {
            let cap = Capsule::of(&note, &v);
            let x = v.time_to_x(t).round() + 0.5;
            let (top, bottom) = (cap.top() - 18.0, cap.bottom() + 18.0);
            out.push(rect(x - 1.0, top, 2.0, bottom - top, accent));
            for (y, dir) in [(top, 1.0f32), (bottom, -1.0)] {
                let ring = vec![(x - 6.0, y - dir * 6.0), (x + 6.0, y - dir * 6.0), (x, y)];
                out.push(Shape::Fan {
                    center: (x, y - dir * 4.0),
                    center_color: accent,
                    colors: vec![accent; 3],
                    ring,
                    flat: accent,
                });
            }
        }
    }
    let _ = white;
    out
}

// =============================================================================
// Triangles (wgpu)
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

/// One frame of the canvas.
#[derive(Debug, Clone, Default)]
pub struct CanvasScene {
    pub clear: [f32; 4],
    pub vertices: Vec<GpuVertex>,
}

struct Builder<'a> {
    out: &'a mut Vec<GpuVertex>,
    s: f32,
}

impl Builder<'_> {
    fn p(&self, q: (f32, f32)) -> [f32; 2] {
        [q.0 * self.s, q.1 * self.s]
    }

    fn tri(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], ca: Color, cb: Color, cc: Color) {
        self.out.push(GpuVertex {
            position: a,
            color: ca,
        });
        self.out.push(GpuVertex {
            position: b,
            color: cb,
        });
        self.out.push(GpuVertex {
            position: c,
            color: cc,
        });
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        if w <= 0.0 || h <= 0.0 || c[3] <= 0.0 {
            return;
        }
        let (a, b) = (self.p((x, y)), self.p((x + w, y + h)));
        self.tri(a, [b[0], a[1]], b, c, c, c);
        self.tri(a, b, [a[0], b[1]], c, c, c);
    }

    /// A segment `width` logical pixels wide with a one-pixel feather.
    fn segment(&mut self, a: (f32, f32), b: (f32, f32), width: f32, color: Color) {
        let (pa, pb) = (self.p(a), self.p(b));
        let (dx, dy) = (pb[0] - pa[0], pb[1] - pa[1]);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-3 {
            return;
        }
        let (nx, ny) = (-dy / len, dx / len);
        let core = (width * self.s * 0.5 - 0.5).max(0.25);
        let edge = core + 1.0;
        let clear = [color[0], color[1], color[2], 0.0];
        let off = |o: f32| {
            (
                [pa[0] + nx * o, pa[1] + ny * o],
                [pb[0] + nx * o, pb[1] + ny * o],
            )
        };
        let (a_in, b_in) = off(core);
        let (a_in2, b_in2) = off(-core);
        let (a_out, b_out) = off(edge);
        let (a_out2, b_out2) = off(-edge);
        self.tri(a_in, b_in, b_in2, color, color, color);
        self.tri(a_in, b_in2, a_in2, color, color, color);
        self.tri(a_out, b_out, b_in, clear, clear, color);
        self.tri(a_out, b_in, a_in, clear, color, color);
        self.tri(a_in2, b_in2, b_out2, color, color, clear);
        self.tri(a_in2, b_out2, a_out2, color, clear, clear);
    }

    fn line(
        &mut self,
        pts: &[(f32, f32)],
        width: f32,
        color: Color,
        closed: bool,
        dash: Option<(f32, f32)>,
    ) {
        let mut segs: Vec<((f32, f32), (f32, f32))> =
            pts.windows(2).map(|w| (w[0], w[1])).collect();
        if closed && pts.len() > 2 {
            segs.push((pts[pts.len() - 1], pts[0]));
        }
        let Some((on, off)) = dash else {
            for (a, b) in segs {
                self.segment(a, b, width, color);
            }
            return;
        };
        let mut run = 0.0f32;
        for (a, b) in segs {
            let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            let mut t = 0.0;
            while t < len {
                let phase = run % (on + off);
                let step = if phase < on {
                    on - phase
                } else {
                    on + off - phase
                };
                let end = (t + step).min(len);
                if phase < on {
                    let at = |u: f32| (a.0 + (b.0 - a.0) * u / len, a.1 + (b.1 - a.1) * u / len);
                    self.segment(at(t), at(end), width, color);
                }
                run += end - t;
                t = end;
            }
        }
    }

    fn fan(&mut self, center: (f32, f32), cc: Color, ring: &[(f32, f32)], colors: &[Color]) {
        let c = self.p(center);
        for k in 0..ring.len() {
            let j = (k + 1) % ring.len();
            let (a, b) = (self.p(ring[k]), self.p(ring[j]));
            self.tri(c, a, b, cc, colors[k], colors[j]);
        }
    }

    fn band(&mut self, top: &[(f32, f32)], bottom: &[(f32, f32)], tc: Color, bc: Color) {
        for k in 0..top.len().min(bottom.len()).saturating_sub(1) {
            let (t0, t1) = (self.p(top[k]), self.p(top[k + 1]));
            let (b0, b1) = (self.p(bottom[k]), self.p(bottom[k + 1]));
            self.tri(t0, t1, b1, tc, tc, bc);
            self.tri(t0, b1, b0, tc, bc, bc);
        }
    }
}

/// The scene for one frame, in physical pixels at `scale`.
pub fn build(data: &CanvasData, pal: &Palette, w: f32, h: f32, scale: f32) -> CanvasScene {
    let mut scene = CanvasScene {
        clear: rgba(pal.background),
        vertices: Vec::with_capacity(16_384),
    };
    let mut b = Builder {
        out: &mut scene.vertices,
        s: scale,
    };
    for shape in shapes(data, pal, w, h) {
        match shape {
            Shape::Rect { x, y, w, h, color } => b.rect(x, y, w, h, color),
            Shape::Fan {
                center,
                center_color,
                ring,
                colors,
                ..
            } => b.fan(center, center_color, &ring, &colors),
            Shape::Band {
                top,
                bottom,
                top_color,
                bottom_color,
            } => b.band(&top, &bottom, top_color, bottom_color),
            Shape::Line {
                points,
                width,
                color,
                closed,
                dash,
            } => b.line(&points, width, color, closed, dash),
        }
    }
    scene
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capsule_narrows_only_in_its_ends() {
        let c = Capsule {
            x0: 0.0,
            x1: 100.0,
            cy: 50.0,
            half: 10.0,
            r: 10.0,
        };
        assert_eq!(c.profile(50.0), 10.0);
        assert!(c.profile(1.0) < 5.0);
        assert!(c.contains(50.0, 45.0));
        assert!(!c.contains(50.0, 65.0));
        let ring = c.ring(0.0);
        assert!(ring.iter().all(|p| p.0 >= -0.01 && p.0 <= 100.01));
    }

    #[test]
    fn a_segment_is_a_core_and_two_feathers() {
        let mut out = Vec::new();
        let mut b = Builder {
            out: &mut out,
            s: 2.0,
        };
        b.segment((0.0, 0.0), (10.0, 0.0), 2.0, [1.0; 4]);
        assert_eq!(out.len(), 18);
        assert!(out.iter().any(|v| v.position[0] == 20.0));
        assert!(out.iter().any(|v| v.color[3] == 0.0));
    }

    #[test]
    fn dashes_leave_gaps() {
        let pts = [(0.0, 0.0), (40.0, 0.0)];
        let mut solid = Vec::new();
        Builder {
            out: &mut solid,
            s: 1.0,
        }
        .line(&pts, 1.0, [1.0; 4], false, None);
        let mut dashed = Vec::new();
        Builder {
            out: &mut dashed,
            s: 1.0,
        }
        .line(&pts, 1.0, [1.0; 4], false, Some((2.0, 3.0)));
        assert!(dashed.len() > solid.len());
    }
}
