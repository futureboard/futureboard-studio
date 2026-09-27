use std::sync::Arc;

use gpui::{
    div, px, App, AppContext, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::timeline::timeline_state::{
    TrackHeightResizeDrag, TrackRowLayoutEntry, HEADER_WIDTH, TRACK_RESIZE_HANDLE_HITBOX,
};
use crate::theme::Colors;

pub type TrackHeightResizeArmCb =
    Arc<dyn Fn(&(String, f32, bool, bool), &mut Window, &mut App) + 'static>;
pub type TrackHeightResizeResetCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

pub fn track_row_resize_handle(
    row: &TrackRowLayoutEntry,
    on_resize_arm: TrackHeightResizeArmCb,
    on_double_click_reset: TrackHeightResizeResetCb,
) -> impl IntoElement {
    let track_id = row.track_id.clone();
    let drag_payload = TrackHeightResizeDrag {
        anchor_track_id: track_id.clone(),
    };
    let arm_cb = on_resize_arm.clone();
    let reset_cb = on_double_click_reset.clone();

    // Header column only. Across the lane it swallowed the bottom 5 px of
    // every row, where a marquee should be able to start; the lane draws its
    // own bottom border.
    div()
        .absolute()
        .left_0()
        .w(px(HEADER_WIDTH))
        .bottom(px(0.0))
        .h(px(TRACK_RESIZE_HANDLE_HITBOX))
        .id(("track-row-resize", row.index))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .on_mouse_down(gpui::MouseButton::Left, {
            let track_id = track_id.clone();
            let arm_cb = arm_cb.clone();
            let reset_cb = reset_cb.clone();
            move |event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    reset_cb(&track_id, window, cx);
                    return;
                }
                let y: f32 = event.position.y.into();
                arm_cb(
                    &(
                        track_id.clone(),
                        y,
                        event.modifiers.shift,
                        event.modifiers.alt,
                    ),
                    window,
                    cx,
                );
            }
        })
        .on_drag(drag_payload, move |_drag, _offset, _window, cx| {
            cx.stop_propagation();
            cx.new(|_| TrackHeightResizeDrag {
                anchor_track_id: track_id.clone(),
            })
        })
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(0.0))
                .h(px(1.0))
                .bg(Colors::border_subtle()),
        )
}

pub fn visible_track_row_range(
    row_layout: &crate::components::timeline::timeline_state::TrackRowLayout,
    scroll_y: f32,
    viewport_height: f32,
    overscan: usize,
) -> (usize, usize, f32, f32) {
    let track_count = row_layout.rows.len();
    if track_count == 0 {
        return (0, 0, 0.0, 0.0);
    }
    // Row tops and bottoms are both non-decreasing (each row starts where the
    // previous one ends), so both edges binary-search instead of scanning. The
    // scan was O(track_count) per consumer per frame and grew with the project;
    // this is O(log track_count).
    let visible_start = row_layout
        .rows
        .partition_point(|row| row.y + row.block_height() <= scroll_y)
        .saturating_sub(overscan);
    let visible_end = row_layout
        .rows
        .partition_point(|row| row.y < scroll_y + viewport_height)
        .saturating_add(overscan)
        .min(track_count);
    let top_spacer = row_layout
        .rows
        .get(visible_start)
        .map(|row| row.y)
        .unwrap_or(0.0);
    let bottom_spacer = if visible_end < track_count {
        let last = &row_layout.rows[visible_end - 1];
        (row_layout.total_height - last.y - last.block_height()).max(0.0)
    } else {
        0.0
    };
    (visible_start, visible_end, top_spacer, bottom_spacer)
}
