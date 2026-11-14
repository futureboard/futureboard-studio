//! The first-run tour: a short walk over the Studio's main regions, shown
//! once over the main window the first time it opens, and again from
//! **Help ▸ Welcome Tour**.
//!
//! Each step names one region — the Browser, the arrangement, the transport
//! bar, the Inspector, the editors under the arrangement — dims everything
//! else, rings the region and puts a card beside it. A region that is hidden
//! right now (a closed dock) is left out rather than pointed at empty space.
//!
//! The tour is modal for the pointer and the keyboard while it is up: a
//! transparent layer takes every click, and the Studio's key handler sends
//! keys here first (Escape skips, Enter and → go on, ← goes back). Plug-in
//! editors and the Rigs page are native child windows and stay above it.
//!
//! Region geometry comes from the measured main row (browser | arrangement |
//! inspector) and the dock widths the Studio lays it out with, so the rings
//! use the same numbers as the layout itself. Every string is in the locale
//! catalogs under `tour.*`.

use std::time::Duration;

use gpui::{
    deferred, div, prelude::FluentBuilder, px, Animation, AnimationExt, AnyElement, App, Bounds,
    InteractiveElement, IntoElement, ParentElement, Pixels, Styled, Window,
};

use crate::components::controls::{fb_button, FbButtonKind};
use crate::i18n::I18n;
use crate::theme::{elevation, motion, radius, space, state, typography, Colors};

/// Above menus, popovers and dropdowns: the tour is the one thing on screen.
const TOUR_PRIORITY: usize = 20;
/// Card width: one short paragraph at reading measure.
const CARD_W: f32 = 340.0;
/// Room left between a ring and the card beside it.
const CARD_GAP: f32 = space::SECTION;
/// How far the ring sits outside the region it names.
const RING_INSET: f32 = space::HAIR;

/// A region of the Studio a step points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TourTarget {
    /// No region: the card in the middle of the window.
    Center,
    Browser,
    Arrangement,
    TopBar,
    Inspector,
    BottomPanel,
}

/// One step: where it points, and its catalog keys.
#[derive(Clone, Copy, Debug)]
pub struct TourStep {
    pub target: TourTarget,
    pub title_key: &'static str,
    pub body_key: &'static str,
}

/// Every step, in order. [`steps_for`] drops the ones whose region is hidden.
pub const STEPS: [TourStep; 7] = [
    TourStep {
        target: TourTarget::Center,
        title_key: "tour.welcome.title",
        body_key: "tour.welcome.body",
    },
    TourStep {
        target: TourTarget::Browser,
        title_key: "tour.browser.title",
        body_key: "tour.browser.body",
    },
    TourStep {
        target: TourTarget::Arrangement,
        title_key: "tour.arrangement.title",
        body_key: "tour.arrangement.body",
    },
    TourStep {
        target: TourTarget::TopBar,
        title_key: "tour.transport.title",
        body_key: "tour.transport.body",
    },
    TourStep {
        target: TourTarget::Inspector,
        title_key: "tour.inspector.title",
        body_key: "tour.inspector.body",
    },
    TourStep {
        target: TourTarget::BottomPanel,
        title_key: "tour.editors.title",
        body_key: "tour.editors.body",
    },
    TourStep {
        target: TourTarget::Center,
        title_key: "tour.done.title",
        body_key: "tour.done.body",
    },
];

/// Which regions are on screen now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TourVisibility {
    pub browser: bool,
    pub inspector: bool,
    pub bottom_panel: bool,
}

/// The steps whose region is on screen, in order.
pub fn steps_for(visible: TourVisibility) -> Vec<TourStep> {
    STEPS
        .iter()
        .copied()
        .filter(|step| match step.target {
            TourTarget::Browser => visible.browser,
            TourTarget::Inspector => visible.inspector,
            TourTarget::BottomPanel => visible.bottom_panel,
            TourTarget::Center | TourTarget::Arrangement | TourTarget::TopBar => true,
        })
        .collect()
}

/// The measured geometry the rings are placed from, in window pixels.
#[derive(Clone, Copy, Debug)]
pub struct TourRegions {
    /// The window's content size.
    pub window_w: f32,
    pub window_h: f32,
    /// The main row (browser | arrangement | inspector), once laid out.
    pub main_row: Option<Bounds<Pixels>>,
    pub browser_w: f32,
    pub inspector_w: f32,
    /// The docked bottom panel's height, when it is docked and open.
    pub bottom_h: Option<f32>,
}

/// A region's rectangle: `(x, y, w, h)` in window pixels. `None` for the
/// centred steps, or before the main row's first layout.
pub fn target_rect(target: TourTarget, regions: &TourRegions) -> Option<(f32, f32, f32, f32)> {
    let row = regions.main_row?;
    let (x, y) = (f32::from(row.origin.x), f32::from(row.origin.y));
    let (w, h) = (f32::from(row.size.width), f32::from(row.size.height));
    let rect = match target {
        TourTarget::Center => return None,
        TourTarget::Browser => (x, y, regions.browser_w, h),
        TourTarget::Inspector => (x + w - regions.inspector_w, y, regions.inspector_w, h),
        TourTarget::Arrangement => (
            x + regions.browser_w,
            y,
            w - regions.browser_w - regions.inspector_w,
            h,
        ),
        TourTarget::TopBar => (0.0, 0.0, regions.window_w, y),
        TourTarget::BottomPanel => (0.0, y + h, regions.window_w, regions.bottom_h?),
    };
    (rect.2 > 1.0 && rect.3 > 1.0).then_some(rect)
}

/// Where the card goes for a ring at `rect`: beside a dock, under the top
/// bar, over the bottom panel, inside the arrangement, always in the window.
/// `card_h` is an estimate; the card is clamped with it.
pub fn card_origin(
    target: TourTarget,
    rect: Option<(f32, f32, f32, f32)>,
    window_w: f32,
    window_h: f32,
    card_h: f32,
) -> (f32, f32) {
    let centred = ((window_w - CARD_W) * 0.5, (window_h - card_h) * 0.5);
    let Some((x, y, w, h)) = rect else {
        return clamp_card(centred, window_w, window_h, card_h);
    };
    let origin = match target {
        TourTarget::Center => centred,
        TourTarget::Browser => (x + w + CARD_GAP, y + space::BLOCK),
        TourTarget::Inspector => (x - CARD_W - CARD_GAP, y + space::BLOCK),
        TourTarget::TopBar => (x + (w - CARD_W) * 0.5, y + h + CARD_GAP),
        TourTarget::BottomPanel => (x + (w - CARD_W) * 0.5, y - card_h - CARD_GAP),
        TourTarget::Arrangement => (x + (w - CARD_W) * 0.5, y + (h - card_h) * 0.5),
    };
    clamp_card(origin, window_w, window_h, card_h)
}

fn clamp_card(origin: (f32, f32), window_w: f32, window_h: f32, card_h: f32) -> (f32, f32) {
    let margin = space::SECTION;
    let max_x = (window_w - CARD_W - margin).max(margin);
    let max_y = (window_h - card_h - margin).max(margin);
    (origin.0.clamp(margin, max_x), origin.1.clamp(margin, max_y))
}

/// What the card's buttons do.
pub struct TourActions {
    pub on_back: Box<dyn Fn(&mut Window, &mut App) + 'static>,
    pub on_next: Box<dyn Fn(&mut Window, &mut App) + 'static>,
    pub on_skip: Box<dyn Fn(&mut Window, &mut App) + 'static>,
}

/// The tour over the window at step `index` of `steps`.
pub fn render_tour(
    steps: &[TourStep],
    index: usize,
    regions: &TourRegions,
    i18n: I18n,
    actions: TourActions,
) -> Option<AnyElement> {
    let step = *steps.get(index)?;
    let last = index + 1 == steps.len();
    let rect = target_rect(step.target, regions);
    // Measured cards differ by language; this covers a title and four lines.
    let card_h = 196.0;
    let (card_x, card_y) = card_origin(
        step.target,
        rect,
        regions.window_w,
        regions.window_h,
        card_h,
    );

    let scrim = Colors::state_scrim();
    let mut layer = div()
        .id("welcome-tour")
        .absolute()
        .top_0()
        .left_0()
        .w(px(regions.window_w))
        .h(px(regions.window_h))
        // Modal for the pointer: nothing under the tour takes a click.
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation());

    // The dim: four bands around the ring's hole, or the whole window.
    match rect {
        Some((x, y, w, h)) => {
            let (x0, y0) = ((x - RING_INSET).max(0.0), (y - RING_INSET).max(0.0));
            let (x1, y1) = (
                (x + w + RING_INSET).min(regions.window_w),
                (y + h + RING_INSET).min(regions.window_h),
            );
            for (bx, by, bw, bh) in [
                (0.0, 0.0, regions.window_w, y0),
                (0.0, y1, regions.window_w, regions.window_h - y1),
                (0.0, y0, x0, y1 - y0),
                (x1, y0, regions.window_w - x1, y1 - y0),
            ] {
                if bw > 0.0 && bh > 0.0 {
                    layer = layer.child(band(bx, by, bw, bh, scrim));
                }
            }
            // The ring: the focus language, since it says "here".
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(x0))
                    .top(px(y0))
                    .w(px(x1 - x0))
                    .h(px(y1 - y0))
                    .rounded(px(radius::CONTROL))
                    .border(px(state::FOCUS_RING_PX))
                    .border_color(Colors::accent_primary())
                    .with_animation(
                        gpui::ElementId::NamedInteger("welcome-tour-ring".into(), index as u64),
                        Animation::new(Duration::from_millis(motion::FAST_MS))
                            .with_easing(gpui::ease_out_quint()),
                        |ring, t| ring.opacity(t),
                    ),
            );
        }
        None => {
            layer = layer.child(band(0.0, 0.0, regions.window_w, regions.window_h, scrim));
        }
    }

    let progress = i18n.tr_vars(
        "tour.progress",
        &[
            ("current", (index + 1).to_string()),
            ("total", steps.len().to_string()),
        ],
    );
    let TourActions {
        on_back,
        on_next,
        on_skip,
    } = actions;

    let card = div()
        .id("welcome-tour-card")
        .absolute()
        .left(px(card_x))
        .top(px(card_y))
        .w(px(CARD_W))
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .p(px(space::SECTION))
        .rounded(px(radius::DIALOG))
        .bg(Colors::surface_raised())
        .border(px(1.0))
        .border_color(Colors::border_normal())
        .shadow(elevation::shadow(elevation::OVERLAY))
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(progress),
        )
        .child(
            div()
                .text_size(px(typography::UI_LG))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(i18n.tr(step.title_key)),
        )
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .line_height(gpui::relative(1.45))
                .text_color(Colors::text_secondary())
                .child(i18n.tr(step.body_key)),
        )
        .child(
            div()
                .mt(px(space::TIGHT))
                .flex()
                .items_center()
                .gap(px(space::BASE))
                // Buttons keep their label on one line; the spacer gives way.
                .when(!last, |row| {
                    row.child(div().flex_none().whitespace_nowrap().child(fb_button(
                        "welcome-tour-skip",
                        i18n.tr("tour.skip"),
                        FbButtonKind::Ghost,
                        true,
                        move |_, window, cx| on_skip(window, cx),
                    )))
                })
                .child(div().flex_1().min_w_0())
                .when(index > 0, |row| {
                    row.child(div().flex_none().whitespace_nowrap().child(fb_button(
                        "welcome-tour-back",
                        i18n.tr("tour.back"),
                        FbButtonKind::Default,
                        true,
                        move |_, window, cx| on_back(window, cx),
                    )))
                })
                .child(div().flex_none().whitespace_nowrap().child(fb_button(
                    "welcome-tour-next",
                    i18n.tr(if last { "tour.finish" } else { "tour.next" }),
                    FbButtonKind::Primary,
                    true,
                    move |_, window, cx| on_next(window, cx),
                ))),
        )
        // Each step's card rises into place; the motion says "this changed".
        .with_animation(
            gpui::ElementId::NamedInteger("welcome-tour-card".into(), index as u64),
            Animation::new(Duration::from_millis(motion::FAST_MS))
                .with_easing(gpui::ease_out_quint()),
            move |card, t| card.top(px(card_y + space::BASE * (1.0 - t))).opacity(t),
        );

    Some(
        deferred(layer.child(card))
            .with_priority(TOUR_PRIORITY)
            .into_any_element(),
    )
}

fn band(x: f32, y: f32, w: f32, h: f32, fill: gpui::Rgba) -> impl IntoElement {
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(h))
        .bg(fill)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regions() -> TourRegions {
        TourRegions {
            window_w: 1600.0,
            window_h: 1000.0,
            main_row: Some(Bounds {
                origin: gpui::point(px(0.0), px(80.0)),
                size: gpui::size(px(1600.0), px(600.0)),
            }),
            browser_w: 300.0,
            inspector_w: 320.0,
            bottom_h: Some(290.0),
        }
    }

    #[test]
    fn regions_follow_the_main_row_and_dock_widths() {
        let r = regions();
        assert_eq!(
            target_rect(TourTarget::Browser, &r),
            Some((0.0, 80.0, 300.0, 600.0))
        );
        assert_eq!(
            target_rect(TourTarget::Inspector, &r),
            Some((1280.0, 80.0, 320.0, 600.0))
        );
        assert_eq!(
            target_rect(TourTarget::Arrangement, &r),
            Some((300.0, 80.0, 980.0, 600.0))
        );
        assert_eq!(
            target_rect(TourTarget::TopBar, &r),
            Some((0.0, 0.0, 1600.0, 80.0))
        );
        assert_eq!(
            target_rect(TourTarget::BottomPanel, &r),
            Some((0.0, 680.0, 1600.0, 290.0))
        );
        assert_eq!(target_rect(TourTarget::Center, &r), None);
        let unmeasured = TourRegions {
            main_row: None,
            ..r
        };
        assert_eq!(target_rect(TourTarget::Browser, &unmeasured), None);
    }

    #[test]
    fn hidden_regions_are_left_out() {
        let all = steps_for(TourVisibility {
            browser: true,
            inspector: true,
            bottom_panel: true,
        });
        assert_eq!(all.len(), STEPS.len());
        let bare = steps_for(TourVisibility {
            browser: false,
            inspector: false,
            bottom_panel: false,
        });
        let targets: Vec<_> = bare.iter().map(|s| s.target).collect();
        assert_eq!(
            targets,
            vec![
                TourTarget::Center,
                TourTarget::Arrangement,
                TourTarget::TopBar,
                TourTarget::Center
            ]
        );
    }

    #[test]
    fn cards_sit_beside_their_region_inside_the_window() {
        let r = regions();
        let browser = target_rect(TourTarget::Browser, &r);
        let (x, _) = card_origin(TourTarget::Browser, browser, 1600.0, 1000.0, 196.0);
        assert!(x >= 300.0, "beside the browser, not over it");
        let inspector = target_rect(TourTarget::Inspector, &r);
        let (x, _) = card_origin(TourTarget::Inspector, inspector, 1600.0, 1000.0, 196.0);
        assert!(x + CARD_W <= 1280.0, "beside the inspector, not over it");
        // A tiny window still keeps the card on screen.
        let (x, y) = card_origin(TourTarget::Center, None, 200.0, 150.0, 196.0);
        assert!(x >= 0.0 && y >= 0.0);
    }
}
