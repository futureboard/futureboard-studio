//! Shared anchor-based overlay positioning for dropdowns, menus, and popovers.

use gpui::{bounds, point, px, size, Bounds, MouseDownEvent, Pixels, Point, Size, Window};

use crate::components::title_bar::TITLEBAR_HEIGHT;

pub const OVERLAY_WINDOW_MARGIN: f32 = 8.0;
pub const COMBO_TRIGGER_HEIGHT: f32 = 30.0;
pub const MENU_LABEL_ESTIMATE_WIDTH: f32 = 72.0;

/// Vertical side a popup opens toward, relative to its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupSide {
    Top,
    Bottom,
}

/// Horizontal alignment of a popup's edge against its anchor's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupAlignment {
    /// Popup's left edge aligns with the anchor's left edge.
    Start,
    /// Popup's right edge aligns with the anchor's right edge.
    End,
}

/// Inputs to [`resolve_popup_placement`]. `gap` is the anchor-to-popup gap on
/// the vertical axis used *only when the popup fits on the preferred side*;
/// once flipped, the same gap is kept between the popup and the anchor on the
/// opposite side.
#[derive(Debug, Clone, Copy)]
pub struct PopupPlacementOptions {
    pub preferred_side: PopupSide,
    pub alignment: PopupAlignment,
    pub viewport_margin: Pixels,
    pub gap: Pixels,
}

/// Output of [`resolve_popup_placement`]: window-space origin/size to paint
/// the popup at, and which side it ended up on (for callers that style the
/// popup differently — e.g. shadow direction — depending on flip state).
#[derive(Debug, Clone, Copy)]
pub struct ResolvedPopupPlacement {
    pub origin: Point<Pixels>,
    pub size: Size<Pixels>,
    pub side: PopupSide,
}

/// Pure popup placement shared by every floating surface (dropdowns, context
/// menus, pickers, tooltips): prefers `options.preferred_side`, flips to the
/// opposite side only when the preferred side cannot hold the popup *and* the
/// opposite side genuinely has more room (never on a stray sub-pixel float),
/// then clamps both axes inside `viewport`.
///
/// All bounds are in the same window-space coordinate system — callers
/// convert local/element bounds into that space before calling this, and the
/// `viewport` passed in should be the usable window content area (not just
/// the immediate parent's bounds), so a popup can never be clipped by an
/// `overflow_hidden`/scrolling ancestor its trigger happens to live inside.
pub fn resolve_popup_placement(
    anchor: Bounds<Pixels>,
    popup_size: Size<Pixels>,
    viewport: Bounds<Pixels>,
    options: PopupPlacementOptions,
) -> ResolvedPopupPlacement {
    let margin: f32 = options.viewport_margin.into();
    let gap: f32 = options.gap.into();

    // Round to whole device pixels before any comparison so a popup that's
    // open doesn't alternate sides from a sub-pixel layout jiggle — the flip
    // decision below is only stable if its inputs are.
    let anchor_left: f32 = f32::from(anchor.origin.x).round();
    let anchor_top: f32 = f32::from(anchor.origin.y).round();
    let anchor_w: f32 = f32::from(anchor.size.width).round();
    let anchor_h: f32 = f32::from(anchor.size.height).round();
    let anchor_right = anchor_left + anchor_w;
    let anchor_bottom = anchor_top + anchor_h;

    let viewport_left: f32 = f32::from(viewport.origin.x).round();
    let viewport_top: f32 = f32::from(viewport.origin.y).round();
    let viewport_right = viewport_left + f32::from(viewport.size.width).round();
    let viewport_bottom = viewport_top + f32::from(viewport.size.height).round();

    let popup_w: f32 = f32::from(popup_size.width).round();
    let popup_h: f32 = f32::from(popup_size.height).round();

    let space_below = viewport_bottom - margin - (anchor_bottom + gap);
    let space_above = (anchor_top - gap) - viewport_top - margin;

    let prefers_bottom = options.preferred_side == PopupSide::Bottom;
    let (space_preferred, space_fallback) = if prefers_bottom {
        (space_below, space_above)
    } else {
        (space_above, space_below)
    };

    let flip = space_preferred < popup_h && space_fallback > space_preferred;
    let side = match (options.preferred_side, flip) {
        (side, false) => side,
        (PopupSide::Bottom, true) => PopupSide::Top,
        (PopupSide::Top, true) => PopupSide::Bottom,
    };

    let available = if flip {
        space_fallback
    } else {
        space_preferred
    }
    .max(0.0);
    let resolved_height = popup_h.min(available).max(0.0);

    let y = match side {
        PopupSide::Bottom => anchor_bottom + gap,
        PopupSide::Top => anchor_top - gap - resolved_height,
    };

    let preferred_x = match options.alignment {
        PopupAlignment::Start => anchor_left,
        PopupAlignment::End => anchor_right - popup_w,
    };
    let min_x = viewport_left + margin;
    let max_x = viewport_right - popup_w - margin;
    let x = preferred_x.clamp(min_x, max_x.max(min_x));

    ResolvedPopupPlacement {
        origin: point(px(x.round()), px(y.round())),
        size: size(px(popup_w.max(0.0)), px(resolved_height)),
        side,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPlacement {
    BottomStart,
    BottomEnd,
    TopStart,
    TopEnd,
    RightStart,
    LeftStart,
    Pointer,
}

#[derive(Debug, Clone, Copy)]
pub struct OverlayAnchor {
    pub bounds: Bounds<Pixels>,
}

impl Default for OverlayAnchor {
    fn default() -> Self {
        Self {
            bounds: bounds(point(px(0.0), px(0.0)), size(px(0.0), px(0.0))),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OverlayPosition {
    pub x: Pixels,
    pub y: Pixels,
    pub width: Option<Pixels>,
    pub max_height: Option<Pixels>,
}

#[derive(Debug, Clone, Copy)]
pub struct OverlaySize {
    pub width: f32,
    pub height: f32,
}

/// Layout of the value column in settings form rows.
#[derive(Debug, Clone, Copy)]
pub struct FormColumnLayout {
    pub value_left: f32,
    pub value_width: f32,
}

/// Where the Preferences window's dropdowns sit: every one is
/// `PREFS_CONTROL_W` wide at the right edge of its group's plate (inside the
/// plate's 1 px border and its row padding), so the column follows the window's
/// width alone.
pub fn settings_form_column(window: &Window) -> FormColumnLayout {
    use crate::components::settings_layout::{
        PREFS_CONTENT_PAD_X, PREFS_CONTROL_W, PREFS_ROW_PAD_X,
    };
    let w: f32 = window.bounds().size.width.into();
    let left = (w - PREFS_CONTENT_PAD_X - 1.0 - PREFS_ROW_PAD_X - PREFS_CONTROL_W).max(0.0);
    FormColumnLayout {
        value_left: left,
        value_width: PREFS_CONTROL_W,
    }
}

/// Build trigger bounds from a form-row combo click using the current window layout.
pub fn form_combo_trigger_bounds(
    layout: FormColumnLayout,
    event: &MouseDownEvent,
    trigger_height: f32,
) -> Bounds<Pixels> {
    let click_y: f32 = event.position.y.into();
    let top = click_y - trigger_height * 0.5;
    bounds(
        point(px(layout.value_left), px(top)),
        size(px(layout.value_width), px(trigger_height)),
    )
}

/// Refresh horizontal geometry on resize while preserving vertical anchor.
pub fn refresh_form_anchor(anchor: OverlayAnchor, layout: FormColumnLayout) -> OverlayAnchor {
    let top: f32 = anchor.bounds.origin.y.into();
    let height: f32 = anchor.bounds.size.height.into();
    OverlayAnchor {
        bounds: bounds(
            point(px(layout.value_left), px(top)),
            size(px(layout.value_width), px(height.max(COMBO_TRIGGER_HEIGHT))),
        ),
    }
}

/// Anchor for a top-menu label (click x ≈ label origin).
pub fn titlebar_label_anchor(click_x: f32) -> OverlayAnchor {
    OverlayAnchor {
        bounds: bounds(
            point(px(click_x), px(0.0)),
            size(px(MENU_LABEL_ESTIMATE_WIDTH), px(TITLEBAR_HEIGHT)),
        ),
    }
}

/// Anchor for the project title button in the title bar.
pub fn project_title_anchor(left_x: f32) -> OverlayAnchor {
    OverlayAnchor {
        bounds: bounds(
            point(px(left_x), px(0.0)),
            size(px(288.0), px(TITLEBAR_HEIGHT)),
        ),
    }
}

/// Anchor at pointer position for context menus.
pub fn pointer_anchor(x: f32, y: f32) -> OverlayAnchor {
    OverlayAnchor {
        bounds: bounds(point(px(x), px(y)), size(px(0.0), px(0.0))),
    }
}

/// Width of the Inspector value column (right-aligned panel).
pub fn inspector_value_width(inspector_width: f32) -> f32 {
    const PAD: f32 = 10.0;
    const LABEL: f32 = 86.0;
    const GAP: f32 = 10.0;
    (inspector_width - PAD * 2.0 - LABEL - GAP).max(80.0)
}

/// Build trigger bounds for an Inspector form-row ComboBox from a click on the
/// trigger. The menu width matches the value column, not the pointer.
pub fn inspector_combo_trigger_bounds(
    window: &Window,
    inspector_width: f32,
    event: &MouseDownEvent,
) -> Bounds<Pixels> {
    const PAD: f32 = 10.0;
    const LABEL: f32 = 86.0;
    const GAP: f32 = 10.0;
    let value_w = inspector_value_width(inspector_width);
    let win_w: f32 = window.bounds().size.width.into();
    let value_left = win_w - inspector_width + PAD + LABEL + GAP;
    let click_y: f32 = event.position.y.into();
    let top = click_y - COMBO_TRIGGER_HEIGHT * 0.5;
    bounds(
        point(px(value_left), px(top)),
        size(px(value_w), px(COMBO_TRIGGER_HEIGHT)),
    )
}

pub fn inspector_combo_menu_position(
    anchor: OverlayAnchor,
    inspector_width: f32,
    menu_height: f32,
    window: &Window,
) -> OverlayPosition {
    let width = inspector_value_width(inspector_width);
    let refreshed = OverlayAnchor {
        bounds: bounds(
            anchor.bounds.origin,
            size(px(width), anchor.bounds.size.height),
        ),
    };
    compute_overlay_position(
        refreshed.bounds,
        OverlaySize {
            width,
            height: menu_height,
        },
        window.bounds(),
        OverlayPlacement::BottomStart,
        4.0,
    )
}

pub fn window_content_bounds(window: &Window) -> Bounds<Pixels> {
    window.bounds()
}

/// Drawable client area below the external dialog title bar (window-local coordinates).
pub fn external_dialog_overlay_bounds(window: &Window) -> Bounds<Pixels> {
    let full = window.bounds();
    let titlebar = TITLEBAR_HEIGHT;
    let full_w: f32 = full.size.width.into();
    let full_h: f32 = full.size.height.into();
    let content_h = (full_h - titlebar).max(0.0);
    bounds(
        point(px(0.0), px(titlebar)),
        size(px(full_w), px(content_h)),
    )
}

/// True when `anchor` lies inside the overlay coordinate space for this window.
pub fn anchor_visible_in_window(anchor: OverlayAnchor, window: &Window) -> bool {
    let content = external_dialog_overlay_bounds(window);
    let anchor_left: f32 = anchor.bounds.origin.x.into();
    let anchor_top: f32 = anchor.bounds.origin.y.into();
    let anchor_w: f32 = anchor.bounds.size.width.into();
    let anchor_h: f32 = anchor.bounds.size.height.into();
    let anchor_right = anchor_left + anchor_w;
    let anchor_bottom = anchor_top + anchor_h;
    let content_left: f32 = content.origin.x.into();
    let content_top: f32 = content.origin.y.into();
    let content_right = content_left + f32::from(content.size.width);
    let content_bottom = content_top + f32::from(content.size.height);
    anchor_left >= content_left - 2.0
        && anchor_top >= content_top - 2.0
        && anchor_right <= content_right + 2.0
        && anchor_bottom <= content_bottom + 2.0
}

/// `Bottom*`/`Pointer` open below the anchor and are the only placements
/// that should auto-flip to open above when there isn't enough room — a
/// caller that explicitly asked for `Top*`/`Right*`/`Left*` chose a fixed
/// direction on purpose (e.g. a submenu known to sit near the bottom edge)
/// and keeps that exact placement unchanged below.
fn bottom_preferring_options(
    placement: OverlayPlacement,
    margin: f32,
) -> Option<PopupPlacementOptions> {
    let (alignment, gap) = match placement {
        OverlayPlacement::BottomStart => (PopupAlignment::Start, margin),
        OverlayPlacement::BottomEnd => (PopupAlignment::End, margin),
        // `Pointer` treats the anchor as the popup's desired top-left corner
        // directly (callers already offset the click position for any
        // visual gap they want), so it keeps its historical zero gap.
        OverlayPlacement::Pointer => (PopupAlignment::Start, 0.0),
        _ => return None,
    };
    Some(PopupPlacementOptions {
        preferred_side: PopupSide::Bottom,
        alignment,
        viewport_margin: px(OVERLAY_WINDOW_MARGIN),
        gap: px(gap),
    })
}

pub fn compute_overlay_position(
    anchor: Bounds<Pixels>,
    overlay_size: OverlaySize,
    window_bounds: Bounds<Pixels>,
    placement: OverlayPlacement,
    margin: f32,
) -> OverlayPosition {
    let anchor_w: f32 = f32::from(anchor.size.width);
    let width = overlay_size.width.max(anchor_w);

    if let Some(options) = bottom_preferring_options(placement, margin) {
        let resolved = resolve_popup_placement(
            anchor,
            size(px(width), px(overlay_size.height)),
            window_bounds,
            options,
        );
        overlay_debug(&format!(
            "type=compute platform={} placement={placement:?} anchor=({:.0},{:.0},{:.0},{:.0}) pos=({:.0},{:.0}) size=({:.0},{:.0}) window=({:.0},{:.0}) flip={}",
            overlay_platform(),
            f32::from(anchor.origin.x),
            f32::from(anchor.origin.y),
            anchor_w,
            f32::from(anchor.size.height),
            f32::from(resolved.origin.x),
            f32::from(resolved.origin.y),
            f32::from(resolved.size.width),
            f32::from(resolved.size.height),
            f32::from(window_bounds.size.width),
            f32::from(window_bounds.size.height),
            resolved.side == PopupSide::Top,
        ));
        return OverlayPosition {
            x: resolved.origin.x,
            y: resolved.origin.y,
            width: Some(resolved.size.width),
            max_height: Some(resolved.size.height),
        };
    }

    let win_w: f32 = window_bounds.size.width.into();
    let win_h: f32 = window_bounds.size.height.into();
    let anchor_left: f32 = anchor.origin.x.into();
    let anchor_top: f32 = anchor.origin.y.into();
    let anchor_h: f32 = f32::from(anchor.size.height);
    let anchor_right = anchor_left + anchor_w;

    let mut height = overlay_size.height;

    let (mut x, mut y) = match placement {
        OverlayPlacement::TopStart => (anchor_left, anchor_top - height - margin),
        OverlayPlacement::TopEnd => (anchor_right - width, anchor_top - height - margin),
        OverlayPlacement::RightStart => (anchor_right + margin, anchor_top),
        OverlayPlacement::LeftStart => (anchor_left - width - margin, anchor_top),
        OverlayPlacement::BottomStart | OverlayPlacement::BottomEnd | OverlayPlacement::Pointer => {
            unreachable!("handled by bottom_preferring_options above")
        }
    };

    if x + width + OVERLAY_WINDOW_MARGIN > win_w {
        x = (win_w - width - OVERLAY_WINDOW_MARGIN).max(OVERLAY_WINDOW_MARGIN);
        overlay_debug(&format!("shift left x={x:.0} win_w={win_w:.0}"));
    }
    if x < OVERLAY_WINDOW_MARGIN {
        x = OVERLAY_WINDOW_MARGIN;
    }
    if y < OVERLAY_WINDOW_MARGIN {
        y = OVERLAY_WINDOW_MARGIN;
    }

    let available_below = (win_h - OVERLAY_WINDOW_MARGIN - y).max(0.0);
    height = available_below.min(height).max(0.0);

    if y + height + OVERLAY_WINDOW_MARGIN > win_h {
        y = (win_h - height - OVERLAY_WINDOW_MARGIN).max(OVERLAY_WINDOW_MARGIN);
    }

    overlay_debug(&format!(
        "type=compute platform={} placement={placement:?} anchor=({anchor_left:.0},{anchor_top:.0},{anchor_w:.0},{anchor_h:.0}) pos=({x:.0},{y:.0}) size=({width:.0},{height:.0}) window=({win_w:.0},{win_h:.0}) flip=false",
        overlay_platform()
    ));

    OverlayPosition {
        x: px(x),
        y: px(y),
        width: Some(px(width)),
        max_height: Some(px(height.max(0.0))),
    }
}

fn overlay_debug(message: &str) {
    if std::env::var_os("FUTUREBOARD_OVERLAY_DEBUG").is_some()
        || std::env::var_os("FUTUREBOARD_COMBOBOX_DEBUG").is_some()
    {
        eprintln!("[overlay] {message}");
    }
}

#[cfg(target_os = "windows")]
fn overlay_platform() -> &'static str {
    "windows"
}

#[cfg(not(target_os = "windows"))]
fn overlay_platform() -> &'static str {
    "other"
}

#[cfg(test)]
mod popup_placement_tests {
    use super::*;

    const VIEWPORT_W: f32 = 1000.0;
    const VIEWPORT_H: f32 = 600.0;
    const MARGIN: f32 = 8.0;
    const GAP: f32 = 4.0;

    fn viewport() -> Bounds<Pixels> {
        bounds(
            point(px(0.0), px(0.0)),
            size(px(VIEWPORT_W), px(VIEWPORT_H)),
        )
    }

    fn anchor_at(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        bounds(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn options(preferred_side: PopupSide, alignment: PopupAlignment) -> PopupPlacementOptions {
        PopupPlacementOptions {
            preferred_side,
            alignment,
            viewport_margin: px(MARGIN),
            gap: px(GAP),
        }
    }

    #[test]
    fn opens_below_when_enough_space_below() {
        let anchor = anchor_at(100.0, 100.0, 120.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(160.0), px(200.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        assert_eq!(resolved.side, PopupSide::Bottom);
        assert_eq!(resolved.origin.y, px(124.0 + GAP));
        assert_eq!(resolved.size.height, px(200.0));
    }

    #[test]
    fn flips_above_when_insufficient_below_but_enough_above() {
        // Anchor near the bottom edge: ~476px above it, ~68px below it in the
        // viewport. A 200px popup cannot fit below but fits comfortably above
        // — this is the exact mixer-output-dropdown-near-the-bottom-edge case.
        let anchor = anchor_at(100.0, 500.0, 120.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(160.0), px(200.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        assert_eq!(resolved.side, PopupSide::Top);
        assert_eq!(
            resolved.size.height,
            px(200.0),
            "must not be squished when it fits above"
        );
        // Popup's bottom edge sits `gap` above the anchor's top edge.
        assert_eq!(resolved.origin.y, px(500.0 - GAP - 200.0));
    }

    #[test]
    fn clamps_height_when_insufficient_on_both_sides() {
        // A popup taller than the whole viewport can't fully fit above or
        // below; whichever side has more room wins and height is clamped
        // instead of overflowing the window.
        let anchor = anchor_at(100.0, 300.0, 120.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(160.0), px(900.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        assert!(resolved.size.height < px(900.0));
        assert!(f32::from(resolved.origin.y) >= 0.0);
        assert!(f32::from(resolved.origin.y) + f32::from(resolved.size.height) <= VIEWPORT_H);
    }

    #[test]
    fn shifts_left_when_anchor_is_near_right_edge() {
        let anchor = anchor_at(950.0, 100.0, 40.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(200.0), px(150.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        let right_edge = f32::from(resolved.origin.x) + f32::from(resolved.size.width);
        assert!(right_edge <= VIEWPORT_W - MARGIN + 0.5);
        assert!(f32::from(resolved.origin.x) >= MARGIN - 0.5);
    }

    #[test]
    fn stays_at_left_margin_when_anchor_is_near_left_edge() {
        let anchor = anchor_at(-30.0, 100.0, 40.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(200.0), px(150.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        assert_eq!(resolved.origin.x, px(MARGIN));
    }

    #[test]
    fn clamps_width_when_popup_is_wider_than_viewport() {
        let anchor = anchor_at(400.0, 100.0, 40.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(2000.0), px(150.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        // The helper does not shrink width (callers measure it), but the
        // origin must still keep the popup's left edge inside the margin so
        // horizontal clamping degrades gracefully instead of pushing it
        // fully off-screen.
        assert_eq!(resolved.origin.x, px(MARGIN));
    }

    #[test]
    fn end_alignment_right_aligns_to_anchor() {
        let anchor = anchor_at(400.0, 100.0, 60.0, 24.0);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(160.0), px(150.0)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::End),
        );
        assert_eq!(resolved.origin.x, px(400.0 + 60.0 - 160.0));
    }

    #[test]
    fn respects_viewport_margin_and_anchor_gap() {
        let anchor = anchor_at(100.0, 100.0, 120.0, 24.0);
        let wide_margin = PopupPlacementOptions {
            preferred_side: PopupSide::Bottom,
            alignment: PopupAlignment::Start,
            viewport_margin: px(40.0),
            gap: px(20.0),
        };
        let resolved =
            resolve_popup_placement(anchor, size(px(160.0), px(200.0)), viewport(), wide_margin);
        assert_eq!(resolved.origin.y, px(124.0 + 20.0));

        // Same anchor, tall popup near the bottom margin — check the margin
        // is actually honored as unusable space, not just cosmetic.
        let anchor_low = anchor_at(100.0, 560.0, 120.0, 24.0);
        let resolved_low = resolve_popup_placement(
            anchor_low,
            size(px(160.0), px(80.0)),
            viewport(),
            wide_margin,
        );
        let bottom_edge = f32::from(resolved_low.origin.y) + f32::from(resolved_low.size.height);
        assert!(bottom_edge <= VIEWPORT_H - 40.0 + 0.5);
    }

    #[test]
    fn handles_fractional_dpi_scaled_coordinates() {
        // Simulates a non-1.0 DPI scale where layout math produces
        // fractional logical-pixel positions (e.g. 1.25x/1.5x scaling).
        let anchor = anchor_at(100.33, 500.67, 120.25, 24.5);
        let resolved = resolve_popup_placement(
            anchor,
            size(px(160.4), px(200.6)),
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        // Rounds cleanly to whole device pixels — no fractional residue that
        // could cause hairline seams or jitter frame-to-frame.
        assert_eq!(f32::from(resolved.origin.x).fract(), 0.0);
        assert_eq!(f32::from(resolved.origin.y).fract(), 0.0);
        assert_eq!(resolved.side, PopupSide::Top);
    }

    #[test]
    fn tiny_subpixel_delta_does_not_flip_side() {
        // Anchor position that fits below by a hair; a 0.1px jiggle in either
        // direction (typical of per-frame layout float noise) must not flip
        // the popup to the opposite side — that's the jitter this rounding
        // step exists to prevent.
        let popup = size(px(160.0), px(200.0));
        let base = resolve_popup_placement(
            anchor_at(100.0, 391.9, 120.0, 24.0),
            popup,
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        let jiggled = resolve_popup_placement(
            anchor_at(100.0, 392.05, 120.0, 24.0),
            popup,
            viewport(),
            options(PopupSide::Bottom, PopupAlignment::Start),
        );
        assert_eq!(base.side, jiggled.side);
    }

    #[test]
    fn mixer_output_dropdown_near_bottom_edge_flips_via_compute_overlay_position() {
        // End-to-end regression test for the reported bug: the mixer output
        // picker uses `OverlayPlacement::Pointer` with a click anchored near
        // the bottom of the window; before this fix `Pointer` never
        // consulted the flip logic at all and the menu was squashed thin
        // instead of opening above the OUT pill.
        let click_anchor = bounds(point(px(300.0), px(560.0)), size(px(0.0), px(0.0)));
        let pos = compute_overlay_position(
            click_anchor,
            OverlaySize {
                width: 180.0,
                height: 220.0,
            },
            bounds(point(px(0.0), px(0.0)), size(px(1200.0), px(600.0))),
            OverlayPlacement::Pointer,
            OVERLAY_WINDOW_MARGIN,
        );
        let bottom_edge = f32::from(pos.y) + f32::from(pos.max_height.unwrap());
        assert!(
            bottom_edge <= 560.0 + 0.5,
            "menu must open above the click point, not overflow past it: bottom_edge={bottom_edge}"
        );
        assert_eq!(
            pos.max_height,
            Some(px(220.0)),
            "must open at full measured height once flipped, not be squeezed"
        );
    }
}
