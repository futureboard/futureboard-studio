//! The Imager's native editor, laid out after Ozone's Imager: the
//! vectorscope and correlation on the left, the band display beside them,
//! and a strip per band below — a width fader reading −100 (mono) to +100,
//! its correlation, its stereoize and its solo — with the global settings
//! (multiband, Link Bands, the stereoize character, Recover Sides) in their
//! own card.
//!
//! Part of the band family (`band_panel`): this module draws the Imager's
//! panel and live displays; the gestures, the band display and the wire
//! edits are shared with the Compressor there.

use gpui::{
    canvas, div, fill, px, AnyElement, App, Bounds, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Styled, Window,
};

use crate::components::band_model::{
    short_hz, stereoize_id, width_readout, BandKind, BandParams, BANDS, BAND_NAMES,
};
use crate::components::band_panel::{band_plot, main_display, pill, press_fader, BandDrag};
use crate::components::controls::fb_checkbox;
use crate::components::plugin_kit::{
    caption, card, choice, display_frame, flag, header, hint, knob_for, Cx, KitEditor, LiveBounds,
    LiveSlot, KNOB,
};
use crate::components::plugin_live::{
    frame_of, paint_correlation, paint_correlation_vertical, paint_level_bar, paint_spectrum,
    paint_vectorscope, Live, ScopeMode,
};
use crate::components::quick_sampler_panel::rect;
use crate::theme::{radius, space, state, typography, Colors};

/// A width fader's travel.
const FADER_H: f32 = 128.0;
const FADER_W: f32 = 30.0;

pub(crate) fn imager_panel(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(e, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .p(px(space::LOOSE))
                .gap(px(space::BASE))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h(px(240.0))
                        .gap(px(space::BASE))
                        .child(scope_column(e, cx))
                        .child(main_display(e, cx)),
                )
                .child(strips(e, cx)),
        )
        .into_any_element()
}

fn well(slots: &LiveBounds, slot: LiveSlot, height: f32) -> gpui::Div {
    div()
        .h(px(height))
        .flex_shrink_0()
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(slots.slot(slot).size_full())
}

fn scope_column(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    let labels = ScopeMode::ALL.map(ScopeMode::label);
    let selected = ScopeMode::ALL.iter().position(|mode| *mode == e.ui.scope);
    div()
        .flex()
        .flex_col()
        .w(px(310.0))
        .flex_shrink_0()
        .gap(px(space::BASE))
        .child(
            display_frame(
                "VECTORSCOPE",
                Some(e.ui.scope.title().to_string()),
                None,
                div()
                    .size_full()
                    .pt(px(22.0))
                    .child(e.live_bounds.slot(LiveSlot::Scope).size_full()),
            )
            .flex_1()
            .min_h(px(180.0)),
        )
        .child(choice(
            e,
            cx,
            "imager-scope",
            &labels,
            selected,
            |this, index, cx| {
                this.ui.scope = ScopeMode::ALL[index];
                cx.notify();
            },
        ))
        .child(well(&e.live_bounds, LiveSlot::Correlation(BANDS), 58.0))
        .child(well(&e.live_bounds, LiveSlot::Meters, 66.0))
        .into_any_element()
}

/// The global card: the split, Link Bands, the stereoize character,
/// Recover Sides and the output trim.
fn global_card(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>, p: &imager::Params) -> AnyElement {
    let link = fb_checkbox(
        "imager-link",
        "Link Bands",
        e.ui.link_bands,
        p.multiband,
        e.click_cb(cx, |this, cx| {
            this.ui.link_bands = !this.ui.link_bands;
            cx.notify();
        }),
    );
    let mode = imager::StereoizeMode::ALL
        .iter()
        .position(|mode| *mode == p.stereoize_mode);
    let body = div()
        .flex()
        .flex_row()
        .gap(px(space::LOOSE))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::SNUG))
                .child(flag(e, cx, "multiband", "Multiband", true))
                .child(link)
                .child(flag(e, cx, "recoverSides", "Recover Sides", true))
                .child(caption("STEREOIZE"))
                .child(choice(
                    e,
                    cx,
                    "imager-stereoize-mode",
                    &["I", "II"],
                    mode,
                    |this, index, cx| this.set_value("stereoizeMode", index as f32, cx),
                ))
                .child(hint(match p.stereoize_mode {
                    imager::StereoizeMode::One => "Haas delay: bold",
                    imager::StereoizeMode::Two => "Decorrelated: smooth",
                })),
        )
        .child(knob_for(e, cx, "outputDb", KNOB + 6.0, None));
    card("GLOBAL", 0.0, 230.0, body)
}

fn strips(e: &KitEditor<BandKind>, cx: &mut Cx<BandKind>) -> AnyElement {
    let BandParams::Imager(p) = e.params.clone() else {
        return div().into_any_element();
    };
    let row = div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .gap(px(space::BASE))
        .child(global_card(e, cx, &p));
    let strips: Vec<AnyElement> = (0..BANDS).map(|band| strip(e, cx, &p, band)).collect();
    row.children(strips).into_any_element()
}

/// One band's strip: its width fader and correlation, its stereoize and
/// solo. With the split off, the first strip works the whole signal and the
/// rest stand aside.
fn strip(
    e: &KitEditor<BandKind>,
    cx: &mut Cx<BandKind>,
    p: &imager::Params,
    band: usize,
) -> AnyElement {
    let params = &e.params;
    let edges = params.edges();
    let live = p.multiband || band == 0;
    let solo = params.solo().filter(|_| p.multiband);
    let soloed = solo == Some(band);
    let title = if p.multiband {
        BAND_NAMES[band].to_uppercase()
    } else if band == 0 {
        "WHOLE SIGNAL".to_string()
    } else {
        BAND_NAMES[band].to_uppercase()
    };
    let range = if p.multiband {
        format!(
            "{} – {} Hz",
            short_hz(edges[band]),
            short_hz(edges[band + 1])
        )
    } else if band == 0 {
        "20 – 20k Hz".to_string()
    } else {
        "Multiband off".to_string()
    };
    let width = p.width[band];
    let dragging = matches!(
        e.ui.drag,
        Some(BandDrag::Fader { band: b, .. } | BandDrag::Width { band: b, .. }) if b == band
    );
    let fader_picture = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_fader(window, bounds, width, dragging, live),
    )
    .absolute()
    .size_full();
    let mut fader = div()
        .id(("imager-fader", band))
        .relative()
        .w(px(FADER_W))
        .h(px(FADER_H))
        .child(fader_picture)
        .child(
            e.live_bounds
                .slot(LiveSlot::Fader(band))
                .absolute()
                .size_full(),
        );
    if live {
        fader = fader.cursor(gpui::CursorStyle::ResizeUpDown).on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, move |this, (_, y), event, cx| {
                press_fader(this, band, y, event, cx)
            }),
        );
    }
    let fader_column = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::HAIR))
        .child(fader)
        .child(caption("WIDTH"))
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(width_readout(width)),
        );
    let correlation = div()
        .w(px(6.0))
        .h(px(FADER_H))
        .child(e.live_bounds.slot(LiveSlot::Correlation(band)).size_full());
    let controls = div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .child(knob_for(e, cx, stereoize_id(band), KNOB, None))
        .children(p.multiband.then(|| {
            pill(
                e,
                cx,
                ("imager-solo", band),
                "Solo",
                soloed,
                Colors::accent_warning(),
                move |this, cx| {
                    let next = if soloed { -1.0 } else { band as f32 };
                    this.set_value("soloBand", next, cx)
                },
            )
        }));
    let mut column = div()
        .flex()
        .flex_col()
        .flex_basis(px(0.0))
        .min_w(px(160.0))
        .gap(px(space::SNUG))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(if soloed {
            Colors::with_alpha(Colors::accent_warning(), 0.6)
        } else {
            Colors::border_subtle()
        })
        .bg(Colors::surface_panel())
        .opacity(if !live {
            state::DISABLED_CONTENT
        } else if solo.is_some() && !soloed {
            0.55
        } else {
            1.0
        });
    column.style().flex_grow = Some(1.0);
    column
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .child(caption(title))
                .child(hint(range)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(space::SNUG))
                .child(fader_column)
                .child(correlation)
                .child(controls),
        )
        .into_any_element()
}

/// A width fader: its track, the unchanged mark at the middle, the travel
/// from there to the setting, and the cap.
fn paint_fader(window: &mut Window, bounds: Bounds<Pixels>, width: f32, active: bool, live: bool) {
    let (x0, y0, w, h) = frame_of(bounds);
    let centre_x = x0 + w * 0.5;
    let y_of = |value: f32| y0 + (1.0 - value / imager::MAX_WIDTH) * h;
    window.paint_quad(
        fill(rect(centre_x - 3.0, y0, 6.0, h), Colors::meter_bg()).corner_radii(px(3.0)),
    );
    for mark in [0.0, 50.0, 150.0, imager::MAX_WIDTH] {
        window.paint_quad(fill(
            rect(x0 + 3.0, y_of(mark) - 0.5, w - 6.0, 1.0),
            Colors::with_alpha(Colors::text_primary(), 0.10),
        ));
    }
    let unchanged = y_of(imager::DEFAULT_WIDTH);
    window.paint_quad(fill(
        rect(x0, unchanged - 0.5, w, 1.0),
        Colors::with_alpha(Colors::text_primary(), 0.35),
    ));
    let at = y_of(width);
    let tone = if !live {
        Colors::text_disabled()
    } else if width < imager::DEFAULT_WIDTH {
        Colors::text_secondary()
    } else {
        Colors::accent_primary()
    };
    let (top, bottom) = if at < unchanged {
        (at, unchanged)
    } else {
        (unchanged, at)
    };
    window.paint_quad(fill(rect(centre_x - 3.0, top, 6.0, bottom - top), tone));
    let cap = rect(x0 + 2.0, at - 5.0, w - 4.0, 10.0);
    window.paint_quad(
        fill(
            cap,
            if active {
                Colors::accent_primary_hover()
            } else {
                Colors::surface_raised()
            },
        )
        .corner_radii(px(3.0))
        .border_widths(px(1.0))
        .border_color(Colors::border_strong()),
    );
}

// ── Live ────────────────────────────────────────────────────────────────────

pub(crate) fn paint_imager_live(
    params: &BandParams,
    scope: ScopeMode,
    live: &mut Live,
    bounds: &LiveBounds,
    window: &mut Window,
    cx: &mut App,
) {
    let BandParams::Imager(p) = params else {
        return;
    };
    if let Some(b) = bounds.get(LiveSlot::Bands) {
        paint_spectrum(window, live, band_plot(b));
    }
    if let Some(b) = bounds.get(LiveSlot::Scope) {
        paint_vectorscope(window, cx, b, live, scope);
    }
    let frame = live.frame;
    let image = live.image().copied();
    let audible = frame.is_some_and(|f| f.out_rms > 1.0e-4);
    let overall = image.filter(|_| audible).map(|i| i.correlation);
    if let Some(b) = bounds.get(LiveSlot::Correlation(BANDS)) {
        paint_correlation(window, cx, b, overall, true);
    }
    for band in 0..BANDS {
        if let Some(b) = bounds.get(LiveSlot::Correlation(band)) {
            let value = if p.multiband {
                image
                    .filter(|i| i.band_level[band] > 1.0e-4)
                    .map(|i| i.band_correlation[band])
            } else if band == 0 {
                overall
            } else {
                None
            };
            paint_correlation_vertical(window, b, value);
        }
    }
    if let Some(b) = bounds.get(LiveSlot::Meters) {
        let (x0, y0, w, h) = frame_of(b);
        let row = h * 0.5;
        paint_level_bar(
            window,
            cx,
            rect(x0, y0, w, row - 6.0),
            "In",
            frame.map(|f| f.in_peak),
            frame.map(|f| f.in_rms),
        );
        paint_level_bar(
            window,
            cx,
            rect(x0, y0 + row, w, row - 6.0),
            "Out",
            frame.map(|f| f.out_peak),
            frame.map(|f| f.out_rms),
        );
    }
}
