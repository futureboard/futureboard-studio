//! A track's take lanes: one row per take under the track while its takes are
//! open, each showing the clip that pass made, where it made it.
//!
//! The row is how a take is chosen. Clicking it makes that take the one heard
//! (muting the takes it overlaps, as [`TimelineState::set_active_take`] does),
//! so the passes of a loop recording sit stacked like the lanes they were
//! played in, and picking one is a click on the one that sounded right.
//! The track's own lane above keeps showing what plays.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
};

use crate::components::timeline::timeline_state::{
    TimelineState, TrackState, TrackTake, HEADER_WIDTH,
};
use crate::theme::{radius, space, typography, Colors};

/// Height of one take lane.
pub const TAKE_LANE_HEIGHT: f32 = 26.0;

/// `(track_id, take_id)`.
pub type TakeLaneCallback =
    std::sync::Arc<dyn Fn(&(String, String), &mut gpui::Window, &mut gpui::App) + 'static>;

/// One take's row: its name and state on the left, its clip on the right.
pub fn take_lane(
    track: &TrackState,
    take: &TrackTake,
    state: &TimelineState,
    on_select: TakeLaneCallback,
    on_delete: TakeLaneCallback,
) -> impl IntoElement {
    let active = take.active;
    let select_ids = (track.id.clone(), take.id.clone());
    let delete_ids = select_ids.clone();
    let row_id = format!("take-lane-{}-{}", track.id, take.id);

    let header = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .flex_none()
        .gap(px(space::SNUG))
        .w(px(HEADER_WIDTH))
        .h_full()
        .pl(px(space::BLOCK))
        .pr(px(space::BASE))
        .bg(Colors::surface_panel())
        .border_r(px(1.0))
        .border_color(Colors::border_strong())
        .text_size(px(typography::DENSE_CAPTION))
        // The track's colour carries down its take rows.
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(3.0))
                .bg(Colors::with_alpha(track.color, 0.55)),
        )
        .child(
            // Filled dot for the take that is heard. The state never rests
            // on colour alone: the dot is filled or it is not.
            div()
                .w(px(7.0))
                .h(px(7.0))
                .flex_none()
                .rounded(px(radius::PILL))
                .border(px(1.0))
                .border_color(if active {
                    Colors::accent_primary()
                } else {
                    Colors::border_strong()
                })
                .when(active, |dot| dot.bg(Colors::accent_primary())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(take.name.clone()),
        )
        .when(!take.recorded_at.is_empty(), |row| {
            row.child(
                div()
                    .flex_none()
                    .text_color(Colors::text_faint())
                    .child(take.recorded_at.clone()),
            )
        })
        .child(
            div()
                .id(gpui::ElementId::Name(format!("{row_id}-delete").into()))
                .flex_none()
                .w(px(16.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::CONTROL_SM))
                .text_color(Colors::text_faint())
                .hover(|style| {
                    style
                        .bg(Colors::with_alpha(Colors::status_error(), 0.18))
                        .text_color(Colors::status_error())
                })
                .tooltip(crate::components::controls::fb_tooltip("Delete take"))
                .child("×")
                .on_click(move |_event, window, cx| {
                    // Without this the click also lands on the row and
                    // activates the take it is deleting.
                    cx.stop_propagation();
                    on_delete(&delete_ids, window, cx);
                }),
        );

    let clip_block = track
        .clips
        .iter()
        .find(|clip| clip.id == take.clip_id)
        .map(|clip| {
            let (left, width) = state.clip_lane_x_span(clip);
            let height = TAKE_LANE_HEIGHT - 2.0 * space::HAIR - 1.0;
            div()
                .absolute()
                .left(px(left))
                .top(px(space::HAIR))
                .w(px(width))
                .h(px(height))
                .rounded(px(radius::clamped(radius::MICRO, width, height)))
                .bg(Colors::with_alpha(
                    track.color,
                    if active { 0.72 } else { 0.24 },
                ))
                .border(px(1.0))
                .border_color(Colors::with_alpha(
                    track.color,
                    if active { 1.0 } else { 0.5 },
                ))
                .overflow_hidden()
                .px(px(space::TIGHT))
                .flex()
                .items_center()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_muted()
                })
                .child(div().truncate().child(take.name.clone()))
        });

    let lane = div()
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .bg(if active {
            Colors::with_alpha(track.color, 0.08)
        } else {
            Colors::timeline_lane_alt_background()
        })
        .children(clip_block);

    div()
        .id(gpui::ElementId::Name(row_id.into()))
        .relative()
        .flex()
        .flex_row()
        .w_full()
        .h(px(TAKE_LANE_HEIGHT))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.opacity(0.92))
        .child(header)
        .child(lane)
        .on_click(move |_event, window, cx| on_select(&select_ids, window, cx))
}

impl TimelineState {
    /// Height of `track`'s open take lanes, below its automation.
    pub fn track_take_lanes_height(&self, track: &TrackState) -> f32 {
        if track.takes_expanded {
            track.takes.len() as f32 * TAKE_LANE_HEIGHT
        } else {
            0.0
        }
    }
}
