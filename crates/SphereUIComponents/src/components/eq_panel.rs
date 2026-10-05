//! The view of a built-in EQ's native editor (EQ-Z8 or EQ-ZX): a header, the
//! response graph, the band strip and the selected band's editor.
//!
//! Pure rendering over [`EqEditorWindow`]: every edit goes back through the
//! window's `*_cb` builders, every gesture on the graph through its handlers.
//! The graph's labels and numbered nodes are divs placed by the same
//! fractions [`eq_graph`] paints and hit-tests by, over one canvas.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, relative, AnyElement, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Rgba, ScrollWheelEvent, StatefulInteractiveElement, Styled,
};

use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, fb_toggle, FbButtonKind, FbLatch,
    FbSegment,
};
use crate::components::eq_graph::{
    band_color, db_fraction, db_grid, freq_at_fraction, freq_fraction, node_fractions, paint_graph,
    GraphPaint, GraphPalette, DB_RANGES, FREQ_LABELS,
};
use crate::components::eq_model::{
    default_band, format_db, format_freq, format_ms, Band, EqKind, Placement, ATTACK_MAX_MS,
    ATTACK_MIN_MS, GAIN_MAX_DB, GAIN_MIN_DB, OUTPUT_MAX_DB, OUTPUT_MIN_DB, Q_MAX, Q_MIN,
    RANGE_MAX_DB, RANGE_MIN_DB, RELEASE_MAX_MS, RELEASE_MIN_MS, SLOPES, THRESHOLD_MAX_DB,
    THRESHOLD_MIN_DB,
};
use crate::components::eq_window::EqEditorWindow;
use crate::components::knob::{knob_bipolar, knob_with_default};
use crate::components::quick_sampler_panel::{knob_cell, KNOB_SIZE};
use crate::theme::{radius, space, state, typography, Colors};

/// The dB label column left of the plot, and the frequency row under it.
const DB_LABEL_W: f32 = 30.0;
const FREQ_LABEL_H: f32 = 16.0;
const NODE: f32 = 18.0;
const STRIP_CELL_W: f32 = 96.0;
const PLOT_MIN_H: f32 = 180.0;

type Cx<'a> = Context<'a, EqEditorWindow>;

/// The whole editor.
pub(crate) fn eq_panel(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
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
                .child(graph_toolbar(w, cx))
                .child(graph(w, cx))
                .child(band_strip(w, cx))
                .child(band_editor(w, cx)),
        )
        .into_any_element()
}

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

/// A cluster of related header controls on one row.
fn cluster() -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(space::TIGHT))
}

// ── Header ─────────────────────────────────────────────────────────────────

fn header(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
    let kind = w.kind();
    let power = w.params.power();
    let open_menu = {
        let entity = cx.entity().clone();
        move |event: &MouseDownEvent, _: &mut gpui::Window, app: &mut gpui::App| {
            let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
            let _ = entity.update(app, |this, cx| this.open_preset_menu(x, y, cx));
        }
    };
    let preset_picker = div()
        .id("eq-preset-picker")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .w(px(200.0))
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
            "eq-preset-prev",
            "‹",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.step_preset(-1, cx)),
        ))
        .child(preset_picker)
        .child(fb_button(
            "eq-preset-next",
            "›",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.step_preset(1, cx)),
        ))
        .child(fb_button(
            "eq-preset-reset",
            "Reset",
            FbButtonKind::Ghost,
            true,
            w.click_cb(cx, |this, cx| this.load_preset(0, cx)),
        ));

    let on_b = w.compare.on_b;
    let compare = cluster()
        .child(
            fb_segmented_track()
                .child(fb_segment(
                    "eq-compare-a",
                    "A",
                    !on_b,
                    FbSegment::First,
                    w.click_cb(cx, move |this, cx| {
                        if this.compare.on_b {
                            this.swap_compare(cx)
                        }
                    }),
                ))
                .child(fb_segment(
                    "eq-compare-b",
                    "B",
                    on_b,
                    FbSegment::Last,
                    w.click_cb(cx, move |this, cx| {
                        if !this.compare.on_b {
                            this.swap_compare(cx)
                        }
                    }),
                ))
                .w(px(track_width(&["A", "B"]))),
        )
        .child(fb_button(
            "eq-compare-copy",
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
            "eq-power",
            "Power",
            power,
            true,
            w.click_cb(cx, |this, cx| this.toggle_power(cx)),
        ))
        .into_any_element()
}

/// What the graph shows, on a row of its own above it: the part of the
/// image (EQ-ZX), the dB span, and the analyser and band curves.
fn graph_toolbar(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
    let range_labels: Vec<String> = DB_RANGES.iter().map(|db| format!("±{db:.0}")).collect();
    let mut range = fb_segmented_track();
    for (index, db) in DB_RANGES.into_iter().enumerate() {
        range = range.child(fb_segment(
            ("eq-db-range", index),
            range_labels[index].clone(),
            w.db_range == db,
            segment_position(index, DB_RANGES.len()),
            w.click_cb(cx, move |this, cx| this.set_db_range(db, cx)),
        ));
    }
    let range = range.w(px(track_width(&range_labels)));

    let view = w.kind().has_placement().then(|| {
        let labels: Vec<String> = Placement::ALL
            .iter()
            .map(|p| p.label().to_string())
            .collect();
        let mut track = fb_segmented_track();
        for (index, placement) in Placement::ALL.into_iter().enumerate() {
            track = track.child(fb_segment(
                ("eq-view", index),
                placement.label(),
                w.view == placement,
                segment_position(index, Placement::ALL.len()),
                w.click_cb(cx, move |this, cx| this.set_view(placement, cx)),
            ));
        }
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .child(caption("VIEW"))
            .child(track.w(px(track_width(&labels))))
    });

    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .ml(px(DB_LABEL_W))
        .children(view)
        .child(div().flex_1())
        .child(fb_checkbox(
            "eq-show-analyser",
            "Analyser",
            w.show_spectrum,
            true,
            w.click_cb(cx, |this, cx| this.toggle_spectrum(cx)),
        ))
        .child(fb_checkbox(
            "eq-show-band-curves",
            "Band curves",
            w.show_band_curves,
            true,
            w.click_cb(cx, |this, cx| this.toggle_band_curves(cx)),
        ))
        .child(range)
        .into_any_element()
}

/// The width a segmented track needs for labels. [b_segment] sets a
/// minimum width, which replaces flexbox's own content-based minimum, so a
/// track narrower than its labels squeezes them out of their segments.
fn track_width<S: AsRef<str>>(labels: &[S]) -> f32 {
    // Segments split a track equally, so each is as wide as the widest label.
    let widest = labels
        .iter()
        .map(|label| (label.as_ref().chars().count() as f32 * 8.5 + 2.0 * space::BASE).max(44.0))
        .fold(0.0f32, f32::max);
    widest * labels.len() as f32 + 2.0 * space::TIGHT + 2.0
}

// ── Graph ──────────────────────────────────────────────────────────────────

fn graph(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
    let range = w.db_range;
    let view = w.shown_view();
    let bypassed = !w.params.power();
    let band_colors: Arc<Vec<Rgba>> = Arc::new((0..w.kind().slots()).map(band_color).collect());
    let paint = GraphPaint {
        curves: w.curves.clone(),
        db_range: range,
        spectrum: if w.show_spectrum {
            w.spectrum.clone()
        } else {
            None
        },
        show_band_curves: w.show_band_curves,
        selected: w.selected,
        bypassed,
        hover: w.hover.map(|(fx, _)| fx),
        palette: GraphPalette::resolve(),
        band_colors,
    };
    let bounds_out = w.plot_bounds.clone();
    let plot_canvas = canvas(
        move |bounds, _, _| bounds_out.set(Some(bounds)),
        move |bounds, _, window, _| paint_graph(window, bounds, &paint),
    )
    .absolute()
    .size_full();

    let solo = w.params.solo();
    let mut nodes = Vec::new();
    for index in w.params.listed() {
        let band = w.params.band(index);
        if !band.placement.heard_in(view) {
            continue;
        }
        let (fx, fy) = node_fractions(&w.params, index, range);
        nodes.push(node(
            index,
            &band,
            (fx, fy),
            w.selected == Some(index),
            solo == Some(index),
            bypassed,
        ));
    }

    let readout = readout(w).map(|text| {
        div()
            .absolute()
            .top(px(space::SNUG))
            .left(px(space::BASE))
            .px(px(space::SNUG))
            .py(px(space::HAIR))
            .rounded(px(radius::CONTROL_SM))
            .bg(Colors::with_alpha(Colors::surface_base(), 0.88))
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(Colors::text_secondary())
            .child(text)
    });

    let hint = if bypassed {
        Some("Bypassed — the EQ passes audio through unchanged".to_string())
    } else if let Some(notice) = w.notice() {
        Some(notice.to_string())
    } else if !w.params.listed().iter().any(|i| w.params.band(*i).active) {
        Some(
            match w.kind() {
                // EQ-Z8's eight nodes are always there to drag; EQ-ZX starts empty.
                EqKind::Z8 => "Double-click the graph to add a band, or drag a numbered node",
                EqKind::Zx => "Double-click the graph to add a band",
            }
            .to_string(),
        )
    } else {
        None
    };
    let hint = hint.map(|text| {
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

    let plot = div()
        .id("eq-plot")
        .relative()
        .flex_1()
        .min_h(px(PLOT_MIN_H))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .cursor(if w.drag.is_some() {
            gpui::CursorStyle::ClosedHand
        } else {
            gpui::CursorStyle::Crosshair
        })
        .child(plot_canvas)
        .children(nodes)
        .children(readout)
        .children(hint)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, _, cx| this.plot_mouse_down(event, cx)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, _, cx| this.plot_mouse_down(event, cx)),
        )
        .on_scroll_wheel(
            cx.listener(|this, event: &ScrollWheelEvent, _, cx| this.plot_scroll(event, cx)),
        )
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            if !hovered {
                this.mouse_left_plot(cx)
            }
        }));

    let db_labels = div().relative().w(px(DB_LABEL_W)).flex_shrink_0().children(
        db_grid(range).into_iter().map(|db| {
            div()
                .absolute()
                .top(relative(db_fraction(db, range)))
                .mt(px(-6.0))
                .right(px(space::TIGHT))
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(if db == 0.0 {
                    Colors::text_secondary()
                } else {
                    Colors::text_faint()
                })
                .child(if db > 0.0 {
                    format!("+{db:.0}")
                } else {
                    format!("{db:.0}")
                })
        }),
    );
    let freq_labels = div()
        .relative()
        .h(px(FREQ_LABEL_H))
        .ml(px(DB_LABEL_W))
        .children(FREQ_LABELS.into_iter().map(|(hz, label)| {
            div()
                .absolute()
                .left(relative(freq_fraction(hz)))
                .ml(px(-12.0))
                .w(px(24.0))
                .flex()
                .justify_center()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(label)
        }));

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(PLOT_MIN_H + FREQ_LABEL_H))
        .opacity(if bypassed { 0.7 } else { 1.0 })
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(PLOT_MIN_H))
                .child(db_labels)
                .child(plot),
        )
        .child(freq_labels)
        .into_any_element()
}

/// The numbered node of band `index` at `(fx, fy)` across and down the plot.
fn node(
    index: usize,
    band: &Band,
    (fx, fy): (f32, f32),
    selected: bool,
    soloed: bool,
    bypassed: bool,
) -> AnyElement {
    let color = band_color(index);
    let (fill, text) = if band.active {
        (color, Colors::surface_canvas())
    } else {
        (Colors::surface_canvas(), color)
    };
    let ring = if soloed {
        Some(Colors::state_solo())
    } else if selected {
        Some(Colors::text_primary())
    } else {
        None
    };
    div()
        .absolute()
        .left(relative(fx))
        .top(relative(fy))
        .ml(px(-NODE / 2.0))
        .mt(px(-NODE / 2.0))
        .size(px(NODE))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(fill)
        .border(px(if ring.is_some() { 2.0 } else { 1.0 }))
        .border_color(ring.unwrap_or(color))
        .when(!band.active, |node| node.border_dashed())
        .opacity(if bypassed { 0.35 } else { 1.0 })
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(text)
        .child(format!("{}", index + 1))
        .into_any_element()
}

/// What the readout in the plot's corner says: the band being dragged, or
/// the response under the pointer.
fn readout(w: &EqEditorWindow) -> Option<String> {
    if let Some(drag) = w.drag {
        let band = w.params.band(drag.band);
        let mut text = format!("Band {} · {} Hz", drag.band + 1, format_freq(band.freq));
        if band.shape.has_gain() {
            text.push_str(&format!(" · {} dB", format_db(band.gain_db)));
        }
        if band.shape.uses_q(w.kind()) {
            text.push_str(&format!(" · Q {:.2}", band.q));
        }
        if let Some(solo) = w.params.solo() {
            text.push_str(&format!(" · listening to band {}", solo + 1));
        }
        return Some(text);
    }
    let (fx, _) = w.hover?;
    let hz = freq_at_fraction(fx);
    let db = w.params.response_db(w.shown_view(), hz, w.sample_rate());
    Some(format!("{} Hz · {} dB", format_freq(hz), format_db(db)))
}

// ── Band strip ─────────────────────────────────────────────────────────────

fn band_strip(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
    let listed = w.params.listed();
    let solo = w.params.solo();
    let mut strip = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::TIGHT))
        .flex_shrink_0();
    for index in listed.iter().copied() {
        strip = strip.child(strip_cell(w, cx, index, solo == Some(index)));
    }
    if w.kind() == EqKind::Zx {
        let slots = w.kind().slots();
        let used = listed.len();
        strip = strip.child(
            div()
                .px(px(space::SNUG))
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(if used == 0 {
                    "No bands yet — double-click the graph to add one".to_string()
                } else if used == slots {
                    format!("{used} / {slots} bands — limit reached")
                } else {
                    format!("{used} / {slots} bands")
                }),
        );
    }
    strip.into_any_element()
}

fn strip_cell(w: &EqEditorWindow, cx: &mut Cx, index: usize, soloed: bool) -> AnyElement {
    let band = w.params.band(index);
    let selected = w.selected == Some(index);
    let color = band_color(index);
    let rest = if selected {
        Colors::composite(Colors::surface_panel(), Colors::state_selected())
    } else {
        Colors::surface_panel()
    };
    let detail = if band.shape.has_gain() {
        format!("{} dB", format_db(band.gain_db))
    } else if band.shape.is_cut() && w.kind().has_slopes() {
        format!("{} {:.0}", band.shape.short(), band.slope)
    } else {
        band.shape.short().to_string()
    };
    let mut badges = Vec::new();
    if band.dynamics_live() {
        badges.push(("D", Colors::accent_warning()));
    }
    if band.placement != Placement::Stereo {
        badges.push((band.placement.short(), Colors::text_secondary()));
    }
    if soloed {
        badges.push(("S", Colors::state_solo()));
    }
    let lamp = {
        let entity = cx.entity().clone();
        div()
            .id(("eq-band-lamp", index))
            .size(px(10.0))
            .rounded_full()
            .border(px(1.0))
            .border_color(color)
            .when(band.active, |lamp| lamp.bg(color))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(MouseButton::Left, move |_, _, app| {
                app.stop_propagation();
                let _ = entity.update(app, |this, cx| this.toggle_band(index, cx));
            })
    };
    let on_press = {
        let entity = cx.entity().clone();
        move |event: &MouseDownEvent, _: &mut gpui::Window, app: &mut gpui::App| {
            let clicks = event.click_count;
            let _ = entity.update(app, |this, cx| {
                if clicks >= 2 {
                    this.toggle_band(index, cx)
                } else {
                    this.select(index, cx)
                }
            });
        }
    };
    div()
        .id(("eq-band-cell", index))
        .relative()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .w(px(STRIP_CELL_W))
        .px(px(space::SNUG))
        .py(px(space::TIGHT))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(if selected {
            Colors::border_normal()
        } else {
            Colors::border_subtle()
        })
        .bg(rest)
        .hover(move |style| style.bg(Colors::composite(rest, Colors::state_hover())))
        .opacity(if band.active { 1.0 } else { 0.55 })
        .cursor(gpui::CursorStyle::PointingHand)
        .on_mouse_down(MouseButton::Left, on_press)
        // The selection marker: a leading-edge accent, never a border.
        .when(selected, |cell| {
            cell.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(space::TIGHT))
                    .bottom(px(space::TIGHT))
                    .w(px(2.0))
                    .bg(Colors::accent_primary()),
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(color)
                        .child(format!("{}", index + 1)),
                )
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(band.shape.short()),
                )
                .children(badges.into_iter().map(|(text, tone)| {
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(tone)
                        .child(text)
                }))
                .child(lamp),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_secondary())
                .child(format!("{} Hz", format_freq(band.freq)))
                .child(detail),
        )
        .into_any_element()
}

// ── Band editor ────────────────────────────────────────────────────────────

/// A knob over a log-mapped range: the knob turns 0..1, the field is
/// `min·(max/min)^position`.
fn log_position(value: f32, min: f32, max: f32) -> f32 {
    ((value.clamp(min, max) / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
}

fn log_value(position: f32, min: f32, max: f32) -> f32 {
    min * (max / min).powf(position.clamp(0.0, 1.0))
}

/// `control` greyed out and inert, with `why` as its readout.
fn disabled(caption: &'static str, why: &'static str, id: String) -> AnyElement {
    div()
        .opacity(state::DISABLED_CONTENT)
        .child(knob_cell(
            caption,
            why.to_string(),
            knob_with_default(
                id,
                0.0,
                0.0,
                1.0,
                KNOB_SIZE,
                Colors::text_disabled(),
                0.0,
                |_, _, _| {},
            ),
        ))
        .into_any_element()
}

/// How a card of the band editor's row shares the width: a flex weight,
/// and the narrowest it may get before the row wraps it — its contents'
/// width, since an explicit minimum replaces flexbox's content minimum.
#[derive(Clone, Copy)]
struct Share {
    grow: f32,
    min_w: f32,
}

const BAND_SHARE: Share = Share {
    grow: 5.0,
    min_w: 380.0,
};
const DYNAMICS_SHARE: Share = Share {
    grow: 3.0,
    min_w: 300.0,
};
const OUTPUT_SHARE: Share = Share {
    grow: 1.0,
    min_w: 150.0,
};

fn section(title: impl Into<String>, share: Share, body: impl IntoElement) -> AnyElement {
    let mut card = div()
        .flex()
        .flex_col()
        .flex_basis(px(0.0))
        .min_w(px(share.min_w))
        .gap(px(space::SNUG));
    card.style().flex_grow = Some(share.grow);
    card.p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(caption(title))
        .child(body)
        .into_any_element()
}

fn knob_row() -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_start()
        .gap(px(space::TIGHT))
}

fn band_editor(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
    // The cards share the full width by weight and stretch to one height.
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(space::BASE))
        .flex_shrink_0();
    if let Some(index) = w.selected {
        row = row
            .child(band_section(w, cx, index))
            .child(dynamics_section(w, cx, index));
    } else {
        row = row.child(section(
            "BAND",
            BAND_SHARE,
            div()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_muted())
                .child("Select a band, or double-click the graph to add one."),
        ));
    }
    row.child(output_section(w, cx)).into_any_element()
}

fn band_section(w: &EqEditorWindow, cx: &mut Cx, index: usize) -> AnyElement {
    let kind = w.kind();
    let band = w.params.band(index);
    let defaults = default_band(kind, index);
    let soloed = w.params.solo() == Some(index);

    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(
            div()
                .flex_1()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(band_color(index))
                .child(format!("Band {}", index + 1)),
        )
        .child(fb_checkbox(
            ("eq-band-on", index),
            "On",
            band.active,
            true,
            w.click_cb(cx, move |this, cx| this.toggle_band(index, cx)),
        ))
        .child(fb_toggle(
            ("eq-band-solo", index),
            "Solo",
            FbLatch::Solo,
            soloed,
            20.0,
            w.click_cb(cx, move |this, cx| this.toggle_solo(index, cx)),
        ))
        .when(kind == EqKind::Zx, |title| {
            title.child(fb_button(
                ("eq-band-remove", index),
                "Remove",
                FbButtonKind::Ghost,
                true,
                w.click_cb(cx, move |this, cx| this.remove_band(index, cx)),
            ))
        });

    let mut shapes = fb_segmented_track();
    let all = kind.shapes();
    for (position, shape) in all.iter().copied().enumerate() {
        shapes = shapes.child(fb_segment(
            ("eq-band-shape", index * 16 + position),
            shape.short(),
            band.shape == shape,
            segment_position(position, all.len()),
            w.click_cb(cx, move |this, cx| {
                this.edit_band(index, |b| b.shape = shape, cx)
            }),
        ));
    }

    let shapes_w = track_width(&all.iter().map(|shape| shape.short()).collect::<Vec<_>>());
    let placement_w = track_width(&Placement::ALL.map(Placement::short));
    // EQ-ZX keeps shape and placement on one line: the card is never
    // narrower than both tracks side by side.
    let share = if kind.has_placement() {
        Share {
            min_w: BAND_SHARE
                .min_w
                .max(shapes_w + space::SNUG + placement_w + 2.0 * space::BASE + 2.0),
            ..BAND_SHARE
        }
    } else {
        BAND_SHARE
    };

    let mut choices = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(space::SNUG))
        .child(shapes.w(px(shapes_w)));
    if kind.has_placement() {
        let mut placement = fb_segmented_track();
        for (position, place) in Placement::ALL.into_iter().enumerate() {
            placement = placement.child(fb_segment(
                ("eq-band-placement", index * 4 + position),
                place.short(),
                band.placement == place,
                segment_position(position, Placement::ALL.len()),
                w.click_cb(cx, move |this, cx| {
                    this.edit_band(index, |b| b.placement = place, cx)
                }),
            ));
        }
        choices = choices.child(placement.w(px(placement_w)));
    }
    if kind.has_slopes() && band.shape.is_cut() {
        let mut slopes = fb_segmented_track();
        for (position, slope) in SLOPES.into_iter().enumerate() {
            slopes = slopes.child(fb_segment(
                ("eq-band-slope", index * 8 + position),
                format!("{slope:.0}"),
                (band.slope - slope).abs() < 0.5,
                segment_position(position, SLOPES.len()),
                w.click_cb(cx, move |this, cx| {
                    this.edit_band(index, |b| b.slope = slope, cx)
                }),
            ));
        }
        choices = choices.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(slopes.w(px(track_width(&SLOPES.map(|slope| format!("{slope:.0}"))))))
                .child(caption("dB/oct")),
        );
    }

    let freq = knob_cell(
        "Freq",
        format!("{} Hz", format_freq(band.freq)),
        knob_with_default(
            format!("eq-band-freq-{index}"),
            freq_fraction(band.freq),
            0.0,
            1.0,
            KNOB_SIZE,
            Colors::accent_primary(),
            freq_fraction(defaults.freq),
            w.band_cb(cx, index, |b, v| b.freq = freq_at_fraction(v)),
        ),
    );
    let gain = if band.shape.has_gain() {
        knob_cell(
            "Gain",
            format!("{} dB", format_db(band.gain_db)),
            knob_bipolar(
                format!("eq-band-gain-{index}"),
                band.gain_db,
                GAIN_MIN_DB,
                GAIN_MAX_DB,
                KNOB_SIZE,
                Colors::accent_primary(),
                None,
                0.0,
                w.band_cb(cx, index, |b, v| b.gain_db = (v * 10.0).round() / 10.0),
            ),
        )
    } else {
        disabled("Gain", "no gain", format!("eq-band-gain-off-{index}"))
    };
    let q = if band.shape.uses_q(kind) {
        knob_cell(
            "Q",
            format!("{:.2}", band.q),
            knob_with_default(
                format!("eq-band-q-{index}"),
                log_position(band.q, Q_MIN, Q_MAX),
                0.0,
                1.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                log_position(defaults.q.max(Q_MIN), Q_MIN, Q_MAX),
                w.band_cb(cx, index, |b, v| {
                    b.q = (log_value(v, Q_MIN, Q_MAX) * 100.0).round() / 100.0
                }),
            ),
        )
    } else {
        disabled("Q", "by slope", format!("eq-band-q-off-{index}"))
    };

    section(
        "BAND",
        share,
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(title)
            .child(choices)
            .child(knob_row().child(freq).child(gain).child(q)),
    )
}

fn dynamics_section(w: &EqEditorWindow, cx: &mut Cx, index: usize) -> AnyElement {
    let kind = w.kind();
    let band = w.params.band(index);
    let defaults = default_band(kind, index);
    let can = band.shape.has_gain();
    let live = band.dynamics_live();

    let mut head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(fb_checkbox(
            ("eq-band-dynamic", index),
            if can {
                "Dynamic"
            } else {
                "Dynamic (needs a bell or shelf)"
            },
            live,
            can,
            w.click_cb(cx, move |this, cx| {
                this.edit_band(index, |b| b.dynamic = !b.dynamic, cx)
            }),
        ));
    if kind.has_dyn_mode() {
        let below = band.dyn_below;
        head = head.child(
            fb_segmented_track()
                .child(fb_segment(
                    ("eq-band-dyn-above", index),
                    "Above",
                    !below,
                    FbSegment::First,
                    w.click_cb(cx, move |this, cx| {
                        this.edit_band(index, |b| b.dyn_below = false, cx)
                    }),
                ))
                .child(fb_segment(
                    ("eq-band-dyn-below", index),
                    "Below",
                    below,
                    FbSegment::Last,
                    w.click_cb(cx, move |this, cx| {
                        this.edit_band(index, |b| b.dyn_below = true, cx)
                    }),
                ))
                .w(px(track_width(&["Above", "Below"]))),
        );
    }

    let knobs = if live {
        knob_row()
            .child(knob_cell(
                "Thresh",
                format!("{} dB", format_db(band.threshold_db)),
                knob_with_default(
                    format!("eq-band-thresh-{index}"),
                    band.threshold_db,
                    THRESHOLD_MIN_DB,
                    THRESHOLD_MAX_DB,
                    KNOB_SIZE,
                    Colors::accent_primary(),
                    defaults.threshold_db,
                    w.band_cb(cx, index, |b, v| b.threshold_db = (v * 2.0).round() / 2.0),
                ),
            ))
            .child(knob_cell(
                "Range",
                format!("{} dB", format_db(band.range_db)),
                knob_bipolar(
                    format!("eq-band-range-{index}"),
                    band.range_db,
                    RANGE_MIN_DB,
                    RANGE_MAX_DB,
                    KNOB_SIZE,
                    Colors::accent_primary(),
                    None,
                    0.0,
                    w.band_cb(cx, index, |b, v| b.range_db = (v * 10.0).round() / 10.0),
                ),
            ))
            .child(knob_cell(
                "Attack",
                format_ms(band.attack_ms),
                knob_with_default(
                    format!("eq-band-attack-{index}"),
                    log_position(band.attack_ms, ATTACK_MIN_MS, ATTACK_MAX_MS),
                    0.0,
                    1.0,
                    KNOB_SIZE,
                    Colors::accent_primary(),
                    log_position(defaults.attack_ms, ATTACK_MIN_MS, ATTACK_MAX_MS),
                    w.band_cb(cx, index, |b, v| {
                        b.attack_ms = log_value(v, ATTACK_MIN_MS, ATTACK_MAX_MS)
                    }),
                ),
            ))
            .child(knob_cell(
                "Release",
                format_ms(band.release_ms),
                knob_with_default(
                    format!("eq-band-release-{index}"),
                    log_position(band.release_ms, RELEASE_MIN_MS, RELEASE_MAX_MS),
                    0.0,
                    1.0,
                    KNOB_SIZE,
                    Colors::accent_primary(),
                    log_position(defaults.release_ms, RELEASE_MIN_MS, RELEASE_MAX_MS),
                    w.band_cb(cx, index, |b, v| {
                        b.release_ms = log_value(v, RELEASE_MIN_MS, RELEASE_MAX_MS)
                    }),
                ),
            ))
    } else {
        let why = if can { "off" } else { "no gain" };
        knob_row()
            .child(disabled(
                "Thresh",
                why,
                format!("eq-band-thresh-off-{index}"),
            ))
            .child(disabled("Range", why, format!("eq-band-range-off-{index}")))
            .child(disabled(
                "Attack",
                why,
                format!("eq-band-attack-off-{index}"),
            ))
            .child(disabled(
                "Release",
                why,
                format!("eq-band-release-off-{index}"),
            ))
    };

    section(
        "DYNAMICS",
        DYNAMICS_SHARE,
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(head)
            .child(knobs),
    )
}

fn output_section(w: &EqEditorWindow, cx: &mut Cx) -> AnyElement {
    let output = w.params.output_db();
    let mut knobs = knob_row().child(knob_cell(
        "Output",
        format!("{} dB", format_db(output)),
        knob_bipolar(
            "eq-output",
            output,
            OUTPUT_MIN_DB,
            OUTPUT_MAX_DB,
            KNOB_SIZE,
            Colors::accent_primary(),
            None,
            0.0,
            w.value_cb(cx, |this, v, cx| {
                this.set_output_db((v * 10.0).round() / 10.0, cx)
            }),
        ),
    ));
    if let Some(mix) = w.params.mix() {
        knobs = knobs.child(knob_cell(
            "Mix",
            format!("{} %", mix.round() as i32),
            knob_with_default(
                "eq-mix",
                mix,
                0.0,
                100.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                100.0,
                w.value_cb(cx, |this, v, cx| this.set_mix(v.round(), cx)),
            ),
        ));
    }
    section("OUTPUT", OUTPUT_SHARE, knobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_knobs_map_both_ways() {
        for q in [Q_MIN, 0.7, 1.0, 4.5, Q_MAX] {
            let back = log_value(log_position(q, Q_MIN, Q_MAX), Q_MIN, Q_MAX);
            assert!((back - q).abs() / q < 1.0e-4, "{q} → {back}");
        }
        assert_eq!(log_position(0.0, Q_MIN, Q_MAX), 0.0);
        assert_eq!(log_position(99.0, Q_MIN, Q_MAX), 1.0);
    }
}
