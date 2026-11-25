//! A track's take lanes, under the track while its takes are open.
//!
//! Takes are counted per region: the bars recorded over more than once (see
//! [`TrackState::take_regions`]). Lane `n` holds every region's take `n + 1`,
//! each drawn where its clip sits, so a track recorded three times over the
//! verse and four times over the chorus shows four lanes — the verse's three
//! takes and the chorus's four side by side — rather than seven.
//!
//! A take's block is how it is chosen. Clicking it makes that take the one
//! heard (muting the takes it overlaps, as [`TimelineState::set_active_take`]
//! does); its × deletes it. The track's own lane above keeps showing what
//! plays.

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

/// One take's block in its lane: its clip's span, its name, and a delete
/// button.
fn take_block(
    track: &TrackState,
    take: &TrackTake,
    state: &TimelineState,
    on_select: &TakeLaneCallback,
    on_delete: &TakeLaneCallback,
) -> Option<gpui::AnyElement> {
    let clip = track.clips.iter().find(|clip| clip.id == take.clip_id)?;
    let active = take.active;
    let select_ids = (track.id.clone(), take.id.clone());
    let delete_ids = select_ids.clone();
    let block_id = format!("take-block-{}-{}", track.id, take.id);
    let (left, width) = state.clip_lane_x_span(clip);
    let height = TAKE_LANE_HEIGHT - 2.0 * space::HAIR - 1.0;
    let on_select = on_select.clone();
    let on_delete = on_delete.clone();
    let tooltip = if take.recorded_at.is_empty() {
        take.name.clone()
    } else {
        format!("{} · {}", take.name, take.recorded_at)
    };
    Some(
        div()
            .id(gpui::ElementId::Name(block_id.clone().into()))
            .group(block_id.clone())
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
            .pl(px(space::TIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .text_size(px(typography::DENSE_CAPTION))
            .text_color(if active {
                Colors::text_primary()
            } else {
                Colors::text_muted()
            })
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.opacity(0.92))
            .tooltip(crate::components::controls::fb_tooltip(tooltip))
            .child(
                // Filled dot for the take that is heard. The state never rests
                // on colour alone: the dot is filled or it is not.
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .flex_none()
                    .rounded(px(radius::PILL))
                    .border(px(1.0))
                    .border_color(if active {
                        Colors::text_primary()
                    } else {
                        Colors::border_strong()
                    })
                    .when(active, |dot| dot.bg(Colors::text_primary())),
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
                    .child(take.name.clone()),
            )
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("{block_id}-delete").into()))
                    .flex_none()
                    .w(px(16.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(Colors::text_faint())
                    .invisible()
                    .group_hover(block_id, |style| style.visible())
                    .hover(|style| {
                        style
                            .bg(Colors::with_alpha(Colors::status_error(), 0.18))
                            .text_color(Colors::status_error())
                    })
                    .tooltip(crate::components::controls::fb_tooltip("Delete take"))
                    .child("×")
                    .on_click(move |_event, window, cx| {
                        // Without this the click also lands on the block and
                        // activates the take it is deleting.
                        cx.stop_propagation();
                        on_delete(&delete_ids, window, cx);
                    }),
            )
            .on_click(move |_event, window, cx| {
                cx.stop_propagation();
                on_select(&select_ids, window, cx);
            })
            .into_any_element(),
    )
}

/// Lane `rank`: every region's take `rank + 1`.
pub fn take_lane(
    track: &TrackState,
    rank: usize,
    takes: &[&TrackTake],
    state: &TimelineState,
    on_select: TakeLaneCallback,
    on_delete: TakeLaneCallback,
) -> impl IntoElement {
    let any_active = takes.iter().any(|take| take.active);
    let row_id = format!("take-lane-{}-{rank}", track.id);

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
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(if any_active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(format!("Take {}", rank + 1)),
        )
        .when(takes.len() > 1, |row| {
            row.child(
                div()
                    .flex_none()
                    .text_color(Colors::text_faint())
                    .child(format!("{} regions", takes.len())),
            )
        });

    let lane = div()
        .flex_1()
        .h_full()
        .relative()
        .overflow_hidden()
        .bg(Colors::timeline_lane_alt_background())
        .children(
            takes
                .iter()
                .filter_map(|take| take_block(track, take, state, &on_select, &on_delete)),
        );

    div()
        .id(gpui::ElementId::Name(row_id.into()))
        .relative()
        .flex()
        .flex_row()
        .w_full()
        .h(px(TAKE_LANE_HEIGHT))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(header)
        .child(lane)
}

impl TimelineState {
    /// Height of `track`'s open take lanes, below its automation.
    pub fn track_take_lanes_height(&self, track: &TrackState) -> f32 {
        if track.takes_expanded {
            track.take_lane_rows().len() as f32 * TAKE_LANE_HEIGHT
        } else {
            0.0
        }
    }
}
