//! Geometry for one visualizer frame.
//!
//! Everything the GPU draws is built here as plain triangles in the surface's
//! physical pixels: filled areas, lines (as thin quads), dots. The GPUI text
//! overlay places its labels with the same [`Layout`], so a frequency label and
//! its grid line are computed from one transform and cannot drift apart.

use gpui::Rgba;

use super::analysis::{Analyzer, DISPLAY_BINS, LOUDNESS_HISTORY, MAX_HZ, MIN_HZ};
use super::VisualizerKind;

/// One vertex: position in physical pixels, straight (not premultiplied)
/// colour.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

pub const VERTEX_FLOATS: usize = 6;

/// The spectrum's visible dB range.
pub const SPECTRUM_TOP_DB: f32 = 6.0;
pub const SPECTRUM_BOTTOM_DB: f32 = -84.0;
pub const SPECTRUM_GRID_DB: [f32; 7] = [0.0, -12.0, -24.0, -36.0, -48.0, -60.0, -72.0];
pub const FREQ_TICKS: [f32; 9] = [
    50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0,
];
/// The decade lines lead a frequency axis; the others recede.
pub fn is_major_freq(hz: f32) -> bool {
    matches!(hz as u32, 100 | 1_000 | 10_000)
}
/// The loudness graph's range and reference lines, in LUFS.
pub const LOUDNESS_TOP: f32 = 0.0;
pub const LOUDNESS_BOTTOM: f32 = -42.0;
pub const LOUDNESS_GRID: [f32; 6] = [-6.0, -12.0, -18.0, -24.0, -30.0, -36.0];
/// Streaming platforms normalise to about this.
pub const LOUDNESS_TARGET: f32 = -14.0;
/// The loudness graph's time lines, in seconds before now.
pub const LOUDNESS_TIME_GRID: [f32; 2] = [10.0, 20.0];
/// The oscilloscope's time divisions, in milliseconds.
pub const SCOPE_DIVISION_MS: f32 = 5.0;

/// A rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    const EMPTY: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Where each view's parts sit inside the surface, in logical pixels. The
/// scene scales it to physical pixels; the overlay uses it as is.
///
/// Every view is the same family: the title bar across the top, one recessed
/// [`Layout::panel`] holding the picture, its value axis in a gutter right of
/// the panel and its time or frequency axis in a gutter below it. Readouts
/// sit on the window's own surface, outside the panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub kind: VisualizerKind,
    pub size: [f32; 2],
    /// Where content starts: below the window's title bar.
    pub top: f32,
    /// The recessed plate the picture is drawn on.
    pub panel: Rect,
    /// The data area: the panel itself, or the half disc inside it.
    pub plot: Rect,
    /// Stereo image: the correlation and balance meters under the panel.
    pub strip: Rect,
    /// Loudness: the readout column left of the panel.
    pub side: Rect,
}

/// The title and close control across the top of every visualizer window.
pub const TITLE_BAR: f32 = 28.0;
/// The window's margin around the panel and readouts.
pub const EDGE: f32 = crate::theme::space::BASE;
/// The value axis right of the panel.
pub const VALUE_GUTTER: f32 = 34.0;
/// The time or frequency axis under the panel.
pub const AXIS_GUTTER: f32 = 18.0;
/// One axis label's line box.
pub const TICK_H: f32 = 12.0;
/// Loudness: the readout column's width.
pub const LOUDNESS_SIDE: f32 = 132.0;
/// Loudness: a big readout's caption and value rows, and the step from one
/// big readout (caption, value, bar) to the next.
pub const READOUT_CAPTION: f32 = 12.0;
pub const READOUT_VALUE: f32 = 22.0;
pub const LOUDNESS_BLOCK: f32 = 46.0;
/// Loudness: the compact caption-and-value rows under the big readouts.
pub const LOUDNESS_ROW: f32 = 17.0;
pub const LOUDNESS_ROWS: usize = 3;
/// Loudness: the reset control's height.
pub const LOUDNESS_RESET_H: f32 = 20.0;
/// A meter bar's thickness.
pub const BAR_H: f32 = 3.0;
/// The balance meter's reach either side of centre.
pub const BALANCE_RANGE_DB: f32 = 12.0;
/// The stereo image's meter strip: caption row, bar, scale row.
pub const STEREO_METER_CAPTION: f32 = 14.0;
pub const STEREO_BAR_OFFSET: f32 = STEREO_METER_CAPTION + 6.0;
pub const STEREO_STRIP: f32 = STEREO_BAR_OFFSET + BAR_H + 3.0 + TICK_H;
/// Room inside the stereo panel for the L, M and R labels round the disc.
const STEREO_LABEL_ROOM: f32 = 16.0;
const STEREO_BASE_PAD: f32 = 6.0;
const STEREO_ARC_SEGMENTS: usize = 72;

impl Layout {
    pub fn new(kind: VisualizerKind, width: f32, height: f32) -> Self {
        use crate::theme::space;
        let (w, h) = (width.max(1.0), height.max(1.0));
        let top = TITLE_BAR;
        let axes = |x: f32| Rect {
            x,
            y: top,
            w: (w - x - VALUE_GUTTER).max(1.0),
            h: (h - top - AXIS_GUTTER).max(1.0),
        };
        let (panel, plot, strip, side) = match kind {
            VisualizerKind::Spectrum
            | VisualizerKind::Spectrogram
            | VisualizerKind::Oscilloscope => {
                let panel = axes(EDGE);
                (panel, panel, Rect::EMPTY, Rect::EMPTY)
            }
            VisualizerKind::Loudness => {
                let side = Rect {
                    x: EDGE,
                    y: top,
                    w: LOUDNESS_SIDE,
                    h: (h - top - EDGE).max(1.0),
                };
                let panel = axes(side.right() + space::LOOSE);
                (panel, panel, Rect::EMPTY, side)
            }
            VisualizerKind::StereoImage => {
                let strip = Rect {
                    x: EDGE,
                    y: (h - EDGE - STEREO_STRIP).max(top),
                    w: (w - EDGE * 2.0).max(1.0),
                    h: STEREO_STRIP,
                };
                let panel = Rect {
                    x: EDGE,
                    y: top,
                    w: strip.w,
                    h: (strip.y - space::LOOSE - top).max(1.0),
                };
                // A half disc (the scope folds its lower half onto the upper),
                // twice as wide as it is tall, so a landscape window fills.
                let radius = (panel.h - STEREO_LABEL_ROOM - STEREO_BASE_PAD)
                    .min((panel.w - STEREO_LABEL_ROOM * 2.0) / 2.0)
                    .max(1.0);
                let base = panel.bottom() - STEREO_BASE_PAD;
                let centre = panel.x + panel.w / 2.0;
                let plot = Rect {
                    x: centre - radius,
                    y: base - radius,
                    w: radius * 2.0,
                    h: radius,
                };
                (panel, plot, strip, Rect::EMPTY)
            }
        };
        Self {
            kind,
            size: [w, h],
            top,
            panel,
            plot,
            strip,
            side,
        }
    }

    /// Stereo image: the correlation (left) and balance (right) meter tracks.
    pub fn stereo_meters(&self) -> [Rect; 2] {
        let s = self.strip;
        let gap = crate::theme::space::BLOCK;
        let half = ((s.w - gap) / 2.0).max(1.0);
        let bar = |x| Rect {
            x,
            y: s.y + STEREO_BAR_OFFSET,
            w: half,
            h: BAR_H,
        };
        [bar(s.x), bar(s.x + half + gap)]
    }

    /// Loudness: where big readout `index` (momentary, short-term) starts.
    pub fn loudness_block(&self, index: usize) -> f32 {
        self.side.y + index as f32 * LOUDNESS_BLOCK
    }

    /// Loudness: the bar under big readout `index`.
    pub fn loudness_bar(&self, index: usize) -> Rect {
        Rect {
            x: self.side.x,
            y: self.loudness_block(index) + READOUT_CAPTION + READOUT_VALUE + 2.0,
            w: self.side.w,
            h: BAR_H,
        }
    }

    /// Loudness: where compact row `index` (integrated, range, true peak)
    /// starts.
    pub fn loudness_row(&self, index: usize) -> f32 {
        self.loudness_block(2) + 2.0 + index as f32 * LOUDNESS_ROW
    }

    /// Loudness: where the reset control starts.
    pub fn loudness_reset(&self) -> f32 {
        self.loudness_row(LOUDNESS_ROWS) + crate::theme::space::BASE
    }

    /// Loudness: seconds before now → x on the history graph.
    pub fn loudness_x(&self, seconds_ago: f32) -> f32 {
        let span = LOUDNESS_HISTORY as f32 * super::analysis::LOUDNESS_HISTORY_STEP;
        self.plot.right() - (seconds_ago / span).clamp(0.0, 1.0) * self.plot.w
    }

    /// Frequency → x on the log axis.
    pub fn freq_x(&self, hz: f32) -> f32 {
        let t = (hz.clamp(MIN_HZ, MAX_HZ) / MIN_HZ).ln() / (MAX_HZ / MIN_HZ).ln();
        self.plot.x + t * self.plot.w
    }

    /// Frequency → y on the spectrogram's vertical log axis (low at the bottom).
    pub fn freq_y(&self, hz: f32) -> f32 {
        let t = (hz.clamp(MIN_HZ, MAX_HZ) / MIN_HZ).ln() / (MAX_HZ / MIN_HZ).ln();
        self.plot.bottom() - t * self.plot.h
    }

    pub fn spectrum_y(&self, db: f32) -> f32 {
        let t = (SPECTRUM_TOP_DB - db.clamp(SPECTRUM_BOTTOM_DB, SPECTRUM_TOP_DB))
            / (SPECTRUM_TOP_DB - SPECTRUM_BOTTOM_DB);
        self.plot.y + t * self.plot.h
    }

    pub fn loudness_y(&self, lufs: f32) -> f32 {
        let t = (LOUDNESS_TOP - lufs.clamp(LOUDNESS_BOTTOM, LOUDNESS_TOP))
            / (LOUDNESS_TOP - LOUDNESS_BOTTOM);
        self.plot.y + t * self.plot.h
    }

    /// Oscilloscope: amplitude (−1..1) → y.
    pub fn scope_y(&self, amplitude: f32) -> f32 {
        self.plot.y + self.plot.h / 2.0 - amplitude.clamp(-1.0, 1.0) * self.plot.h / 2.0
    }

    /// Oscilloscope: milliseconds from the start of the trace → x.
    pub fn scope_x(&self, ms: f32) -> f32 {
        let span = super::analysis::SCOPE_WINDOW_SECONDS * 1_000.0;
        self.plot.x + (ms / span).clamp(0.0, 1.0) * self.plot.w
    }
}

/// Theme colours the scene paints with — the Studio palette only.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// The recessed panel the picture sits on.
    pub background: Rgba,
    /// The window's own surface round the panel: title, gutters, readouts.
    pub chrome: Rgba,
    /// The panel's hairline edge.
    pub frame: Rgba,
    pub grid: Rgba,
    pub grid_major: Rgba,
    pub accent: Rgba,
    pub accent_hi: Rgba,
    pub ink: Rgba,
    pub ink_muted: Rgba,
    pub warn: Rgba,
    pub danger: Rgba,
}

impl Palette {
    pub fn from_theme() -> Self {
        use crate::theme::Colors;
        let with_alpha = |mut color: Rgba, alpha: f32| {
            color.a = alpha;
            color
        };
        Self {
            background: Colors::surface_canvas(),
            chrome: Colors::surface_window(),
            frame: Colors::border_subtle(),
            grid: with_alpha(Colors::text_primary(), 0.045),
            grid_major: with_alpha(Colors::text_primary(), 0.1),
            accent: Colors::accent_primary(),
            accent_hi: Colors::accent_primary_hover(),
            ink: Colors::text_primary(),
            ink_muted: Colors::text_muted(),
            warn: Colors::accent_warning(),
            danger: Colors::accent_danger(),
        }
    }
}

/// View state the scene needs that is not analysis: the vectorscope's gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewState {
    /// Vectorscope magnification, ≥ 1.
    pub scope_gain: f32,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { scope_gain: 1.0 }
    }
}

/// A frame's worth of drawing.
#[derive(Debug, Default)]
pub struct Scene {
    /// Clear to this, or — for a persistent view — `None`, and draw
    /// [`Scene::fade`] over the previous frame instead.
    pub clear: Option<[f32; 4]>,
    /// Translucent background drawn over the previous frame (persistence).
    pub fade: Option<[f32; 4]>,
    /// Spectrogram: draw the history texture into this physical-pixel rect.
    pub spectrogram: Option<[f32; 4]>,
    /// Alpha-blended triangles, drawn after the spectrogram.
    pub triangles: Vec<Vertex>,
    /// Additively blended triangles (vectorscope dots), drawn last.
    pub glow: Vec<Vertex>,
}

fn rgba(color: Rgba, alpha_scale: f32) -> [f32; 4] {
    [
        color.r,
        color.g,
        color.b,
        (color.a * alpha_scale).clamp(0.0, 1.0),
    ]
}

/// Triangle helpers in physical pixels.
struct Builder<'a> {
    out: &'a mut Vec<Vertex>,
    scale: f32,
}

impl Builder<'_> {
    fn tri(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], color: [f32; 4]) {
        for position in [a, b, c] {
            self.out.push(Vertex { position, color });
        }
    }

    fn quad(&mut self, corners: [[f32; 2]; 4], colors: [[f32; 4]; 4]) {
        let [a, b, c, d] = corners;
        let [ca, cb, cc, cd] = colors;
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
            color: cc,
        });
        self.out.push(Vertex {
            position: a,
            color: ca,
        });
        self.out.push(Vertex {
            position: c,
            color: cc,
        });
        self.out.push(Vertex {
            position: d,
            color: cd,
        });
    }

    /// Axis-aligned rectangle given in logical pixels.
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        let s = self.scale;
        let (x0, y0, x1, y1) = (x * s, y * s, (x + w) * s, (y + h) * s);
        self.quad([[x0, y0], [x1, y0], [x1, y1], [x0, y1]], [color; 4]);
    }

    /// A line segment `width` logical pixels wide, endpoints in logical pixels.
    fn line(&mut self, a: [f32; 2], b: [f32; 2], width: f32, color: [f32; 4]) {
        let s = self.scale;
        let (ax, ay, bx, by) = (a[0] * s, a[1] * s, b[0] * s, b[1] * s);
        let (dx, dy) = (bx - ax, by - ay);
        let length = (dx * dx + dy * dy).sqrt();
        if length < 1.0e-4 {
            return;
        }
        let half = width * s * 0.5;
        let (nx, ny) = (-dy / length * half, dx / length * half);
        self.quad(
            [
                [ax + nx, ay + ny],
                [bx + nx, by + ny],
                [bx - nx, by - ny],
                [ax - nx, ay - ny],
            ],
            [color; 4],
        );
    }

    /// A polyline through logical points, each segment its own quad with the
    /// joins filled by a small square so thick lines do not crack.
    fn polyline(&mut self, points: &[[f32; 2]], width: f32, color: [f32; 4]) {
        for pair in points.windows(2) {
            self.line(pair[0], pair[1], width, color);
        }
        if width > 1.2 {
            for point in points.iter().skip(1).take(points.len().saturating_sub(2)) {
                let r = width * 0.5;
                self.rect(point[0] - r, point[1] - r, width, width, color);
            }
        }
    }

    /// The area between a polyline and a baseline, with a vertical gradient.
    fn area(&mut self, points: &[[f32; 2]], baseline: f32, top: [f32; 4], bottom: [f32; 4]) {
        let s = self.scale;
        for pair in points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            self.quad(
                [
                    [a[0] * s, a[1] * s],
                    [b[0] * s, b[1] * s],
                    [b[0] * s, baseline * s],
                    [a[0] * s, baseline * s],
                ],
                [top, top, bottom, bottom],
            );
        }
    }

    fn dot(&mut self, x: f32, y: f32, size: f32, color: [f32; 4]) {
        let s = self.scale;
        let r = size * s * 0.5;
        let (cx, cy) = (x * s, y * s);
        self.quad(
            [
                [cx - r, cy - r],
                [cx + r, cy - r],
                [cx + r, cy + r],
                [cx - r, cy + r],
            ],
            [color; 4],
        );
    }
}

/// Build the frame for `layout.kind`. `scale` is physical pixels per logical
/// pixel.
pub fn build(
    layout: &Layout,
    scale: f32,
    analyzer: &Analyzer,
    view: &ViewState,
    palette: &Palette,
    oscilloscope: &[f32],
) -> Scene {
    let mut scene = Scene::default();
    let background = rgba(palette.background, 1.0);
    let chrome = rgba(palette.chrome, 1.0);
    // The surface is cleared (or, for a persistent view, faded) to the
    // panel's colour, and the window's own surface laid over everything
    // round the panel — so the spectrogram's texture, drawn before the
    // triangles, is not covered, and only the panel keeps a history.
    if layout.kind == VisualizerKind::StereoImage {
        scene.fade = Some([background[0], background[1], background[2], 0.35]);
    } else {
        scene.clear = Some(background);
    }
    surround(&mut scene.triangles, layout, scale, chrome);
    match layout.kind {
        VisualizerKind::Spectrum => {
            spectrum(&mut scene.triangles, layout, scale, analyzer, palette);
        }
        VisualizerKind::Spectrogram => {
            let p = layout.plot;
            scene.spectrogram = Some([p.x * scale, p.y * scale, p.w * scale, p.h * scale]);
            spectrogram_grid(&mut scene.triangles, layout, scale, palette);
        }
        VisualizerKind::StereoImage => {
            stereo_image(&mut scene, layout, scale, analyzer, view, palette);
        }
        VisualizerKind::Loudness => {
            loudness(&mut scene.triangles, layout, scale, analyzer, palette);
        }
        VisualizerKind::Oscilloscope => {
            oscilloscope_trace(&mut scene.triangles, layout, scale, oscilloscope, palette);
        }
    }
    frame(&mut scene.triangles, layout.panel, scale, palette);
    scene
}

/// The window's surface everywhere outside the panel, as four rects.
fn surround(out: &mut Vec<Vertex>, layout: &Layout, scale: f32, color: [f32; 4]) {
    let mut b = Builder { out, scale };
    let [w, h] = layout.size;
    let p = layout.panel;
    b.rect(0.0, 0.0, w, p.y, color);
    b.rect(0.0, p.bottom(), w, (h - p.bottom()).max(0.0), color);
    b.rect(0.0, p.y, p.x, p.h, color);
    b.rect(p.right(), p.y, (w - p.right()).max(0.0), p.h, color);
}

/// The panel's hairline, just outside it so it never covers data. Square:
/// the panel is a content canvas (DESIGN.md, "These must stay square").
fn frame(out: &mut Vec<Vertex>, panel: Rect, scale: f32, palette: &Palette) {
    let mut b = Builder { out, scale };
    let color = rgba(palette.frame, 1.0);
    let (x, y, r, btm) = (panel.x - 1.0, panel.y - 1.0, panel.right(), panel.bottom());
    b.rect(x, y, panel.w + 2.0, 1.0, color);
    b.rect(x, btm, panel.w + 2.0, 1.0, color);
    b.rect(x, panel.y, 1.0, panel.h, color);
    b.rect(r, panel.y, 1.0, panel.h, color);
}

fn grid_color(palette: &Palette, major: bool) -> [f32; 4] {
    rgba(
        if major {
            palette.grid_major
        } else {
            palette.grid
        },
        1.0,
    )
}

fn spectrum(
    out: &mut Vec<Vertex>,
    layout: &Layout,
    scale: f32,
    analyzer: &Analyzer,
    palette: &Palette,
) {
    let mut b = Builder { out, scale };
    let p = layout.plot;
    for hz in FREQ_TICKS.iter().take(FREQ_TICKS.len() - 1) {
        let x = layout.freq_x(*hz);
        b.rect(x, p.y, 1.0, p.h, grid_color(palette, is_major_freq(*hz)));
    }
    for db in SPECTRUM_GRID_DB {
        let y = layout.spectrum_y(db);
        b.rect(p.x, y, p.w, 1.0, grid_color(palette, db == 0.0));
    }
    if !analyzer.live {
        return;
    }
    let curve: Vec<[f32; 2]> = (0..DISPLAY_BINS)
        .map(|i| {
            [
                layout.freq_x(Analyzer::display_bin_hz(i)),
                layout.spectrum_y(analyzer.spectrum[i]),
            ]
        })
        .collect();
    let peaks: Vec<[f32; 2]> = (0..DISPLAY_BINS)
        .map(|i| {
            [
                layout.freq_x(Analyzer::display_bin_hz(i)),
                layout.spectrum_y(analyzer.peaks[i]),
            ]
        })
        .collect();
    b.area(
        &curve,
        p.bottom(),
        rgba(palette.accent, 0.32),
        rgba(palette.accent, 0.04),
    );
    b.polyline(&peaks, 1.0, rgba(palette.ink_muted, 0.55));
    b.polyline(&curve, 1.6, rgba(palette.accent, 1.0));
}

fn spectrogram_grid(out: &mut Vec<Vertex>, layout: &Layout, scale: f32, palette: &Palette) {
    let mut b = Builder { out, scale };
    let p = layout.plot;
    // Over the picture: the decades read through it, the rest barely.
    for hz in FREQ_TICKS.iter().take(FREQ_TICKS.len() - 1) {
        let y = layout.freq_y(*hz);
        b.rect(p.x, y, p.w, 1.0, grid_color(palette, is_major_freq(*hz)));
    }
}

fn stereo_image(
    scene: &mut Scene,
    layout: &Layout,
    scale: f32,
    analyzer: &Analyzer,
    view: &ViewState,
    palette: &Palette,
) {
    use std::f32::consts::{FRAC_1_SQRT_2, PI};
    let p = layout.plot;
    let radius = p.h;
    let (cx, base) = (p.x + p.w / 2.0, p.bottom());
    {
        let mut b = Builder {
            out: &mut scene.triangles,
            scale,
        };
        // Rings at full scale and half, the S axis along the base, and the
        // L, M and R axes out from the centre.
        for (fraction, color) in [(1.0, palette.grid_major), (0.5, palette.grid)] {
            let arc: Vec<[f32; 2]> = (0..=STEREO_ARC_SEGMENTS)
                .map(|i| {
                    let a = PI * i as f32 / STEREO_ARC_SEGMENTS as f32;
                    let r = radius * fraction;
                    [cx + r * a.cos(), base - r * a.sin()]
                })
                .collect();
            b.polyline(&arc, 1.0, rgba(color, 1.0));
        }
        b.line(
            [p.x, base],
            [p.right(), base],
            1.0,
            rgba(palette.grid_major, 1.0),
        );
        for (degrees, color) in [
            (45.0f32, palette.grid),
            (90.0, palette.grid_major),
            (135.0, palette.grid),
        ] {
            let a = degrees.to_radians();
            b.line(
                [cx, base],
                [cx + radius * a.cos(), base - radius * a.sin()],
                1.0,
                rgba(color, 1.0),
            );
        }

        let [correlation, balance] = layout.stereo_meters();
        let value = analyzer.correlation.clamp(-1.0, 1.0);
        meter(
            &mut b,
            correlation,
            if analyzer.live { Some(value) } else { None },
            if value < 0.0 {
                palette.warn
            } else {
                palette.accent
            },
            palette,
        );
        let lean = (analyzer.balance_db / BALANCE_RANGE_DB).clamp(-1.0, 1.0);
        meter(
            &mut b,
            balance,
            if analyzer.live { Some(lean) } else { None },
            palette.accent,
            palette,
        );
    }
    if !analyzer.live {
        return;
    }

    // The trace: consecutive pairs joined, added up so that where the signal
    // dwells the picture brightens. A pair below the S axis (M < 0) is folded
    // through the centre onto the upper half, which keeps its width and its
    // phase; the jump a fold makes is not drawn.
    let mut glow = Builder {
        out: &mut scene.glow,
        scale,
    };
    let k = radius * view.scope_gain * FRAC_1_SQRT_2;
    let trace = rgba(palette.accent, 0.07);
    let longest = radius * 0.3;
    let mut previous: Option<[f32; 2]> = None;
    for [l, r] in analyzer.scope_pairs() {
        let (l, r) = if l + r < 0.0 { (-l, -r) } else { (l, r) };
        let (dx, dy) = ((r - l) * k, (l + r) * k);
        if dx * dx + dy * dy > radius * radius {
            previous = None;
            continue;
        }
        let point = [cx + dx, base - dy];
        if let Some(from) = previous {
            let (jx, jy) = (point[0] - from[0], point[1] - from[1]);
            if jx * jx + jy * jy < longest * longest {
                glow.line(from, point, 1.0, trace);
            }
        }
        previous = Some(point);
    }
}

/// A centre-zero meter: a track with end and centre ticks, a fill from the
/// centre to `value` (−1..1) and a marker at it.
fn meter(b: &mut Builder, track: Rect, value: Option<f32>, fill: Rgba, palette: &Palette) {
    let mid = track.x + track.w / 2.0;
    b.rect(
        track.x,
        track.y,
        track.w,
        track.h,
        rgba(palette.grid_major, 1.5),
    );
    for x in [track.x, mid - 0.5, track.right() - 1.0] {
        b.rect(
            x,
            track.y - 2.0,
            1.0,
            track.h + 4.0,
            rgba(palette.grid_major, 3.0),
        );
    }
    let Some(value) = value else {
        return;
    };
    let at = mid + value.clamp(-1.0, 1.0) * track.w / 2.0;
    let (x0, x1) = if at < mid { (at, mid) } else { (mid, at) };
    if x1 - x0 > 0.5 {
        b.rect(x0, track.y, x1 - x0, track.h, rgba(fill, 0.85));
    }
    b.rect(
        at - 1.0,
        track.y - 3.0,
        2.0,
        track.h + 6.0,
        rgba(palette.ink, 1.0),
    );
}

/// The vectorscope magnification that puts the loudest recent pair near 80 %
/// of the radius, clamped to ×1..×32 and eased so it does not pump.
pub fn next_scope_gain(current: f32, analyzer: &Analyzer, dt: f32) -> f32 {
    // A pair lands √(l² + r²) radii from the centre at ×1 (see `stereo_image`).
    let loudest = analyzer
        .scope_pairs()
        .map(|[l, r]| (l * l + r * r).sqrt())
        .fold(0.0f32, f32::max);
    if loudest < 1.0e-4 {
        return current;
    }
    let target = (0.8 / loudest).clamp(1.0, 32.0);
    let rate = if target < current { 8.0 } else { 1.0 };
    current + (target - current) * (1.0 - (-dt * rate).exp())
}

fn loudness(
    out: &mut Vec<Vertex>,
    layout: &Layout,
    scale: f32,
    analyzer: &Analyzer,
    palette: &Palette,
) {
    let mut b = Builder { out, scale };
    let p = layout.plot;
    for lufs in LOUDNESS_GRID {
        let y = layout.loudness_y(lufs);
        b.rect(p.x, y, p.w, 1.0, grid_color(palette, false));
    }
    for seconds in LOUDNESS_TIME_GRID {
        let x = layout.loudness_x(seconds);
        b.rect(x, p.y, 1.0, p.h, grid_color(palette, false));
    }
    let target = layout.loudness_y(LOUDNESS_TARGET);
    b.rect(p.x, target, p.w, 1.0, rgba(palette.warn, 0.6));

    // Momentary and short-term as bars under their readouts in the side
    // column (the overlay prints the numbers above them), on the graph's own
    // scale, with the target marked across each.
    for (index, value) in [analyzer.momentary_lufs, analyzer.short_term_lufs]
        .into_iter()
        .enumerate()
    {
        let bar = layout.loudness_bar(index);
        b.rect(bar.x, bar.y, bar.w, bar.h, rgba(palette.grid_major, 1.6));
        let fraction = |lufs: f32| {
            ((lufs - LOUDNESS_BOTTOM) / (LOUDNESS_TOP - LOUDNESS_BOTTOM)).clamp(0.0, 1.0)
        };
        if let Some(lufs) = value {
            let color = if lufs > LOUDNESS_TARGET {
                palette.warn
            } else {
                palette.accent
            };
            b.rect(
                bar.x,
                bar.y,
                bar.w * fraction(lufs),
                bar.h,
                rgba(color, 1.0),
            );
        }
        let at = bar.x + bar.w * fraction(LOUDNESS_TARGET);
        b.rect(at, bar.y - 2.0, 1.0, bar.h + 4.0, rgba(palette.warn, 0.8));
    }

    let history = &analyzer.loudness_history;
    if history.len() < 2 {
        return;
    }
    let step = p.w / (LOUDNESS_HISTORY - 1) as f32;
    let first = LOUDNESS_HISTORY - history.len();
    let mut segment: Vec<[f32; 2]> = Vec::new();
    let flush = |segment: &mut Vec<[f32; 2]>, b: &mut Builder| {
        if segment.len() > 1 {
            b.area(
                segment,
                p.bottom(),
                rgba(palette.accent, 0.22),
                rgba(palette.accent, 0.02),
            );
            // Two pixels: the history is denser than the panel's pixels, and
            // a 1.5 px line over it alternates one and two rows into dashes.
            b.polyline(segment, 2.0, rgba(palette.accent, 1.0));
        }
        segment.clear();
    };
    for (i, lufs) in history.iter().enumerate() {
        if lufs.is_finite() {
            segment.push([p.x + (first + i) as f32 * step, layout.loudness_y(*lufs)]);
        } else {
            flush(&mut segment, &mut b);
        }
    }
    flush(&mut segment, &mut b);
}

fn oscilloscope_trace(
    out: &mut Vec<Vertex>,
    layout: &Layout,
    scale: f32,
    trace: &[f32],
    palette: &Palette,
) {
    let mut b = Builder { out, scale };
    let p = layout.plot;
    for level in [-0.5f32, 0.5] {
        b.rect(
            p.x,
            layout.scope_y(level),
            p.w,
            1.0,
            grid_color(palette, false),
        );
    }
    let span_ms = super::analysis::SCOPE_WINDOW_SECONDS * 1_000.0;
    let mut ms = SCOPE_DIVISION_MS;
    while ms < span_ms - 0.5 {
        b.rect(
            layout.scope_x(ms),
            p.y,
            1.0,
            p.h,
            grid_color(palette, false),
        );
        ms += SCOPE_DIVISION_MS;
    }
    b.rect(
        p.x,
        layout.scope_y(0.0),
        p.w,
        1.0,
        grid_color(palette, true),
    );
    if trace.len() < 2 {
        return;
    }
    let step = p.w / (trace.len() - 1) as f32;
    let points: Vec<[f32; 2]> = trace
        .iter()
        .enumerate()
        .map(|(i, v)| [p.x + i as f32 * step, layout.scope_y(*v)])
        .collect();
    b.polyline(&points, 1.5, rgba(palette.accent, 1.0));
}

/// Spectrogram colour for a 0..1 level: the background, through the accent,
/// to the accent's highlight and finally the ink — one hue, lit by value.
pub fn spectrogram_lut(palette: &Palette) -> [[u8; 4]; 256] {
    let stops = [
        (0.0, palette.background),
        (0.35, mix(palette.background, palette.accent, 0.35)),
        (0.7, palette.accent),
        (0.88, palette.accent_hi),
        (1.0, palette.ink),
    ];
    let mut lut = [[0u8; 4]; 256];
    for (i, entry) in lut.iter_mut().enumerate() {
        let t = i as f32 / 255.0;
        let upper = stops
            .iter()
            .position(|(at, _)| *at >= t)
            .unwrap_or(stops.len() - 1)
            .max(1);
        let (a_at, a) = stops[upper - 1];
        let (b_at, b) = stops[upper];
        let f = ((t - a_at) / (b_at - a_at).max(1.0e-6)).clamp(0.0, 1.0);
        let c = mix(a, b, f);
        *entry = [
            (c.r * 255.0).round() as u8,
            (c.g * 255.0).round() as u8,
            (c.b * 255.0).round() as u8,
            255,
        ];
    }
    lut
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::super::analysis::Features;
    use super::*;

    fn palette() -> Palette {
        let c = |r, g, b| Rgba { r, g, b, a: 1.0 };
        Palette {
            background: c(0.05, 0.05, 0.06),
            chrome: c(0.09, 0.09, 0.12),
            frame: Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.08,
            },
            grid: Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.05,
            },
            grid_major: Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.1,
            },
            accent: c(0.18, 0.79, 0.84),
            accent_hi: c(0.33, 0.85, 0.89),
            ink: c(0.92, 0.93, 0.95),
            ink_muted: c(0.56, 0.58, 0.62),
            warn: c(0.91, 0.72, 0.36),
            danger: c(0.95, 0.39, 0.37),
        }
    }

    #[test]
    fn the_axes_map_the_ends_of_their_ranges_to_the_plot_edges() {
        let layout = Layout::new(VisualizerKind::Spectrum, 400.0, 240.0);
        assert!((layout.freq_x(MIN_HZ) - layout.plot.x).abs() < 1.0e-3);
        assert!((layout.freq_x(MAX_HZ) - layout.plot.right()).abs() < 1.0e-3);
        assert!((layout.spectrum_y(SPECTRUM_TOP_DB) - layout.plot.y).abs() < 1.0e-3);
        assert!((layout.spectrum_y(-200.0) - layout.plot.bottom()).abs() < 1.0e-3);
        let spectrogram = Layout::new(VisualizerKind::Spectrogram, 400.0, 240.0);
        assert!(spectrogram.freq_y(MIN_HZ) > spectrogram.freq_y(MAX_HZ));
    }

    #[test]
    fn a_stereo_layout_keeps_the_half_disc_between_the_title_and_the_meters() {
        for (w, h) in [(420.0, 260.0), (260.0, 420.0), (240.0, 180.0)] {
            let layout = Layout::new(VisualizerKind::StereoImage, w, h);
            assert!((layout.plot.w - 2.0 * layout.plot.h).abs() < 1.0e-3);
            assert!(layout.plot.y >= TITLE_BAR);
            assert!(layout.plot.x >= 0.0 && layout.plot.right() <= w);
            assert!(layout.plot.bottom() <= layout.strip.y + 1.0e-3);
            assert!(layout.strip.bottom() <= h);
            let [correlation, balance] = layout.stereo_meters();
            assert!(correlation.right() < balance.x);
            assert!(balance.right() <= layout.strip.right() + 1.0e-3);
        }
    }

    /// At each view's smallest window the panel, its gutters and the loudness
    /// readouts (down to the reset control) all stay on the surface.
    #[test]
    fn every_view_fits_its_smallest_window() {
        for kind in VisualizerKind::ALL {
            let (w, h) = super::super::window::min_size(kind);
            let layout = Layout::new(kind, w, h);
            let p = layout.panel;
            assert!(p.x >= EDGE && p.y >= TITLE_BAR, "{kind:?}");
            assert!(p.right() <= w && p.bottom() <= h, "{kind:?}");
            assert!(p.w > 100.0 && p.h > 80.0, "{kind:?} panel {p:?}");
            if kind == VisualizerKind::Loudness {
                assert!(
                    layout.loudness_reset() + LOUDNESS_RESET_H <= layout.side.bottom(),
                    "the reset control is cut off"
                );
                assert!(layout.side.right() < p.x);
            }
        }
    }

    #[test]
    fn every_plot_starts_below_the_title_bar() {
        for kind in VisualizerKind::ALL {
            assert!(
                Layout::new(kind, 420.0, 260.0).plot.y >= TITLE_BAR,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn every_view_builds_whole_triangles_inside_the_surface() {
        let mut analyzer = Analyzer::new(Features {
            spectrum: true,
            loudness: true,
        });
        let tone: Vec<f32> = (0..48_000)
            .map(|i| 0.5 * (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        for chunk in tone.chunks(800) {
            analyzer.push(chunk, chunk);
        }
        let mut trace = Vec::new();
        analyzer.oscilloscope(&mut trace);
        let (w, h, scale) = (420.0, 260.0, 1.5);
        for kind in VisualizerKind::ALL {
            let layout = Layout::new(kind, w, h);
            let scene = build(
                &layout,
                scale,
                &analyzer,
                &ViewState::default(),
                &palette(),
                &trace,
            );
            assert_eq!(scene.triangles.len() % 3, 0, "{kind:?}");
            assert_eq!(scene.glow.len() % 3, 0, "{kind:?}");
            assert!(scene.clear.is_some() != scene.fade.is_some(), "{kind:?}");
            for vertex in scene.triangles.iter().chain(&scene.glow) {
                let [x, y] = vertex.position;
                assert!(x.is_finite() && y.is_finite());
                assert!(
                    x >= -2.0 * scale && x <= (w + 2.0) * scale,
                    "{kind:?} x={x}"
                );
                assert!(
                    y >= -2.0 * scale && y <= (h + 2.0) * scale,
                    "{kind:?} y={y}"
                );
            }
        }
    }

    #[test]
    fn the_spectrogram_ramp_runs_from_the_background_to_the_ink() {
        let palette = palette();
        let lut = spectrogram_lut(&palette);
        let to = |c: Rgba| {
            [
                (c.r * 255.0).round() as u8,
                (c.g * 255.0).round() as u8,
                (c.b * 255.0).round() as u8,
                255,
            ]
        };
        assert_eq!(lut[0], to(palette.background));
        assert_eq!(lut[255], to(palette.ink));
    }
}
