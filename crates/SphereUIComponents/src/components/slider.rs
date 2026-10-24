//! Horizontal sliders: the unipolar level slider used by the timeline
//! TrackHeader volume row, and the bipolar centre-origin slider used by the
//! Inspector pan row.
//!
//! Drag-driven. The slider's hitbox is intentionally taller than its visible
//! rail so vertical wiggle during a drag does not lose tracking. Mouse-down
//! does **not** snap value-to-cursor, and a drag moves the value by the
//! pointer's travel rather than to its position (Shift for fine), so grabbing
//! the rail anywhere never jumps the level.
//!
//! Both variants share one element body ([`drag_slider`]) so paint, hit-test,
//! accessibility and gesture handling cannot drift apart between them. What
//! differs is captured in [`SliderSpec`]: the value domain, where the fill
//! starts, whether there is a neutral detent, and whether dragged values are
//! quantised.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AccessibleAction, App, AppContext, DragMoveEvent, InteractiveElement, IntoElement,
    Orientation, ParentElement, Role, StatefulInteractiveElement, Styled, Window,
};
use std::sync::Arc;

use crate::components::fader::{drag_to, is_reset_double_click, FaderDrag};
use crate::theme::{elevation, radius, Colors};

type SliderValueCb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
type SliderCommitCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;

// ── Geometry ────────────────────────────────────────────────────────────────
// Fixed chrome, not data-driven: named here rather than repeated inline so the
// rail, the detent, the glow and the thumb stay on one vertical rhythm.
const TRACK_H: f32 = 20.0;
const RAIL_TOP: f32 = 7.0;
const RAIL_H: f32 = 6.0;
const DETENT_TOP: f32 = 5.0;
const DETENT_W: f32 = 1.0;
const DETENT_H: f32 = 10.0;
const GLOW_TOP: f32 = 2.0;
const GLOW_W: f32 = 12.0;
const GLOW_H: f32 = 16.0;
const THUMB_TOP: f32 = 3.0;
const THUMB_W: f32 = 8.0;
const THUMB_H: f32 = 14.0;
const THUMB_NOTCH_TOP: f32 = 3.0;
const THUMB_NOTCH_INSET: f32 = 2.0;
const THUMB_NOTCH_H: f32 = 1.5;

const COMPACT_TRACK_H: f32 = 10.0;
const COMPACT_RAIL_TOP: f32 = 3.5;
const COMPACT_RAIL_H: f32 = 3.0;

/// Half-width of the snap-to-neutral dead zone, in **window pixels**.
///
/// Pixels, not a fraction of the value range: the Inspector's pan rail renders
/// at roughly 120 px (292 px panel − 86 px label − gaps − a 48 px readout), so
/// a dead zone expressed as a fraction of travel would come out narrower than
/// gpui's own 2 px `DRAG_THRESHOLD` and the detent would be physically
/// unreachable.
const CENTER_SNAP_PX: f32 = 4.0;

/// Rounding applied to bipolar drag values, matching the pan quantum the mixer
/// knob (`knob.rs`) and the TrackHeader pan pill already use.
const BIPOLAR_QUANTUM: f32 = 0.001;

fn accessible_label_from_id(id: &str) -> String {
    id.replace(['-', '_'], " ")
}

/// Where a slider's fill starts on the rail.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FillOrigin {
    /// Fill grows from the left edge — level controls whose zero is the
    /// minimum of the range.
    Start,
    /// Fill grows outward from the rail midpoint — bipolar controls whose
    /// neutral value sits in the middle of the range (track pan).
    Center,
}

/// Value domain, fill origin and gesture rules for one slider variant.
#[derive(Clone, Copy)]
struct SliderSpec {
    min: f32,
    max: f32,
    /// Keyboard / assistive-technology step.
    step: f32,
    origin: FillOrigin,
    /// Half-width of the snap-to-neutral dead zone in window pixels; `0.0`
    /// disables snapping.
    snap_px: f32,
    /// Rounding applied to dragged values. `None` leaves them untouched — the
    /// unipolar slider drives hosted-plugin parameters
    /// (`effect_editor_tab_view.rs`), whose resolution is not ours to quantise.
    quantum: Option<f32>,
}

impl SliderSpec {
    const fn unipolar() -> Self {
        Self {
            min: 0.0,
            max: 1.0,
            step: 0.01,
            origin: FillOrigin::Start,
            snap_px: 0.0,
            quantum: None,
        }
    }

    const fn bipolar() -> Self {
        Self {
            min: -1.0,
            max: 1.0,
            step: 0.01,
            origin: FillOrigin::Center,
            snap_px: CENTER_SNAP_PX,
            quantum: Some(BIPOLAR_QUANTUM),
        }
    }

    fn span(&self) -> f32 {
        (self.max - self.min).max(f32::EPSILON)
    }

    /// Track position of the fill origin, `0.0..=1.0`.
    fn origin_pos(&self) -> f32 {
        match self.origin {
            FillOrigin::Start => 0.0,
            FillOrigin::Center => 0.5,
        }
    }

    /// The value the fill grows away from.
    fn neutral(&self) -> f32 {
        self.min + self.origin_pos() * self.span()
    }

    /// Track position of `value`, `0.0..=1.0`. Paint and hit-test both go
    /// through this, so the drawn thumb is always where the pointer maps to.
    fn position_of(&self, value: f32) -> f32 {
        ((value - self.min) / self.span()).clamp(0.0, 1.0)
    }

    fn quantise(&self, value: f32) -> f32 {
        match self.quantum {
            Some(q) if q > 0.0 => (value / q).round() * q,
            _ => value,
        }
    }
}

/// The value a drag that started at track position `start_pos` reaches with
/// the pointer at window x `pointer_x`.
///
/// Relative, like the faders and knobs: the thumb moves by how far the pointer
/// travels across the rail, never to where it is. Mapping the pointer straight
/// onto a 70–120 px rail jumped the value to wherever the press landed and made
/// every pixel a coarse step. Shift drags at a tenth of the rate.
fn dragged_value(
    drag: &FaderDrag,
    start_pos: f32,
    pointer_x: f32,
    rail_w: f32,
    fine: bool,
    spec: &SliderSpec,
) -> f32 {
    let pos = drag_to(drag, start_pos, pointer_x, fine, rail_w, |dx| dx);
    (spec.min + pos * spec.span()).clamp(spec.min, spec.max)
}

/// Pull a value inside the dead zone onto the neutral point.
///
/// `unsnap` (Shift) drags straight through it, matching the piano roll's
/// unsnap modifier. `snap_px <= 0.0` disables snapping entirely.
fn apply_center_snap(
    value: f32,
    neutral: f32,
    snap_px: f32,
    bounds_w: f32,
    spec: &SliderSpec,
    unsnap: bool,
) -> f32 {
    if snap_px <= 0.0 || unsnap {
        return value;
    }
    let half_width = snap_px / bounds_w.max(1.0) * spec.span();
    if (value - neutral).abs() <= half_width {
        neutral
    } else {
        value
    }
}

/// `(left, width)` of the fill as fractions of the rail, for a track position
/// `pos` and a fill origin `origin_pos` (both `0.0..=1.0`).
fn fill_span(pos: f32, origin_pos: f32) -> (f32, f32) {
    (pos.min(origin_pos), (pos - origin_pos).abs())
}

/// Step a value by `delta` (negative to decrement), landing on the step grid.
///
/// A dragged value carries a fractional remainder — pan 0.037, say — and
/// stepping it blindly would keep that remainder forever, so a keyboard user
/// could walk past exact neutral without ever landing on it. Off the grid, the
/// first press moves to the nearest grid point in the direction of travel.
fn stepped(value: f32, delta: f32, spec: &SliderSpec) -> f32 {
    let grid = delta.abs();
    if grid <= 0.0 {
        return value.clamp(spec.min, spec.max);
    }
    let on_grid = ((value / grid).round() * grid - value).abs() <= grid * 1.0e-3;
    let next = if on_grid {
        value + delta
    } else if delta > 0.0 {
        (value / grid).ceil() * grid
    } else {
        (value / grid).floor() * grid
    };
    next.clamp(spec.min, spec.max)
}

/// Render a horizontal slider.
///
/// * `id`           – stable id (also stamped into the drag marker so move
///                    events can be filtered by which slider started the drag).
/// * `value_norm`   – current value in `0.0..=1.0`.
/// * `accent`       – fill color (usually the track color).
/// * `on_change`    – called with the new normalized value each time the user
///                    drags. Wire this into a `TimelineState::set_track_volume`
///                    callback.
pub fn slider(
    id: impl Into<gpui::SharedString>,
    value_norm: f32,
    accent: gpui::Rgba,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    slider_with_reset(
        id,
        value_norm,
        accent,
        on_change,
        None::<fn(&mut Window, &mut App)>,
    )
}

pub fn slider_with_reset(
    id: impl Into<gpui::SharedString>,
    value_norm: f32,
    accent: gpui::Rgba,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_double_click_reset: Option<impl Fn(&mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    slider_with_drag_callbacks(
        id,
        value_norm,
        accent,
        None::<fn(&f32, &mut Window, &mut App)>,
        Some(on_change),
        None::<fn(&mut Window, &mut App)>,
        on_double_click_reset,
    )
}

/// Compact fill-only slider for dense rows (mixer send gain). No thumb — value
/// is read from the filled rail length. Smaller hit/rail than [`slider`].
pub fn compact_slider_with_reset(
    id: impl Into<gpui::SharedString>,
    value_norm: f32,
    accent: gpui::Rgba,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
    on_double_click_reset: Option<impl Fn(&mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let id_str: gpui::SharedString = id.into();
    let id_string = id_str.to_string();
    let accessible_label = accessible_label_from_id(&id_string);
    let value = value_norm.clamp(0.0, 1.0);
    let on_change: SliderValueCb = Arc::new(on_change);
    let on_change_drag = on_change.clone();
    let on_change_increment = on_change.clone();
    let on_change_decrement = on_change.clone();
    let focus = Colors::state_focus_ring();
    let fill = {
        let mut c = accent;
        c.a = 0.9;
        c
    };

    div()
        .id(gpui::ElementId::Name(id_str.clone()))
        .role(Role::Slider)
        .aria_label(accessible_label)
        .aria_numeric_value(value as f64)
        .aria_min_numeric_value(0.0)
        .aria_max_numeric_value(1.0)
        .aria_orientation(Orientation::Horizontal)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .on_a11y_action(AccessibleAction::Increment, move |_, window, cx| {
            let next = (value + 0.01).min(1.0);
            on_change_increment(&next, window, cx);
        })
        .on_a11y_action(AccessibleAction::Decrement, move |_, window, cx| {
            let next = (value - 0.01).max(0.0);
            on_change_decrement(&next, window, cx);
        })
        .h(px(COMPACT_TRACK_H))
        .flex_1()
        .min_w_0()
        .relative()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(COMPACT_RAIL_TOP))
                .h(px(COMPACT_RAIL_H))
                .bg(Colors::divider())
                .rounded(px(radius::PILL)),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(COMPACT_RAIL_TOP))
                .h(px(COMPACT_RAIL_H))
                .w(gpui::relative(value))
                .bg(fill)
                .rounded(px(radius::PILL)),
        )
        .on_drag(
            FaderDrag::new(id_string.clone()),
            move |drag, _offset, _window, cx| cx.new(|_| FaderDrag::new(drag.id.clone())),
        )
        .on_drag_move::<FaderDrag>(move |event: &DragMoveEvent<FaderDrag>, window, cx| {
            let drag = event.drag(cx);
            if drag.id != id_string {
                return;
            }
            let new_value = dragged_value(
                drag,
                value,
                event.event.position.x.into(),
                f32::from(event.bounds.size.width).max(1.0),
                event.event.modifiers.shift,
                &SliderSpec::unipolar(),
            );
            on_change_drag(&new_value, window, cx);
        })
        .when_some(on_double_click_reset, |this, reset| {
            this.on_click(move |event, window, cx| {
                if is_reset_double_click(event) {
                    reset(window, cx);
                }
            })
        })
}

/// Unipolar level slider with the full drag lifecycle: begin, live preview and
/// a single commit at mouse-up. Values are `0.0..=1.0`.
pub fn slider_with_drag_callbacks(
    id: impl Into<gpui::SharedString>,
    value_norm: f32,
    accent: gpui::Rgba,
    on_drag_start: Option<impl Fn(&f32, &mut Window, &mut App) + 'static>,
    on_drag_preview: Option<impl Fn(&f32, &mut Window, &mut App) + 'static>,
    on_drag_commit: Option<impl Fn(&mut Window, &mut App) + 'static>,
    on_double_click_reset: Option<impl Fn(&mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    drag_slider(
        id.into(),
        value_norm,
        SliderSpec::unipolar(),
        None,
        accent,
        on_drag_start.map(|callback| Arc::new(callback) as SliderValueCb),
        on_drag_preview.map(|callback| Arc::new(callback) as SliderValueCb),
        on_drag_commit.map(|callback| Arc::new(callback) as SliderCommitCb),
        on_double_click_reset.map(|callback| Arc::new(callback) as SliderCommitCb),
    )
}

/// Bipolar slider for a centre-neutral parameter (track pan, `-1.0..=1.0`).
///
/// The fill grows **outward from the rail centre** — left of centre for L,
/// right for R — so neutral reads as empty rather than half-full, and a centre
/// detent tick marks it. Values in and values out are the real parameter
/// range: the caller does no `0..1` remapping, so what the screen reader
/// announces and what the engine receives are the same number.
///
/// Dragging within [`CENTER_SNAP_PX`] of centre snaps to exact neutral; hold
/// Shift to drag through the detent (the unsnap modifier the piano roll uses).
/// Double-click resets, guarded by [`is_reset_double_click`] so two
/// consecutive drags cannot fire it.
///
/// * `value_label` – the readout text (e.g. `"L 50"`, `"Center"`), announced
///   in place of the raw number. `None` falls back to the numeric value.
#[allow(clippy::too_many_arguments)]
pub fn bipolar_slider_with_drag_callbacks(
    id: impl Into<gpui::SharedString>,
    value: f32,
    accent: gpui::Rgba,
    value_label: Option<impl Into<gpui::SharedString>>,
    on_drag_start: Option<impl Fn(&f32, &mut Window, &mut App) + 'static>,
    on_drag_preview: Option<impl Fn(&f32, &mut Window, &mut App) + 'static>,
    on_drag_commit: Option<impl Fn(&mut Window, &mut App) + 'static>,
    on_double_click_reset: Option<impl Fn(&mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    drag_slider(
        id.into(),
        value,
        SliderSpec::bipolar(),
        value_label.map(Into::into),
        accent,
        on_drag_start.map(|callback| Arc::new(callback) as SliderValueCb),
        on_drag_preview.map(|callback| Arc::new(callback) as SliderValueCb),
        on_drag_commit.map(|callback| Arc::new(callback) as SliderCommitCb),
        on_double_click_reset.map(|callback| Arc::new(callback) as SliderCommitCb),
    )
}

/// The shared element body behind [`slider_with_drag_callbacks`] and
/// [`bipolar_slider_with_drag_callbacks`].
///
/// Takes already-`Arc`'d callbacks so the two public wrappers stay thin and the
/// body is monomorphised once.
#[allow(clippy::too_many_arguments)]
fn drag_slider(
    id_str: gpui::SharedString,
    raw_value: f32,
    spec: SliderSpec,
    value_label: Option<gpui::SharedString>,
    accent: gpui::Rgba,
    on_drag_start: Option<SliderValueCb>,
    on_drag_preview: Option<SliderValueCb>,
    on_drag_commit: Option<SliderCommitCb>,
    on_double_click_reset: Option<SliderCommitCb>,
) -> impl IntoElement {
    let id_string = id_str.to_string();
    let accessible_label = accessible_label_from_id(&id_string);
    let value = raw_value.clamp(spec.min, spec.max);
    let pos = spec.position_of(value);
    let origin_pos = spec.origin_pos();
    let (fill_left, fill_width) = fill_span(pos, origin_pos);

    let a11y_start_increment = on_drag_start.clone();
    let a11y_preview_increment = on_drag_preview.clone();
    let a11y_commit_increment = on_drag_commit.clone();
    let a11y_start_decrement = on_drag_start.clone();
    let a11y_preview_decrement = on_drag_preview.clone();
    let a11y_commit_decrement = on_drag_commit.clone();

    let focus = Colors::state_focus_ring();
    let fill = {
        let mut c = accent;
        c.a = 0.95;
        c
    };
    let mut accent_glow = accent;
    accent_glow.a = 0.35;

    div()
        .id(gpui::ElementId::Name(id_str.clone()))
        .role(Role::Slider)
        .aria_label(accessible_label)
        .aria_numeric_value(value as f64)
        .aria_min_numeric_value(spec.min as f64)
        .aria_max_numeric_value(spec.max as f64)
        .aria_orientation(Orientation::Horizontal)
        // Announce the readout the user can see ("Center", "L 50") rather than
        // a bare number whose sign convention is invisible to a screen reader.
        .when_some(value_label, |this, label| this.aria_value(label))
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .on_a11y_action(AccessibleAction::Increment, move |_, window, cx| {
            let next = stepped(value, spec.step, &spec);
            if let Some(start) = a11y_start_increment.as_ref() {
                start(&value, window, cx);
            }
            if let Some(preview) = a11y_preview_increment.as_ref() {
                preview(&next, window, cx);
            }
            if let Some(commit) = a11y_commit_increment.as_ref() {
                commit(window, cx);
            }
        })
        .on_a11y_action(AccessibleAction::Decrement, move |_, window, cx| {
            let next = stepped(value, -spec.step, &spec);
            if let Some(start) = a11y_start_decrement.as_ref() {
                start(&value, window, cx);
            }
            if let Some(preview) = a11y_preview_decrement.as_ref() {
                preview(&next, window, cx);
            }
            if let Some(commit) = a11y_commit_decrement.as_ref() {
                commit(window, cx);
            }
        })
        // Generous vertical hit area so the user can drift up/down during drag.
        .h(px(TRACK_H))
        .flex_1()
        .min_w_0()
        .relative()
        .cursor(gpui::CursorStyle::ResizeLeftRight)
        // Recessed rail
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(RAIL_TOP))
                .h(px(RAIL_H))
                .bg(Colors::divider())
                .border(px(1.0))
                .border_color(Colors::with_alpha(Colors::surface_canvas(), 0.25))
                .rounded(px(radius::PILL)),
        )
        // Fill bar. Bipolar sliders grow it outward from the rail midpoint, so
        // neutral is an empty rail and the fill's length *is* the offset from
        // neutral. A zero-width pill would paint as a stray blob, so at neutral
        // there is simply no fill child.
        .when(fill_width > f32::EPSILON, |this| {
            this.child(
                div()
                    .absolute()
                    .left(gpui::relative(fill_left))
                    .top(px(RAIL_TOP))
                    .h(px(RAIL_H))
                    .w(gpui::relative(fill_width))
                    .bg(fill)
                    .rounded(px(radius::PILL)),
            )
        })
        // Centre detent. Painted after the fill so it stays readable as the
        // fill sweeps past it; `text_muted` is the same token the bipolar knob
        // uses for its zero tick, and unlike `fader_tick` it is opaque enough
        // to be seen over the rail.
        .when(spec.origin == FillOrigin::Center, |this| {
            this.child(
                div()
                    .absolute()
                    .left(gpui::relative(origin_pos))
                    .ml(px(-DETENT_W / 2.0))
                    .top(px(DETENT_TOP))
                    .w(px(DETENT_W))
                    .h(px(DETENT_H))
                    .bg(Colors::text_muted()),
            )
        })
        // Soft glow halo behind the thumb so it reads as the active element.
        .child(
            div()
                .absolute()
                .top(px(GLOW_TOP))
                .left(gpui::relative(pos))
                .ml(px(-GLOW_W / 2.0))
                .w(px(GLOW_W))
                .h(px(GLOW_H))
                .rounded(px(radius::PILL))
                .bg(accent_glow),
        )
        // Handle thumb — bordered, brighter, and a hair taller than the rail.
        .child(
            div()
                .absolute()
                .top(px(THUMB_TOP))
                .left(gpui::relative(pos))
                .ml(px(-THUMB_W / 2.0))
                .w(px(THUMB_W))
                .h(px(THUMB_H))
                .rounded(px(radius::MICRO))
                .bg(Colors::text_primary())
                .border(px(1.0))
                .border_color(Colors::border_strong())
                // Center accent notch inside the handle so the user can read
                // which slider they're touching at a glance.
                .child(
                    div()
                        .absolute()
                        .top(px(THUMB_NOTCH_TOP))
                        .left(px(THUMB_NOTCH_INSET))
                        .right(px(THUMB_NOTCH_INSET))
                        .h(px(THUMB_NOTCH_H))
                        .bg(accent),
                ),
        )
        .on_drag(
            FaderDrag::new(id_string.clone()),
            move |drag, _offset, window, cx| {
                if let Some(start) = on_drag_start.as_ref() {
                    start(&value, window, cx);
                }
                cx.new(|_| FaderDrag::new(drag.id.clone()))
            },
        )
        .on_drag_move::<FaderDrag>(move |event: &DragMoveEvent<FaderDrag>, window, cx| {
            let drag = event.drag(cx);
            if drag.id != id_string {
                return;
            }
            let ow: f32 = f32::from(event.bounds.size.width).max(1.0);
            let new_value = dragged_value(
                drag,
                pos,
                event.event.position.x.into(),
                ow,
                event.event.modifiers.shift,
                &spec,
            );
            let new_value = apply_center_snap(
                new_value,
                spec.neutral(),
                spec.snap_px,
                ow,
                &spec,
                event.event.modifiers.shift,
            );
            let new_value = spec.quantise(new_value);
            if let Some(preview) = on_drag_preview.as_ref() {
                preview(&new_value, window, cx);
            }
        })
        .when_some(on_drag_commit, |this, commit| {
            this.on_mouse_up(gpui::MouseButton::Left, {
                let commit = commit.clone();
                move |_event, window, cx| commit(window, cx)
            })
            .on_mouse_up_out(gpui::MouseButton::Left, move |_event, window, cx| {
                commit(window, cx)
            })
        })
        .when_some(on_double_click_reset, |this, reset| {
            this.on_click(move |event, window, cx| {
                // `click_count() >= 2` alone also fires for two consecutive
                // drags begun near the same point, which would silently stomp
                // the value the user just set. See `fader::is_reset_double_click`.
                if is_reset_double_click(event) {
                    reset(window, cx);
                }
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_moves_by_pointer_travel_not_to_the_pointer() {
        let spec = SliderSpec::unipolar();
        let drag = FaderDrag::new("s".into());
        // Grabbed at 0.25 with the pointer far from the thumb: no jump.
        assert!((dragged_value(&drag, 0.25, 500.0, 200.0, false, &spec) - 0.25).abs() < 1.0e-6);
        // 20 px of a 200 px rail is a tenth of the travel.
        assert!((dragged_value(&drag, 0.25, 520.0, 200.0, false, &spec) - 0.35).abs() < 1.0e-6);
        // Clamped to the domain.
        assert!((dragged_value(&drag, 0.25, 900.0, 200.0, false, &spec) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn shift_drags_a_bipolar_slider_finely_without_a_jump() {
        let spec = SliderSpec::bipolar();
        let drag = FaderDrag::new("p".into());
        // Pan centre on a 120 px rail; 12 px right is a tenth of the rail.
        assert!(dragged_value(&drag, 0.5, 300.0, 120.0, false, &spec).abs() < 1.0e-6);
        assert!((dragged_value(&drag, 0.5, 312.0, 120.0, false, &spec) - 0.2).abs() < 1.0e-5);
        // Pressing Shift continues from there at a tenth of the rate.
        assert!((dragged_value(&drag, 0.5, 312.0, 120.0, true, &spec) - 0.2).abs() < 1.0e-5);
        assert!((dragged_value(&drag, 0.5, 324.0, 120.0, true, &spec) - 0.22).abs() < 1.0e-5);
    }

    #[test]
    fn centre_detent_snaps_and_shift_drags_through_it() {
        let spec = SliderSpec::bipolar();
        let neutral = spec.neutral();
        // 4 px of a 120 px rail spanning 2.0 units == 0.0667 units either side.
        assert!(
            apply_center_snap(0.05, neutral, CENTER_SNAP_PX, 120.0, &spec, false).abs() < 1.0e-6
        );
        assert!(
            (apply_center_snap(0.05, neutral, CENTER_SNAP_PX, 120.0, &spec, true) - 0.05).abs()
                < 1.0e-6
        );
        // Outside the dead zone the value is untouched.
        assert!(
            (apply_center_snap(0.5, neutral, CENTER_SNAP_PX, 120.0, &spec, false) - 0.5).abs()
                < 1.0e-6
        );
        // The unipolar slider never snaps.
        let uni = SliderSpec::unipolar();
        assert!(
            (apply_center_snap(0.02, uni.neutral(), uni.snap_px, 120.0, &uni, false) - 0.02).abs()
                < 1.0e-6
        );
    }

    #[test]
    fn the_dead_zone_is_wider_than_the_drag_threshold_on_a_narrow_rail() {
        // The Inspector pan rail is ~120 px. Expressed in value units the dead
        // zone must still be worth more than gpui's 2 px DRAG_THRESHOLD, or the
        // detent can never be hit.
        let spec = SliderSpec::bipolar();
        let half_width = CENTER_SNAP_PX / 120.0 * spec.span();
        let half_width_px = half_width / spec.span() * 120.0;
        assert!(half_width_px > 2.0);
    }

    #[test]
    fn fill_grows_outward_from_centre() {
        // Bipolar: neutral is an empty rail, not a half-full one.
        assert_eq!(fill_span(0.5, 0.5), (0.5, 0.0));
        assert_eq!(fill_span(0.25, 0.5), (0.25, 0.25));
        assert_eq!(fill_span(0.75, 0.5), (0.5, 0.25));
        assert_eq!(fill_span(0.0, 0.5), (0.0, 0.5));
        assert_eq!(fill_span(1.0, 0.5), (0.5, 0.5));
        // Unipolar fill is unchanged: left edge, width == position.
        assert_eq!(fill_span(0.4, 0.0), (0.0, 0.4));
        assert_eq!(fill_span(0.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn position_and_quantisation_match_the_declared_domain() {
        let bi = SliderSpec::bipolar();
        assert!((bi.position_of(-1.0) - 0.0).abs() < 1.0e-6);
        assert!((bi.position_of(0.0) - 0.5).abs() < 1.0e-6);
        assert!((bi.position_of(1.0) - 1.0).abs() < 1.0e-6);
        assert!((bi.quantise(0.12345) - 0.123).abs() < 1.0e-4);

        let uni = SliderSpec::unipolar();
        assert!((uni.position_of(0.25) - 0.25).abs() < 1.0e-6);
        // Plugin parameter resolution is left alone.
        assert!((uni.quantise(0.123456) - 0.123456).abs() < 1.0e-9);
    }

    #[test]
    fn keyboard_steps_land_on_the_step_grid_so_centre_is_reachable() {
        let spec = SliderSpec::bipolar();
        // Already on the grid: a whole step.
        assert!((stepped(0.02, spec.step, &spec) - 0.03).abs() < 1.0e-4);
        // Off the grid: land on it, so a dragged 0.037 walks 0.04, 0.05, ...
        assert!((stepped(0.037, spec.step, &spec) - 0.04).abs() < 1.0e-4);
        assert!((stepped(0.037, -spec.step, &spec) - 0.03).abs() < 1.0e-4);
        // Repeated decrements reach exact centre.
        let mut v = 0.03;
        for _ in 0..3 {
            v = stepped(v, -spec.step, &spec);
        }
        assert!(v.abs() < 1.0e-4);
        // Clamped to the domain.
        assert!((stepped(1.0, spec.step, &spec) - 1.0).abs() < 1.0e-6);
        assert!((stepped(-1.0, -spec.step, &spec) + 1.0).abs() < 1.0e-6);
    }
}
