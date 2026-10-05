//! The view of a built-in time effect's native editor (VerbSpace or
//! EchoSpace): a header, a toolbar, two displays and a row of knob cards.
//!
//! Pure rendering over [`FxEditorWindow`]: every edit goes back through the
//! window's `*_cb` builders. The displays paint the window's cached
//! [`FxView`] — the DSP crates' own models — and never compute anything per
//! frame.

use gpui::{
    canvas, div, fill, px, relative, AnyElement, Bounds, Context, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Rgba, Styled, Window,
};

use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::eq_graph::{freq_fraction, paint_area, paint_line, FREQ_GRID};
use crate::components::fx_model::{default_value, knob, FxParams};
use crate::components::fx_window::{curve_hz, FxEditorWindow, FxView, CURVE_POINTS, ECHO_FLOOR_DB};
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::quick_sampler_panel::{knob_cell, rect, KNOB_SIZE};
use crate::theme::{radius, space, state, typography, Colors};

type Cx<'a> = Context<'a, FxEditorWindow>;

/// The axis label row under a display.
const AXIS_H: f32 = 16.0;
const DISPLAY_MIN_H: f32 = 150.0;
/// A knob cell's width plus the gap after it, for sizing a card.
const KNOB_PITCH: f32 = 58.0 + space::TIGHT;
/// Floor of the decay display, in dB.
const DECAY_FLOOR_DB: f32 = -60.0;
/// Floor of the tone display, in dB.
const TONE_FLOOR_DB: f32 = -36.0;

/// The whole editor.
pub(crate) fn fx_panel(w: &FxEditorWindow, cx: &mut Cx) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(w, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .p(px(space::LOOSE))
                .gap(px(space::BASE))
                .child(toolbar(w, cx))
                .child(displays(w))
                .child(cards(w, cx)),
        )
        .into_any_element()
}

// ── Small pieces ─────────────────────────────────────────────────────────────

fn segment_position(index: usize, count: usize) -> FbSegment {
    match index {
        0 => FbSegment::First,
        i if i + 1 == count => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

fn caption(text: impl Into<String>) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text.into())
        .into_any_element()
}

/// A cluster of related controls on one row.
fn cluster() -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(space::TIGHT))
}

/// The width a segmented track needs for labels: segments split a track
/// equally, so each is as wide as the widest label.
fn track_width<S: AsRef<str>>(labels: &[S]) -> f32 {
    let widest = labels
        .iter()
        .map(|label| (label.as_ref().chars().count() as f32 * 8.5 + 2.0 * space::BASE).max(44.0))
        .fold(0.0f32, f32::max);
    widest * labels.len() as f32 + 2.0 * space::TIGHT + 2.0
}

/// A segmented choice over `labels`, `selected` lit, `pick(i)` on a click.
fn choice(
    w: &FxEditorWindow,
    cx: &mut Cx,
    id: &'static str,
    labels: &[&'static str],
    selected: usize,
    pick: impl Fn(&mut FxEditorWindow, usize, &mut Cx) + Copy + 'static,
) -> AnyElement {
    let mut track = fb_segmented_track();
    for (index, label) in labels.iter().enumerate() {
        track = track.child(fb_segment(
            (id, index),
            *label,
            index == selected,
            segment_position(index, labels.len()),
            w.click_cb(cx, move |this, cx| pick(this, index, cx)),
        ));
    }
    track.w(px(track_width(labels))).into_any_element()
}

// ── Header ─────────────────────────────────────────────────────────────────

fn header(w: &FxEditorWindow, cx: &mut Cx) -> AnyElement {
    let kind = w.kind();
    let key = kind.key();
    let open_menu = {
        let entity = cx.entity().clone();
        move |event: &MouseDownEvent, _: &mut Window, app: &mut gpui::App| {
            let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
            let _ = entity.update(app, |this, cx| this.open_preset_menu(x, y, cx));
        }
    };
    let preset_picker = div()
        .id((key, 0usize))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .w(px(180.0))
        .h(px(26.0))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::button_border())
        .bg(Colors::surface_canvas())
        .hover(|style| style.border_color(Colors::border_normal()))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::UI_SM))
        .text_color(Colors::text_primary())
        .child(div().flex_1().truncate().child(w.preset_label()))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child("▾"),
        )
        .on_mouse_down(MouseButton::Left, open_menu);

    let presets = cluster()
        .child(fb_button(
            (key, 1usize),
            "‹",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.step_preset(-1, cx)),
        ))
        .child(preset_picker)
        .child(fb_button(
            (key, 2usize),
            "›",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.step_preset(1, cx)),
        ))
        .child(fb_button(
            (key, 3usize),
            "Reset",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.load_preset(0, cx)),
        ));

    let on_b = w.compare.on_b;
    let compare = cluster()
        .child(choice(
            w,
            cx,
            "fx-compare",
            &["A", "B"],
            usize::from(on_b),
            |this, index, cx| {
                if (index == 1) != this.compare.on_b {
                    this.swap_compare(cx);
                }
            },
        ))
        .child(fb_button(
            (key, 4usize),
            if on_b { "Copy B → A" } else { "Copy A → B" },
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.copy_compare(cx)),
        ));

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::LOOSE))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .child(
                    div()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_primary())
                        .child(kind.title()),
                )
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(kind.subtitle()),
                ),
        )
        .child(presets)
        .child(compare)
        .child(div().flex_1())
        .child(fb_checkbox(
            (key, 5usize),
            "Power",
            w.params.power(),
            true,
            w.click_cb(cx, |this, cx| this.toggle("power", cx)),
        ))
        .into_any_element()
}

// ── Toolbar ────────────────────────────────────────────────────────────────

fn toolbar(w: &FxEditorWindow, cx: &mut Cx) -> AnyElement {
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE));
    let freeze = fb_checkbox(
        (w.kind().key(), 10usize),
        "Freeze",
        w.params.flag("freeze"),
        true,
        w.click_cb(cx, |this, cx| this.toggle("freeze", cx)),
    );
    match &w.params {
        FxParams::Verb(p) => {
            let labels = verbspace::ReverbMode::ALL.map(|mode| mode.label());
            let selected = verbspace::ReverbMode::ALL
                .iter()
                .position(|mode| *mode == p.mode)
                .unwrap_or(0);
            row.child(caption("MODE"))
                .child(choice(
                    w,
                    cx,
                    "verb-mode",
                    &labels,
                    selected,
                    |this, i, cx| {
                        this.set_value("mode", verbspace::ReverbMode::ALL[i].to_wire(), cx)
                    },
                ))
                .child(div().flex_1())
                .child(freeze)
                .into_any_element()
        }
        FxParams::Echo(p) => {
            let labels = echospace::DelayMode::ALL.map(|mode| mode.label());
            let selected = echospace::DelayMode::ALL
                .iter()
                .position(|mode| *mode == p.mode)
                .unwrap_or(0);
            let tempo = p.sync.then(|| caption(format!("{:.1} BPM", w.tempo_bpm())));
            row.child(caption("MODE"))
                .child(choice(
                    w,
                    cx,
                    "echo-mode",
                    &labels,
                    selected,
                    |this, i, cx| {
                        this.set_value("mode", echospace::DelayMode::ALL[i].to_wire(), cx)
                    },
                ))
                .child(fb_checkbox(
                    "echo-sync",
                    "Sync",
                    p.sync,
                    true,
                    w.click_cb(cx, |this, cx| this.toggle("sync", cx)),
                ))
                .child(fb_checkbox(
                    "echo-link",
                    "Link L/R",
                    p.link,
                    p.mode.uses_right_time(),
                    w.click_cb(cx, |this, cx| this.toggle("link", cx)),
                ))
                .children(tempo)
                .child(div().flex_1())
                .child(freeze)
                .into_any_element()
        }
    }
}

// ── Displays ───────────────────────────────────────────────────────────────

/// Every colour the displays need, resolved before painting.
#[derive(Clone, Copy)]
struct Palette {
    background: Rgba,
    grid: Rgba,
    grid_major: Rgba,
    axis: Rgba,
    signal: Rgba,
    signal_fill: Rgba,
    low: Rgba,
    high: Rgba,
    left: Rgba,
    right: Rgba,
    early: Rgba,
    shade: Rgba,
    dim: f32,
}

impl Palette {
    fn resolve(bypassed: bool) -> Self {
        let ink = Colors::text_primary();
        Self {
            background: Colors::surface_canvas(),
            grid: Colors::with_alpha(ink, 0.045),
            grid_major: Colors::with_alpha(ink, 0.09),
            axis: Colors::with_alpha(ink, 0.18),
            signal: Colors::accent_primary(),
            signal_fill: Colors::accent_primary(),
            low: Colors::track_audio(),
            high: Colors::accent_warning(),
            left: Colors::accent_primary(),
            right: Colors::track_instrument(),
            early: Colors::text_secondary(),
            shade: Colors::surface_window(),
            dim: if bypassed { 0.35 } else { 1.0 },
        }
    }
}

fn displays(w: &FxEditorWindow) -> AnyElement {
    let bypassed = !w.params.power();
    let palette = Palette::resolve(bypassed);
    let row = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(DISPLAY_MIN_H + AXIS_H))
        .gap(px(space::BASE))
        .opacity(if bypassed { 0.75 } else { 1.0 });
    let notice = w.notice().map(str::to_string).or_else(|| {
        bypassed.then(|| {
            format!(
                "Bypassed — {} passes audio through unchanged",
                w.kind().title()
            )
        })
    });
    match w.view.as_ref() {
        FxView::Verb { profile, rt, cuts } => {
            let span = decay_span(profile);
            let profile = *profile;
            let rt = rt.clone();
            let cuts = cuts.clone();
            let rt_top = nice_ceiling(
                rt.iter()
                    .copied()
                    .filter(|s| s.is_finite())
                    .fold(0.5, f32::max)
                    * 1.15,
            );
            let legend = if profile.frozen {
                "Frozen — the tail holds".to_string()
            } else {
                format!(
                    "Low {} · Mid {} · High {}",
                    seconds(profile.rt_low_sec),
                    seconds(profile.rt_mid_sec),
                    seconds(profile.rt_high_sec)
                )
            };
            row.child(display(
                "DECAY",
                Some(legend),
                notice,
                3.0,
                time_axis(span),
                move |bounds, window| paint_decay(window, bounds, &profile, span, &palette),
            ))
            .child(display(
                "DECAY BY FREQUENCY",
                Some(format!("RT60, 0–{}", seconds(rt_top))),
                None,
                2.0,
                freq_axis(),
                move |bounds, window| paint_rt(window, bounds, &rt, &cuts, rt_top, &palette),
            ))
            .into_any_element()
        }
        FxView::Echo {
            echoes,
            tones,
            times_ms,
            tempo_bpm,
        } => {
            let span = repeat_span(echoes, *times_ms);
            let echoes = echoes.clone();
            let tones = tones.clone();
            let FxParams::Echo(p) = &w.params else {
                return row.into_any_element();
            };
            let beat_ms = p.sync.then(|| 60_000.0 / tempo_bpm.max(1.0));
            let legend = if p.mode == echospace::DelayMode::Mono {
                format!("{}", ms(times_ms.0))
            } else {
                format!("L {} · R {}", ms(times_ms.0), ms(times_ms.1))
            };
            let legend = if p.sync {
                let label = |d: u8| echospace::DIVISION_LABELS[(d as usize).min(17)];
                if p.mode == echospace::DelayMode::Mono {
                    format!("{} — {legend}", label(p.division_l))
                } else {
                    format!(
                        "{} · {} — {legend}",
                        label(p.division_l),
                        label(p.division_r)
                    )
                }
            } else {
                legend
            };
            let mono = p.mode == echospace::DelayMode::Mono;
            row.child(display(
                "REPEATS",
                Some(legend),
                notice,
                3.0,
                time_axis(span),
                move |bounds, window| {
                    paint_repeats(window, bounds, &echoes, span, beat_ms, mono, &palette)
                },
            ))
            .child(display(
                "TONE PER PASS",
                Some("1 · 2 · 4 · 8 passes".to_string()),
                None,
                2.0,
                freq_axis(),
                move |bounds, window| paint_tone(window, bounds, &tones, &palette),
            ))
            .into_any_element()
        }
    }
}

/// A framed display: `paint` over a canvas, a caption and an optional legend
/// over its top edge, a passing notice along its bottom, and `axis` under it.
fn display(
    title: &'static str,
    legend: Option<String>,
    notice: Option<String>,
    grow: f32,
    axis: AnyElement,
    paint: impl Fn(Bounds<Pixels>, &mut Window) + 'static,
) -> AnyElement {
    let plot_canvas = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint(bounds, window),
    )
    .absolute()
    .size_full();
    let tag = |text: String| {
        div()
            .px(px(space::SNUG))
            .py(px(space::HAIR))
            .rounded(px(radius::CONTROL_SM))
            .bg(Colors::with_alpha(Colors::surface_base(), 0.88))
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(Colors::text_secondary())
            .child(text)
    };
    let top = div()
        .absolute()
        .top(px(space::SNUG))
        .left(px(space::SNUG))
        .right(px(space::SNUG))
        .flex()
        .flex_row()
        .justify_between()
        .child(tag(title.to_string()))
        .children(legend.map(tag));
    let notice = notice.map(|text| {
        div()
            .absolute()
            .bottom(px(space::LOOSE))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .px(px(space::BASE))
                    .py(px(space::TIGHT))
                    .rounded(px(radius::CONTROL))
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .bg(Colors::with_alpha(Colors::surface_base(), 0.92))
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_secondary())
                    .child(text),
            )
    });
    let mut column = div().flex().flex_col().flex_basis(px(0.0)).min_w(px(220.0));
    column.style().flex_grow = Some(grow);
    column
        .child(
            div()
                .relative()
                .flex_1()
                .min_h(px(DISPLAY_MIN_H))
                .overflow_hidden()
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(plot_canvas)
                .child(top)
                .children(notice),
        )
        .child(axis)
        .into_any_element()
}

/// A row of labels at fractions across a display.
fn axis_row(labels: Vec<(f32, String)>) -> AnyElement {
    div()
        .relative()
        .h(px(AXIS_H))
        .flex_shrink_0()
        .children(labels.into_iter().map(|(fraction, label)| {
            div()
                .absolute()
                .left(relative(fraction))
                .ml(px(-20.0))
                .w(px(40.0))
                .flex()
                .justify_center()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(label)
        }))
        .into_any_element()
}

/// The frequencies a narrow display labels.
const FREQ_TICKS: [(f32, &str); 5] = [
    (100.0, "100"),
    (300.0, "300"),
    (1_000.0, "1k"),
    (3_000.0, "3k"),
    (10_000.0, "10k"),
];

fn freq_axis() -> AnyElement {
    axis_row(
        FREQ_TICKS
            .iter()
            .map(|(hz, label)| (freq_fraction(*hz), label.to_string()))
            .collect(),
    )
}

/// Tick labels for a time axis `span_ms` long.
fn time_axis(span_ms: f32) -> AnyElement {
    let step = nice_step(span_ms, 5);
    let labels = (1..)
        .map(|i| i as f32 * step)
        .take_while(|t| *t < span_ms * 0.999)
        .map(|t| (t / span_ms, ms(t)))
        .collect();
    axis_row(labels)
}

/// The decay display's time span: past the slowest band's RT60, rounded to
/// a tidy figure.
fn decay_span(profile: &verbspace::DecayProfile) -> f32 {
    let tail = if profile.frozen {
        4.0
    } else {
        profile.longest_sec()
    };
    nice_ceiling((profile.predelay_ms + tail * 1_000.0 * 1.08).clamp(200.0, 45_000.0))
}

/// The repeat display's span: past the last repeat drawn.
fn repeat_span(echoes: &[echospace::Echo], times_ms: (f32, f32)) -> f32 {
    let last = echoes.iter().map(|e| e.at_ms).fold(0.0, f32::max);
    let floor = times_ms.0.max(times_ms.1) * 2.0;
    nice_ceiling((last.max(floor) * 1.06).clamp(100.0, 30_000.0))
}

/// 1, 2 or 5 times a power of ten, giving about `ticks` steps across `span`.
fn nice_step(span: f32, ticks: usize) -> f32 {
    let raw = span / ticks.max(1) as f32;
    let magnitude = 10f32.powf(raw.log10().floor());
    let scaled = raw / magnitude;
    let nice = if scaled < 1.5 {
        1.0
    } else if scaled < 3.5 {
        2.0
    } else if scaled < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// `value` rounded up to the next tidy figure on a 1-2-5 ladder.
fn nice_ceiling(value: f32) -> f32 {
    let magnitude = 10f32.powf(value.max(1.0e-3).log10().floor());
    [1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0]
        .iter()
        .map(|m| m * magnitude)
        .find(|v| *v >= value)
        .unwrap_or(value)
}

fn ms(value: f32) -> String {
    if value >= 1_000.0 {
        let s = value / 1_000.0;
        if (s - s.round()).abs() < 1.0e-3 {
            format!("{s:.0} s")
        } else if s >= 10.0 {
            format!("{s:.1} s")
        } else {
            format!("{s:.2} s")
        }
    } else {
        format!("{value:.0} ms")
    }
}

fn seconds(value: f32) -> String {
    if !value.is_finite() {
        "∞".to_string()
    } else if value < 10.0 {
        format!("{value:.2} s")
    } else {
        format!("{value:.1} s")
    }
}

fn frame(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    )
}

/// The frequency grid every frequency display shares with the EQs.
fn paint_freq_grid(window: &mut Window, bounds: Bounds<Pixels>, p: &Palette) {
    let (x0, y0, w, h) = frame(bounds);
    for hz in FREQ_GRID {
        let major = [100.0, 1_000.0, 10_000.0].contains(&hz);
        let x = x0 + freq_fraction(hz) * w;
        window.paint_quad(fill(
            rect(x, y0, 1.0, h),
            if major { p.grid_major } else { p.grid },
        ));
    }
}

fn paint_time_grid(window: &mut Window, bounds: Bounds<Pixels>, span_ms: f32, p: &Palette) {
    let (x0, y0, w, h) = frame(bounds);
    let step = nice_step(span_ms, 5);
    let mut t = step;
    while t < span_ms {
        window.paint_quad(fill(rect(x0 + t / span_ms * w, y0, 1.0, h), p.grid_major));
        t += step;
    }
}

/// The reverb's level over time: the pre-delay, the early reflections, and
/// each band's decay down to −60 dB.
fn paint_decay(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    profile: &verbspace::DecayProfile,
    span_ms: f32,
    p: &Palette,
) {
    let (x0, y0, w, h) = frame(bounds);
    window.paint_quad(fill(bounds, p.background));
    paint_time_grid(window, bounds, span_ms, p);
    for db in [-12.0, -24.0, -36.0, -48.0] {
        let y = y0 + db / DECAY_FLOOR_DB * h;
        window.paint_quad(fill(rect(x0, y, w, 1.0), p.grid));
    }
    let x_at = |t_ms: f32| x0 + (t_ms / span_ms).clamp(0.0, 1.0) * w;
    let y_at = |db: f32| y0 + (db / DECAY_FLOOR_DB).clamp(0.0, 1.0) * h;

    // The pre-delay: nothing yet.
    if profile.predelay_ms > 0.0 {
        window.paint_quad(fill(
            rect(x0, y0, x_at(profile.predelay_ms) - x0, h),
            Colors::with_alpha(p.shade, 0.55),
        ));
    }

    // The tail of each band, from the first late arrival.
    let onset = profile.predelay_ms + profile.first_late_ms;
    let line_of = |rt_sec: f32| -> Vec<(f32, f32)> {
        if !rt_sec.is_finite() {
            return vec![(x_at(onset), y_at(-3.0)), (x0 + w, y_at(-3.0))];
        }
        let end = onset + rt_sec * 1_000.0;
        let mut points = vec![(x_at(onset), y_at(-3.0))];
        if end <= span_ms {
            points.push((x_at(end), y_at(DECAY_FLOOR_DB)));
        } else {
            let db = -3.0 + (DECAY_FLOOR_DB + 3.0) * (span_ms - onset) / (end - onset);
            points.push((x0 + w, y_at(db)));
        }
        points
    };
    let mid = line_of(profile.rt_mid_sec);
    let floor_y = y0 + h;
    paint_area(
        window,
        &mid,
        floor_y,
        Colors::with_alpha(p.signal_fill, 0.12 * p.dim),
    );
    paint_line(
        window,
        &line_of(profile.rt_low_sec),
        1.2,
        Colors::with_alpha(p.low, 0.85 * p.dim),
    );
    paint_line(
        window,
        &line_of(profile.rt_high_sec),
        1.2,
        Colors::with_alpha(p.high, 0.85 * p.dim),
    );
    paint_line(window, &mid, 2.0, Colors::with_alpha(p.signal, p.dim));

    // Early reflections, as ticks from the floor: left a touch to the left
    // of right, so coincident pairs both show.
    for reflection in profile.early.iter().filter(|e| e.gain != 0.0) {
        let db = 20.0 * reflection.gain.abs().max(1.0e-6).log10();
        let x = x_at(reflection.at_ms) + if reflection.right { 1.0 } else { -1.0 };
        let top = y_at(db.max(DECAY_FLOOR_DB));
        let color = if reflection.right { p.right } else { p.left };
        window.paint_quad(fill(
            rect(x, top, 1.5, floor_y - top),
            Colors::with_alpha(color, 0.7 * p.dim),
        ));
    }
    window.paint_quad(fill(rect(x0, y_at(0.0), w, 1.0), p.axis));
}

/// RT60 across frequency, with the wet cuts shading what never leaves.
fn paint_rt(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    rt: &[f32],
    cuts: &[f32],
    top_sec: f32,
    p: &Palette,
) {
    let (x0, y0, w, h) = frame(bounds);
    window.paint_quad(fill(bounds, p.background));
    paint_freq_grid(window, bounds, p);
    let column = w / (CURVE_POINTS - 1) as f32;
    for (i, db) in cuts.iter().enumerate() {
        let amount = (-db / 24.0).clamp(0.0, 1.0);
        if amount > 0.02 {
            let x = x0 + i as f32 * column - column * 0.5;
            window.paint_quad(fill(
                rect(x, y0, column + 0.5, h),
                Colors::with_alpha(p.shade, 0.75 * amount),
            ));
        }
    }
    let points: Vec<(f32, f32)> = rt
        .iter()
        .enumerate()
        .map(|(i, sec)| {
            let level = if sec.is_finite() { sec / top_sec } else { 1.0 };
            (
                x0 + freq_fraction(curve_hz(i)) * w,
                y0 + h - level.clamp(0.0, 1.0) * (h - 4.0),
            )
        })
        .collect();
    paint_area(
        window,
        &points,
        y0 + h,
        Colors::with_alpha(p.signal_fill, 0.12 * p.dim),
    );
    paint_line(window, &points, 2.0, Colors::with_alpha(p.signal, p.dim));
}

/// The repeats: left above the centre line, right below, each as tall as it
/// is loud; the beat grid behind them while synced.
fn paint_repeats(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    echoes: &[echospace::Echo],
    span_ms: f32,
    beat_ms: Option<f32>,
    mono: bool,
    p: &Palette,
) {
    let (x0, y0, w, h) = frame(bounds);
    window.paint_quad(fill(bounds, p.background));
    match beat_ms {
        Some(beat) if span_ms / beat < 96.0 => {
            let mut index = 1;
            while index as f32 * beat < span_ms {
                let x = x0 + index as f32 * beat / span_ms * w;
                let bar = index % 4 == 0;
                window.paint_quad(fill(
                    rect(x, y0, 1.0, h),
                    if bar { p.grid_major } else { p.grid },
                ));
                index += 1;
            }
        }
        _ => paint_time_grid(window, bounds, span_ms, p),
    }
    let centre = y0 + h * 0.5;
    window.paint_quad(fill(rect(x0, centre, w, 1.0), p.axis));
    // The dry hit.
    window.paint_quad(fill(
        rect(x0, y0 + 6.0, 2.0, h - 12.0),
        Colors::with_alpha(p.early, 0.5),
    ));

    let lane = h * 0.5 - 6.0;
    for echo in echoes {
        if echo.at_ms > span_ms {
            continue;
        }
        let db = 20.0 * echo.gain.max(1.0e-6).log10();
        let level = ((db - ECHO_FLOOR_DB) / -ECHO_FLOOR_DB).clamp(0.0, 1.0);
        let length = (level * lane).max(1.0);
        let x = x0 + echo.at_ms / span_ms * w - 1.0;
        let color = if echo.right && !mono { p.right } else { p.left };
        let alpha = (0.35 + 0.65 * level) * p.dim;
        let bar = if echo.right {
            rect(x, centre + 1.0, 2.5, length)
        } else {
            rect(x, centre - length, 2.5, length)
        };
        window.paint_quad(fill(bar, Colors::with_alpha(color, alpha)));
    }
}

/// What the tone stage leaves after each of the window's `TONE_PASSES`.
fn paint_tone(window: &mut Window, bounds: Bounds<Pixels>, tones: &[Vec<f32>], p: &Palette) {
    let (x0, y0, w, h) = frame(bounds);
    window.paint_quad(fill(bounds, p.background));
    paint_freq_grid(window, bounds, p);
    for db in [-12.0, -24.0] {
        window.paint_quad(fill(rect(x0, y0 + db / TONE_FLOOR_DB * h, w, 1.0), p.grid));
    }
    for (pass, tone) in tones.iter().enumerate().rev() {
        let points: Vec<(f32, f32)> = tone
            .iter()
            .enumerate()
            .map(|(i, db)| {
                (
                    x0 + freq_fraction(curve_hz(i)) * w,
                    y0 + 3.0 + (db / TONE_FLOOR_DB).clamp(0.0, 1.0) * (h - 6.0),
                )
            })
            .collect();
        let alpha = [1.0, 0.6, 0.38, 0.22][pass.min(3)];
        let width = if pass == 0 { 2.0 } else { 1.2 };
        if pass == 0 {
            paint_area(
                window,
                &points,
                y0 + h,
                Colors::with_alpha(p.signal_fill, 0.10 * p.dim),
            );
        }
        paint_line(
            window,
            &points,
            width,
            Colors::with_alpha(p.signal, alpha * p.dim),
        );
    }
}

// ── Cards ──────────────────────────────────────────────────────────────────

/// A card of knobs. It takes a share of the row by how many knobs it holds,
/// and never gets narrower than they are.
fn section(title: &'static str, knobs: Vec<AnyElement>) -> AnyElement {
    let count = knobs.len() as f32;
    let mut card = div()
        .flex()
        .flex_col()
        .flex_basis(px(0.0))
        .min_w(px(count * KNOB_PITCH + 2.0 * space::BASE + 2.0))
        .gap(px(space::SNUG));
    card.style().flex_grow = Some(count);
    card.p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(caption(title))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_start()
                .gap(px(space::TIGHT))
                .children(knobs),
        )
        .into_any_element()
}

/// The knob for param `id`, live or — when the mode leaves it nothing to
/// do — greyed with `why` in place of its value.
fn knob_for(
    w: &FxEditorWindow,
    cx: &mut Cx,
    id: &'static str,
    why_not: Option<&'static str>,
) -> AnyElement {
    let kind = w.kind();
    let Some(spec) = knob(kind, id) else {
        return div().into_any_element();
    };
    let element_id = format!("{}-{id}", kind.key());
    if let Some(why) = why_not {
        return div()
            .opacity(state::DISABLED_CONTENT)
            .child(knob_cell(
                spec.label,
                why.to_string(),
                knob_with_default(
                    element_id,
                    0.0,
                    0.0,
                    1.0,
                    KNOB_SIZE,
                    Colors::text_disabled(),
                    0.0,
                    |_, _, _| {},
                ),
            ))
            .into_any_element();
    }
    let value = w.params.value(id);
    let default = default_value(kind, id);
    let (min, max) = spec.knob_range();
    let on_change = w.knob_cb(cx, spec);
    let control = if spec.bipolar {
        knob_bipolar(
            element_id,
            spec.to_knob(value),
            min,
            max,
            KNOB_SIZE,
            Colors::accent_primary(),
            None,
            spec.to_knob(default),
            on_change,
        )
        .into_any_element()
    } else {
        knob_with_default(
            element_id,
            spec.to_knob(value),
            min,
            max,
            KNOB_SIZE,
            Colors::accent_primary(),
            spec.to_knob(default),
            on_change,
        )
        .into_any_element()
    };
    knob_cell(spec.label, spec.readout(value), control)
}

fn cards(w: &FxEditorWindow, cx: &mut Cx) -> AnyElement {
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .flex_shrink_0()
        .gap(px(space::BASE));
    let mut knobs = |ids: &[(&'static str, Option<&'static str>)]| -> Vec<AnyElement> {
        ids.iter()
            .map(|(id, why)| knob_for(w, cx, id, *why))
            .collect()
    };
    match &w.params {
        FxParams::Verb(_) => {
            row = row
                .child(section(
                    "SPACE",
                    knobs(&[
                        ("predelayMs", None),
                        ("size", None),
                        ("decaySec", None),
                        ("diffusion", None),
                    ]),
                ))
                .child(section(
                    "TONE",
                    knobs(&[
                        ("damping", None),
                        ("bassMult", None),
                        ("lowCutHz", None),
                        ("highCutHz", None),
                    ]),
                ))
                .child(section(
                    "MOTION",
                    knobs(&[("modDepth", None), ("modRateHz", None)]),
                ))
                .child(section(
                    "OUTPUT",
                    knobs(&[("width", None), ("mix", None), ("outputDb", None)]),
                ));
        }
        FxParams::Echo(p) => {
            let mono = p.mode == echospace::DelayMode::Mono;
            let right_off = mono.then_some("mono");
            let (left, right) = if p.sync {
                ("divisionL", "divisionR")
            } else {
                ("timeMsL", "timeMsR")
            };
            row = row
                .child(section("TIME", knobs(&[(left, None), (right, right_off)])))
                .child(section(
                    "FEEDBACK",
                    knobs(&[
                        ("feedback", None),
                        ("crossFeedback", right_off),
                        ("diffusion", None),
                    ]),
                ))
                .child(section(
                    "TONE",
                    knobs(&[
                        ("lowCutHz", None),
                        ("highCutHz", None),
                        ("saturation", None),
                    ]),
                ))
                .child(section(
                    "MOTION",
                    knobs(&[("modDepth", None), ("modRateHz", None)]),
                ))
                .child(section(
                    "OUTPUT",
                    knobs(&[
                        ("duck", None),
                        ("width", right_off),
                        ("mix", None),
                        ("outputDb", None),
                    ]),
                ));
        }
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axes_land_on_tidy_figures() {
        assert_eq!(nice_ceiling(2_430.0), 2_500.0);
        assert_eq!(nice_ceiling(910.0), 1_000.0);
        assert_eq!(nice_step(2_500.0, 5), 500.0);
        assert_eq!(nice_step(12_000.0, 5), 2_000.0);
        assert_eq!(ms(1_500.0), "1.50 s");
        assert_eq!(ms(2_000.0), "2 s");
        assert_eq!(ms(250.0), "250 ms");
    }
}
