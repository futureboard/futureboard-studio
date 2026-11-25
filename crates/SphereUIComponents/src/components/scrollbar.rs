//! Draggable scrollbars, the one kind every scrolling view uses.
//!
//! The mixer's channel scroller, the piano roll and the timeline are
//! virtualized against a plain `scroll_x` / `scroll_y` held by their owner;
//! lists and panels scrolled by a GPUI `ScrollHandle` use
//! [`scroll_handle_scrollbar`]. GPUI draws no scrollbar of its own, so this
//! draws one for both:
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
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Window,
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
        self.scroll_for_thumb_start(local - self.thumb_len * 0.5)
    }

    /// Scroll offset that puts the thumb's leading edge `start` px along the
    /// track.
    pub fn scroll_for_thumb_start(&self, start: f32) -> f32 {
        let track_range = (self.track_len - self.thumb_len).max(1.0);
        let start = start.clamp(0.0, track_range);
        ((start / track_range) * self.max_scroll).clamp(0.0, self.max_scroll)
    }

    /// Whether a pointer `local` px along the track is on the thumb.
    pub fn thumb_contains(&self, local: f32) -> bool {
        local >= self.thumb_start && local <= self.thumb_start + self.thumb_len
    }

    /// How far into the thumb a pointer `local` px along the track is, kept
    /// inside the thumb.
    pub fn grab_offset(&self, local: f32) -> f32 {
        (local - self.thumb_start).clamp(0.0, self.thumb_len)
    }

    /// Where the thumb starts once a track press at `local` has centered it
    /// there (or as near as the track's ends allow).
    pub fn centered_thumb_start(&self, local: f32) -> f32 {
        (local - self.thumb_len * 0.5).clamp(0.0, (self.track_len - self.thumb_len).max(0.0))
    }
}

/// The last press on a scrollbar: which bar, and where along its thumb the
/// pointer sat. GPUI begins a drag only once the pointer has moved a few px,
/// and the frame's handlers that see the press are not the ones that start the
/// drag, so the press is handed over here. UI thread only.
#[derive(Clone, Copy)]
struct Press {
    key: u64,
    grab: f32,
}

thread_local! {
    static PRESS: Cell<Option<Press>> = const { Cell::new(None) };
}

/// GPUI drag payload for a scrollbar thumb. `on_drag_move` handlers fire for
/// every element listening to the payload type, so the payload names the bar
/// it belongs to and each bar ignores drags that are not its own.
///
/// `grab` is where along the thumb the pointer took hold, set when the drag
/// starts: the thumb keeps that point under the pointer instead of jumping to
/// center on it.
#[derive(Clone, Debug)]
pub struct ScrollbarDrag {
    key: u64,
    grab: Rc<Cell<f32>>,
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
    // A press on the track jumps the thumb there; a press on the thumb leaves
    // it where it is, so grabbing it never moves the view. Either way the
    // press records where along the thumb the pointer now sits.
    let on_track_down = {
        let on_scroll = on_scroll.clone();
        move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
            if let Some(origin) = track_origin.get() {
                let local = along(event.position) - origin;
                let grab = if geometry.thumb_contains(local) {
                    geometry.grab_offset(local)
                } else {
                    on_scroll(geometry.scroll_for_local_pointer(local), window, cx);
                    local - geometry.centered_thumb_start(local)
                };
                PRESS.with(|press| press.set(Some(Press { key, grab })));
            }
            cx.stop_propagation();
        }
    };
    // Drag-move positions are measured against the track's own bounds, which
    // GPUI hands every event, so a drag never depends on the prepaint value.
    let on_thumb_drag =
        move |event: &DragMoveEvent<ScrollbarDrag>, window: &mut Window, cx: &mut App| {
            let drag = event.drag(cx);
            if drag.key != key {
                return;
            }
            let grab = drag.grab.get();
            let local = along(event.event.position) - along(event.bounds.origin);
            on_scroll(geometry.scroll_for_thumb_start(local - grab), window, cx);
        };
    let on_drag_start = move |drag: &ScrollbarDrag,
                              offset: gpui::Point<gpui::Pixels>,
                              _window: &mut Window,
                              cx: &mut App| {
        // GPUI starts the drag a few px after the press, so the grab comes
        // from the press itself. `offset` (the pointer relative to the track,
        // by now past the threshold) is only the fallback.
        let pressed = PRESS.with(|press| press.take()).filter(|p| p.key == key);
        let grab = match pressed {
            Some(press) => press.grab,
            None => geometry.grab_offset(along(offset)),
        };
        drag.grab.set(grab);
        cx.new(|_| drag.clone())
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
            .on_drag(
                ScrollbarDrag {
                    key,
                    grab: Rc::new(Cell::new(geometry.thumb_len * 0.5)),
                },
                on_drag_start,
            )
            .on_drag_move::<ScrollbarDrag>(on_thumb_drag)
            .child(thumb)
            .into_any_element(),
    )
}

/// The vertical scrollbar for a region GPUI scrolls through a [`ScrollHandle`]
/// (an `overflow_y_scroll` div or a `uniform_list`), laid over the region's
/// right edge. Add it as the last child of a `relative` box the scrolling
/// element fills. The thumb drags and the track takes clicks, both moving the
/// handle the wheel moves; `None` when everything fits.
pub fn scroll_handle_scrollbar(id: &'static str, handle: &ScrollHandle) -> Option<AnyElement> {
    let view_h: f32 = handle.bounds().size.height.into();
    let max_y: f32 = handle.max_offset().y.into();
    // GPUI keeps the offset negative: content moves up as you scroll down.
    let scroll = (-f32::from(handle.offset().y)).clamp(0.0, max_y.max(0.0));
    let target = handle.clone();
    let on_scroll: OnScroll = Arc::new(move |y, window, _cx| {
        let x = target.offset().x;
        target.set_offset(gpui::point(x, px(-y)));
        window.refresh();
    });
    let bar = scrollbar(
        id,
        Axis::Vertical,
        scroll,
        view_h + max_y,
        view_h,
        max_y,
        view_h,
        on_scroll,
    )?;
    Some(
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(THICKNESS))
            .child(bar)
            .into_any_element(),
    )
}

#[cfg(test)]
mod pointer_tests {
    //! Real pointer events through GPUI's dispatch, against the two kinds of
    //! region the app's lists use.
    use super::*;
    use gpui::{
        uniform_list, Context, Modifiers, MouseMoveEvent, MouseUpEvent, Pixels, Point,
        TestAppContext, UniformListScrollHandle, VisualTestContext, WindowHandle,
    };

    const PANE_W: f32 = 200.0;
    const PANE_H: f32 = 300.0;

    /// A `uniform_list` (plug-in picker, plug-in manager, browser tree) and an
    /// `overflow_y_scroll` div (picker and manager side rails), side by side,
    /// each with its bar. The list's rows are clickable and draggable, as the
    /// picker's are, and run under the bar.
    struct Rig {
        list: UniformListScrollHandle,
        plain: ScrollHandle,
        /// Row clicks and row drags started.
        row_events: Rc<Cell<usize>>,
    }

    #[derive(Clone)]
    struct RowDrag;

    impl Render for RowDrag {
        fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            Empty
        }
    }

    impl Render for Rig {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let list_handle = self.list.0.borrow().base_handle.clone();
            let row_events = self.row_events.clone();
            div()
                .flex()
                .flex_row()
                .size_full()
                .child(
                    div()
                        .relative()
                        .w(px(PANE_W))
                        .h(px(PANE_H))
                        .child(
                            uniform_list("rig-list", 200, move |range, _window, _cx| {
                                range
                                    .map(|i| {
                                        let clicked = row_events.clone();
                                        let dragged = row_events.clone();
                                        div()
                                            .id(("rig-row", i))
                                            .w_full()
                                            .h(px(20.0))
                                            .on_click(move |_, _, _| clicked.set(clicked.get() + 1))
                                            .on_drag(RowDrag, move |drag, _, _, cx| {
                                                dragged.set(dragged.get() + 1);
                                                cx.new(|_| drag.clone())
                                            })
                                            .child(format!("row {i}"))
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .track_scroll(&self.list)
                            .size_full(),
                        )
                        .children(scroll_handle_scrollbar("rig-list-bar", &list_handle)),
                )
                .child(
                    div()
                        .relative()
                        .w(px(PANE_W))
                        .h(px(PANE_H))
                        .child(
                            div()
                                .id("rig-plain")
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.plain)
                                .child(div().h(px(3_000.0))),
                        )
                        .children(scroll_handle_scrollbar("rig-plain-bar", &self.plain)),
                )
        }
    }

    fn open(cx: &mut TestAppContext) -> (WindowHandle<Rig>, VisualTestContext) {
        let window = cx.open_window(gpui::size(px(PANE_W * 2.0), px(PANE_H)), |_, _| Rig {
            list: UniformListScrollHandle::new(),
            plain: ScrollHandle::new(),
            row_events: Rc::new(Cell::new(0)),
        });
        cx.run_until_parked();
        // The bar sizes itself from the handle's last layout: draw once more.
        window.update(cx, |_, _, cx| cx.notify()).expect("window");
        cx.run_until_parked();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (window, vcx)
    }

    fn scrolled(window: WindowHandle<Rig>, vcx: &mut VisualTestContext, list: bool) -> f32 {
        window
            .update(vcx, |rig, _, _| {
                let offset = if list {
                    rig.list.0.borrow().base_handle.offset()
                } else {
                    rig.plain.offset()
                };
                -f32::from(offset.y)
            })
            .expect("window")
    }

    fn at(x: f32, y: f32) -> Point<Pixels> {
        gpui::point(px(x), px(y))
    }

    fn drag(vcx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        vcx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        vcx.run_until_parked();
        // Several moves, as a real pointer sends: the first one past GPUI's
        // drag threshold starts the drag, the rest move it.
        let steps = 8;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let position = from + (to - from) * t;
            vcx.simulate_event(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            });
            vcx.run_until_parked();
        }
        vcx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: to,
            modifiers: Modifiers::default(),
            click_count: 1,
        });
        vcx.run_until_parked();
    }

    /// The middle of a bar's track, `x` px from the window's left edge.
    fn bar_x(pane: usize) -> f32 {
        PANE_W * (pane as f32 + 1.0) - THICKNESS * 0.5
    }

    #[gpui::test]
    fn dragging_a_lists_thumb_scrolls_the_list(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        assert_eq!(scrolled(window, &mut vcx, true), 0.0);
        let x = bar_x(0);
        drag(&mut vcx, at(x, 10.0), at(x, 110.0));
        let after = scrolled(window, &mut vcx, true);
        assert!(after > 100.0, "the list scrolled {after} px");
        let row_events = window
            .update(&mut vcx, |rig, _, _| rig.row_events.get())
            .expect("window");
        assert_eq!(row_events, 0, "the row under the bar took the press");
    }

    #[gpui::test]
    fn the_rows_still_take_clicks_and_drags_beside_the_bar(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        vcx.simulate_click(at(40.0, 30.0), Modifiers::default());
        vcx.run_until_parked();
        drag(&mut vcx, at(40.0, 30.0), at(120.0, 30.0));
        let row_events = window
            .update(&mut vcx, |rig, _, _| rig.row_events.get())
            .expect("window");
        assert_eq!(row_events, 2, "one row click and one row drag");
        assert_eq!(scrolled(window, &mut vcx, true), 0.0);
    }

    #[gpui::test]
    fn dragging_a_panels_thumb_scrolls_the_panel(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        let x = bar_x(1);
        drag(&mut vcx, at(x, 10.0), at(x, 110.0));
        let after = scrolled(window, &mut vcx, false);
        assert!(after > 100.0, "the panel scrolled {after} px");
    }

    #[gpui::test]
    fn the_thumb_follows_the_pointer_without_jumping(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        let x = bar_x(1);
        // 3000 px of content in a 300 px pane: the thumb is 30 px, so 1 px of
        // thumb travel is (3000 - 300) / (300 - 30) = 10 px of content.
        drag(&mut vcx, at(x, 5.0), at(x, 55.0));
        let after = scrolled(window, &mut vcx, false);
        assert!(
            (after - 500.0).abs() < 15.0,
            "scrolled {after} px, wanted ~500"
        );
    }

    #[gpui::test]
    fn clicking_the_track_jumps_there(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        let x = bar_x(1);
        vcx.simulate_click(at(x, PANE_H - 2.0), Modifiers::default());
        vcx.run_until_parked();
        let after = scrolled(window, &mut vcx, false);
        assert!(after > 2_000.0, "a click at the bottom scrolled {after} px");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressing_the_thumb_keeps_the_point_grabbed_under_the_pointer() {
        let (content, view, max) = big();
        let g = ScrollbarGeometry::new(max * 0.5, content, view, max, view).unwrap();
        // Take hold near the thumb's top, not its middle.
        let press = g.thumb_start + 3.0;
        assert!(g.thumb_contains(press));
        let grab = g.grab_offset(press);
        // Without moving, the drag resolves to the offset already shown.
        let still = g.scroll_for_thumb_start(press - grab);
        assert!((still - max * 0.5).abs() < 1.0, "jumped to {still}");
        // Moving 10 px moves the thumb 10 px.
        let moved = g.scroll_for_thumb_start(press + 10.0 - grab);
        let moved_g = ScrollbarGeometry::new(moved, content, view, max, view).unwrap();
        assert!((moved_g.thumb_start - (g.thumb_start + 10.0)).abs() < 0.5);
    }

    #[test]
    fn a_press_off_the_thumb_is_on_the_track() {
        let (content, view, max) = big();
        let g = ScrollbarGeometry::new(0.0, content, view, max, view).unwrap();
        assert!(!g.thumb_contains(g.thumb_len + 1.0));
        assert!(g.thumb_contains(0.0));
    }

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
