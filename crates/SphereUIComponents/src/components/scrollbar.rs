//! Draggable scrollbars for views that own their scroll offset.
//!
//! The mixer's channel scroller and the piano roll are virtualized against a
//! plain `scroll_x` / `scroll_y` held by their owner rather than a GPUI
//! `ScrollHandle`, so they get no scrollbar for free. This draws one for them:
//!
//! - a visible track and a thumb that is easy to see and to grab — the
//!   earlier 4 px, 20 %-alpha pill with no track read as decoration and was
//!   lost entirely over coloured mixer name plates;
//! - rest / hover / pressed states on the thumb;
//! - clicking the track jumps there, dragging the thumb scrolls.
//!
//! Geometry, drawing and hit-testing share [`ScrollbarGeometry`], so a click
//! never lands the thumb somewhere a drag would not. The track's window origin
//! is measured in prepaint, so pointer positions map to offsets without
//! estimating where the view sits.

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    canvas, div, px, AnyElement, App, AppContext, DragMoveEvent, Empty, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, StatefulInteractiveElement,
    Styled, Window,
};

use crate::theme::{radius, Colors};

/// Bar thickness: the track, and the hit area the pointer has to find.
pub const THICKNESS: f32 = 12.0;
/// Inset between the track edge and the thumb (thumb = 8 px thick).
pub const THUMB_INSET: f32 = 2.0;
/// The thumb never shrinks below this, however long the content.
pub const MIN_THUMB: f32 = 28.0;

/// Fill alphas over `text.primary`. The track is a faint lane so the bar's
/// extent is visible even where the thumb is short; the thumb is bright enough
/// to read over any panel or name-plate colour.
const TRACK_ALPHA: f32 = 0.07;
const THUMB_REST_ALPHA: f32 = 0.38;
const THUMB_HOVER_ALPHA: f32 = 0.55;
const THUMB_ACTIVE_ALPHA: f32 = 0.70;

/// Callback with the new scroll offset, already clamped to `0..=max_scroll`.
pub type OnScroll = Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// The one transform behind a scrollbar: thumb size and position, and pointer
/// position back to a scroll offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarGeometry {
    pub track_len: f32,
    pub thumb_len: f32,
    pub thumb_start: f32,
    max_scroll: f32,
}

impl ScrollbarGeometry {
    /// `None` when the content already fits — nothing to scroll, no bar.
    pub fn new(
        scroll: f32,
        content_len: f32,
        view_len: f32,
        max_scroll: f32,
        track_len: f32,
    ) -> Option<Self> {
        if max_scroll <= 0.5 || view_len <= 0.0 || content_len <= 0.0 || track_len <= 0.0 {
            return None;
        }
        let thumb_len = ((view_len / content_len) * track_len)
            .max(MIN_THUMB)
            .min(track_len);
        let progress = (scroll / max_scroll).clamp(0.0, 1.0);
        Some(Self {
            track_len,
            thumb_len,
            thumb_start: progress * (track_len - thumb_len).max(0.0),
            max_scroll,
        })
    }

    /// Scroll offset that centers the thumb on a pointer `local` px along the
    /// track.
    pub fn scroll_for_local_pointer(&self, local: f32) -> f32 {
        let track_range = (self.track_len - self.thumb_len).max(1.0);
        let local = (local - self.thumb_len * 0.5).clamp(0.0, track_range);
        ((local / track_range) * self.max_scroll).clamp(0.0, self.max_scroll)
    }
}

/// GPUI drag payload for a scrollbar thumb. `on_drag_move` handlers fire for
/// every element listening to the payload type, so the payload names the bar
/// it belongs to and each bar ignores drags that are not its own.
#[derive(Clone, Copy, Debug)]
pub struct ScrollbarDrag {
    key: u64,
}

impl Render for ScrollbarDrag {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn key_for(id: &str, axis: Axis) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    axis.hash(&mut h);
    h.finish()
}

/// A scrollbar along `axis`, filling the cross-axis `THICKNESS` and the full
/// main-axis length of its parent (the caller positions it). `track_len` is
/// that length in px; `scroll`, `content_len`, `view_len` and `max_scroll` are
/// the owner's scroll state along the axis. `None` when there is nothing to
/// scroll.
pub fn scrollbar(
    id: &'static str,
    axis: Axis,
    scroll: f32,
    content_len: f32,
    view_len: f32,
    max_scroll: f32,
    track_len: f32,
    on_scroll: OnScroll,
) -> Option<AnyElement> {
    let geometry = ScrollbarGeometry::new(scroll, content_len, view_len, max_scroll, track_len)?;
    let key = key_for(id, axis);

    // Written during prepaint, read by this frame's pointer handlers.
    let track_origin: Rc<Cell<Option<f32>>> = Rc::new(Cell::new(None));
    let measure = {
        let track_origin = track_origin.clone();
        canvas(
            move |bounds: gpui::Bounds<gpui::Pixels>, _w, _cx| {
                let origin = match axis {
                    Axis::Horizontal => bounds.origin.x,
                    Axis::Vertical => bounds.origin.y,
                };
                track_origin.set(Some(origin.into()));
            },
            |_b, _r, _w, _cx| {},
        )
        .absolute()
        .inset_0()
    };

    let along = move |p: gpui::Point<gpui::Pixels>| -> f32 {
        match axis {
            Axis::Horizontal => p.x.into(),
            Axis::Vertical => p.y.into(),
        }
    };
    let scroll_for_pointer = {
        let track_origin = track_origin.clone();
        move |pointer: f32| -> Option<f32> {
            Some(geometry.scroll_for_local_pointer(pointer - track_origin.get()?))
        }
    };

    let on_track_down = {
        let on_scroll = on_scroll.clone();
        let scroll_for_pointer = scroll_for_pointer.clone();
        move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
            if let Some(offset) = scroll_for_pointer(along(event.position)) {
                on_scroll(offset, window, cx);
            }
            cx.stop_propagation();
        }
    };
    let on_thumb_drag =
        move |event: &DragMoveEvent<ScrollbarDrag>, window: &mut Window, cx: &mut App| {
            if event.drag(cx).key != key {
                return;
            }
            if let Some(offset) = scroll_for_pointer(along(event.event.position)) {
                on_scroll(offset, window, cx);
            }
        };

    let thumb_rest = Colors::with_alpha(Colors::text_primary(), THUMB_REST_ALPHA);
    let thumb_hover = Colors::with_alpha(Colors::text_primary(), THUMB_HOVER_ALPHA);
    let thumb_active = Colors::with_alpha(Colors::text_primary(), THUMB_ACTIVE_ALPHA);

    let thumb = div()
        .id(gpui::ElementId::Name(format!("{id}-thumb").into()))
        .absolute()
        .rounded(px(radius::PILL))
        .bg(thumb_rest)
        .hover(move |s| s.bg(thumb_hover))
        .active(move |s| s.bg(thumb_active));
    let thumb = match axis {
        Axis::Horizontal => thumb
            .left(px(geometry.thumb_start))
            .w(px(geometry.thumb_len))
            .top(px(THUMB_INSET))
            .bottom(px(THUMB_INSET)),
        Axis::Vertical => thumb
            .top(px(geometry.thumb_start))
            .h(px(geometry.thumb_len))
            .left(px(THUMB_INSET))
            .right(px(THUMB_INSET)),
    };

    let track = div()
        .id(id)
        .relative()
        .bg(Colors::with_alpha(Colors::text_primary(), TRACK_ALPHA));
    let track = match axis {
        Axis::Horizontal => track.w_full().h(px(THICKNESS)),
        Axis::Vertical => track.h_full().w(px(THICKNESS)),
    };
    Some(
        track
            .child(measure)
            .on_mouse_down(MouseButton::Left, on_track_down)
            .on_drag(ScrollbarDrag { key }, |drag, _offset, _window, cx| {
                cx.new(|_| *drag)
            })
            .on_drag_move::<ScrollbarDrag>(on_thumb_drag)
            .child(thumb)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2000-channel session: 88 px strips in an ~880 px bay.
    fn big() -> (f32, f32, f32) {
        let content = 2_000.0 * 88.0;
        let view = 880.0;
        (content, view, content - view)
    }

    #[test]
    fn no_bar_when_the_content_already_fits() {
        assert!(ScrollbarGeometry::new(0.0, 400.0, 880.0, 0.0, 880.0).is_none());
    }

    #[test]
    fn thumb_stays_grabbable_and_inside_the_track() {
        let (content, view, max) = big();
        for scroll in [0.0, max * 0.5, max] {
            let g = ScrollbarGeometry::new(scroll, content, view, max, view).unwrap();
            assert!(g.thumb_len >= MIN_THUMB, "thumb {}", g.thumb_len);
            assert!(g.thumb_start >= 0.0);
            assert!(g.thumb_start + g.thumb_len <= g.track_len + 0.01);
        }
    }

    #[test]
    fn thumb_position_tracks_the_offset() {
        let (content, view, max) = big();
        let start = ScrollbarGeometry::new(0.0, content, view, max, view).unwrap();
        let end = ScrollbarGeometry::new(max, content, view, max, view).unwrap();
        assert_eq!(start.thumb_start, 0.0);
        assert!((end.thumb_start + end.thumb_len - end.track_len).abs() < 0.01);
    }

    #[test]
    fn pointer_maps_back_to_the_offset_that_drew_the_thumb() {
        let (content, view, max) = big();
        for scroll in [0.0, max * 0.25, max * 0.5, max] {
            let g = ScrollbarGeometry::new(scroll, content, view, max, view).unwrap();
            let resolved = g.scroll_for_local_pointer(g.thumb_start + g.thumb_len * 0.5);
            assert!(
                (resolved - scroll).abs() < 1.0,
                "drew {scroll}, resolved {resolved}"
            );
        }
    }

    #[test]
    fn each_bar_has_its_own_drag_key() {
        assert_ne!(
            key_for("piano-roll", Axis::Horizontal),
            key_for("piano-roll", Axis::Vertical)
        );
        assert_ne!(
            key_for("mixer", Axis::Horizontal),
            key_for("piano-roll", Axis::Horizontal)
        );
    }
}
