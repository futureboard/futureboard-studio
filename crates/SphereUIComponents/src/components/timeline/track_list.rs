use gpui::prelude::FluentBuilder;
use gpui::{div, px, InteractiveElement, IntoElement, ParentElement, Styled};

use crate::components::edit::{lane_press_intent, LanePressIntent};
use crate::components::timeline::audio_clip::{
    AudioClipProcessCommitCb, AudioClipProcessPreviewCb,
};
use crate::components::timeline::automation_control_lane::{
    automation_control_lane, AutomationControlCallback,
};
use crate::components::timeline::automation_lane::{
    automation_lane, AutomationDeleteCallback, AutomationDownCallback, AutomationHoverCallback,
    AutomationLaneActionCallback,
};
use crate::components::timeline::take_lane::take_lane;
use crate::components::timeline::timeline_state::{
    AutomationHover, AutomationMarquee, TimelineGestureContext, TimelineState, TrackRowLayout,
    AUTOMATION_CONTROL_LANE_HEIGHT, AUTOMATION_SUBLANE_HEIGHT, DEFAULT_TRACK_HEIGHT, HEADER_WIDTH,
};
use crate::components::timeline::track_header::{
    track_header, FolderPlacement, TrackHeaderCallbacks,
};
use crate::components::timeline::track_lane::{track_lane, MarqueePress, MarqueePressCb};
use crate::components::timeline::track_lane_view::{TrackLaneView, TrackLaneViews};
use crate::components::timeline::track_resize::{
    track_row_resize_handle, visible_track_row_range, TrackHeightResizeArmCb,
    TrackHeightResizeResetCb,
};
use crate::components::timeline::vu_meter::TrackMeterViews;
use crate::theme::Colors;

/// Rows above/below the visible viewport that are kept rendered to prevent
/// pop-in during fast scrolling. Measured in track rows. The inline track
/// rename reads it too, to know whether its header is drawn this frame.
pub(crate) const OVERSCAN: usize = 2;

/// `FUTUREBOARD_TIMELINE_BG_DEBUG=1` — trace the timeline background metrics.
/// Cached: `track_list` runs on every timeline repaint, so re-reading the OS
/// env store here would cost a syscall per frame.
fn timeline_bg_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_TIMELINE_BG_DEBUG").is_some())
}

/// Arrangement rows for the current viewport.
///
/// `row_layout` is this frame's arrangement geometry, built once by
/// `Timeline::render` and shared with the arrangement surface — rebuilding it
/// here would clone every track id a second time per frame.
pub fn track_list(
    state: &TimelineState,
    row_layout: &TrackRowLayout,
    meters: &TrackMeterViews,
    header_callbacks: TrackHeaderCallbacks,
    on_resize_arm: TrackHeightResizeArmCb,
    on_resize_reset: TrackHeightResizeResetCb,
    on_select_track: std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    on_select_clip: std::sync::Arc<
        dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_add_clip: std::sync::Arc<
        dyn Fn(&(String, f32, u32, bool), &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_track_context_menu: Option<
        std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_clip_context_menu: Option<
        std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_open_editor: Option<std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>>,
    on_range_start: Option<MarqueePressCb>,
    on_erase_start: Option<
        std::sync::Arc<dyn Fn(&f32, &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_erase_clip: Option<
        std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    >,
    on_cut_clip: Option<crate::components::timeline::audio_clip::AudioClipCutCb>,
    erase_preview_ids: Option<&std::collections::HashSet<String>>,
    on_audio_clip_process_preview: AudioClipProcessPreviewCb,
    on_audio_clip_process_commit: AudioClipProcessCommitCb,
    on_crossfade: Option<crate::components::timeline::crossfade_overlay::CrossfadeGestureCb>,
    on_automation_down: Option<AutomationDownCallback>,
    on_automation_lane_action: Option<AutomationLaneActionCallback>,
    on_automation_hover: Option<AutomationHoverCallback>,
    on_automation_delete: Option<AutomationDeleteCallback>,
    on_automation_control: Option<AutomationControlCallback>,
    automation_marquee: Option<&AutomationMarquee>,
    automation_hover: Option<&AutomationHover>,
    // The arrangement grid layer, built by `Timeline` so it can be a cached
    // view. Passed in rather than built here because a free function has no
    // entity to cache against.
    arrangement_surface: gpui::AnyElement,
    // This frame's gesture snapshot, built once by `Timeline::render` and
    // shared with the cached lane views so it is not derived twice.
    gesture: &std::rc::Rc<TimelineGestureContext>,
    // One cached view per track lane, for the same reason as the grid. Empty on
    // the very first render, which falls back to the inline build below.
    lane_views: &TrackLaneViews,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("TrackList");
    // One per-frame coordinate/snap snapshot shared by every lane and automation
    // sub-lane gesture closure. Previously each of those cloned the entire
    // `TimelineState` (all tracks, clips, and MIDI notes) to satisfy `'static`.
    let grid_height = state.viewport.viewport_height.max(DEFAULT_TRACK_HEIGHT);
    let total_tracks_height = row_layout.total_height;
    let tail_start_y = (total_tracks_height - state.viewport.scroll_y).max(0.0);

    if timeline_bg_debug_enabled() {
        eprintln!(
            "[timeline bg] tracks={} total_h={:.1} scroll_y={:.1} viewport_h={:.1} tail_start_y={:.1}",
            row_layout.rows.len(),
            total_tracks_height,
            state.viewport.scroll_y,
            grid_height,
            tail_start_y
        );
    }
    let move_hint = track_move_hint(state, row_layout, grid_height);
    // The track a drag is carrying, and (for a folder) everything inside it:
    // dimmed where they are, so the move reads as from here to there.
    let dragged_index = state
        .dragging_track_id
        .as_deref()
        .and_then(|id| state.tracks.iter().position(|t| t.id == id));

    let scroll_y = state.viewport.scroll_y;
    let viewport_height = state.viewport.viewport_height;
    let active_tool = state.active_tool;
    let tail_marquee = on_range_start.clone();
    let tail_anchor_track_id = row_layout
        .rows
        .iter()
        .rev()
        .find(|row| row.height > 0.0)
        .map(|row| row.track_id.clone());
    let (visible_start, visible_end, top_spacer_h, bottom_spacer_h) =
        visible_track_row_range(row_layout, scroll_y, viewport_height, OVERSCAN);

    crate::perf::count(
        "visible_track_rows",
        visible_end.saturating_sub(visible_start) as u64,
    );

    let mut rows: Vec<gpui::AnyElement> =
        Vec::with_capacity(visible_end.saturating_sub(visible_start) + 2);

    if top_spacer_h > 0.0 {
        rows.push(
            div()
                .w_full()
                .h(px(top_spacer_h))
                .flex_none()
                .into_any_element(),
        );
    }

    // Read once for every row this frame draws.
    let group_tree = state.group_tree();
    for (offset, track) in state.tracks[visible_start..visible_end].iter().enumerate() {
        // `row_layout.rows` is 1:1 with `state.tracks`, so the row is an index
        // lookup rather than an id scan. The scan was O(track_count) per visible
        // row, which made per-frame cost grow with total track count even though
        // the row window is already virtualized.
        let index = visible_start + offset;
        let Some(row_entry) = row_layout.row_for_index(index) else {
            continue;
        };
        // Mixer-only channels (Bus/Return + VSTi multi-out children) and the
        // children of a collapsed group never render as arrangement rows (no
        // header, no lane, no resize handle). The layout already collapsed them
        // to zero height, so spacers/indices stay aligned.
        if row_entry.height <= 0.0 {
            continue;
        }
        let row_height = row_entry.height;
        let row_y = row_entry.y;
        let automation_height = row_entry.automation_height;
        let total_row_height = row_height + automation_height + row_entry.take_height;

        // Build the expandable automation sub-lane rows that stack directly
        // below the parent track. Each one owns its full row bounds so point
        // hit-testing maps into the correct lane, and is highlighted when it is
        // the active (focused) lane.
        let active_target = state.active_automation_target(&track.id);
        let mut sub_lanes: Vec<gpui::AnyElement> = Vec::new();
        if state.track_automation_expanded(track) {
            sub_lanes.push(
                automation_control_lane(
                    &track.id,
                    track.color,
                    AUTOMATION_CONTROL_LANE_HEIGHT,
                    state,
                    on_automation_control.clone(),
                )
                .into_any_element(),
            );
            let mut lane_y = row_y + row_height + AUTOMATION_CONTROL_LANE_HEIGHT;
            for lane in track.automation_lanes.iter().filter(|l| l.visible) {
                let is_active = lane.target == active_target;
                sub_lanes.push(
                    automation_lane(
                        &track.id,
                        lane,
                        track.color,
                        is_active,
                        lane_y,
                        AUTOMATION_SUBLANE_HEIGHT,
                        state,
                        gesture,
                        on_automation_down.clone(),
                        on_automation_lane_action.clone(),
                        on_automation_hover.clone(),
                        on_automation_delete.clone(),
                        automation_marquee,
                        automation_hover,
                    )
                    .into_any_element(),
                );
                lane_y += AUTOMATION_SUBLANE_HEIGHT;
            }
        }

        let carried =
            dragged_index.is_some_and(|from| index == from || group_tree.is_inside(index, from));
        let row = div()
            .relative()
            .w_full()
            .when(carried, |row| row.opacity(0.4))
            .h(px(total_row_height))
            .flex()
            .flex_col()
            .child(
                // Parent track block (header + clip lane). The resize handle
                // sits at its bottom so it grows only the parent row, not the
                // automation lanes below.
                div()
                    .relative()
                    .w_full()
                    .h(px(row_height))
                    .flex_none()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .size_full()
                            .child(track_header(
                                track,
                                index,
                                state,
                                row_height,
                                &FolderPlacement::of(state, &group_tree, index),
                                header_callbacks.clone(),
                                meters,
                            ))
                            .child(match lane_views.get(track.id.as_str()) {
                                // Cached: a playhead or meter frame reuses the
                                // clips instead of rebuilding them.
                                Some(view) => gpui::AnyView::from(view.clone())
                                    .cached(TrackLaneView::cached_style(row_height))
                                    .into_any_element(),
                                None => track_lane(
                                    track,
                                    index,
                                    state,
                                    gesture,
                                    row_height,
                                    on_select_track.clone(),
                                    on_select_clip.clone(),
                                    on_add_clip.clone(),
                                    on_track_context_menu.clone(),
                                    on_clip_context_menu.clone(),
                                    on_open_editor.clone(),
                                    on_range_start.clone(),
                                    on_erase_start.clone(),
                                    on_erase_clip.clone(),
                                    on_cut_clip.clone(),
                                    erase_preview_ids,
                                    on_audio_clip_process_preview.clone(),
                                    on_audio_clip_process_commit.clone(),
                                    on_crossfade.clone(),
                                )
                                .into_any_element(),
                            }),
                    )
                    .child(track_row_resize_handle(
                        row_entry,
                        on_resize_arm.clone(),
                        on_resize_reset.clone(),
                    )),
            )
            .children(sub_lanes)
            .children((row_entry.take_height > 0.0).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .children(track.takes.iter().map(|take| {
                        take_lane(
                            track,
                            take,
                            state,
                            header_callbacks.on_select_take.clone(),
                            header_callbacks.on_delete_take.clone(),
                        )
                    }))
            }));
        rows.push(row.into_any_element());
    }

    if bottom_spacer_h > 0.0 {
        rows.push(
            div()
                .w_full()
                .h(px(bottom_spacer_h))
                .flex_none()
                .into_any_element(),
        );
    }

    div()
        .relative()
        .size_full()
        .bg(Colors::surface_base())
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(HEADER_WIDTH))
                .bg(Colors::surface_panel())
                .border_r(px(1.0))
                .border_color(Colors::border_strong())
                .child(
                    div()
                        .absolute()
                        .right(px(0.0))
                        .top_0()
                        .bottom_0()
                        .w(px(1.0))
                        .bg(Colors::border_strong()),
                ),
        )
        .child(
            div()
                .absolute()
                .left(px(HEADER_WIDTH))
                .right_0()
                .top_0()
                .bottom_0()
                // Nothing painted for the arrangement may reach the track
                // header column. The grid canvas resolves bar and beat
                // positions from the scroll offset, so anything straddling the
                // left edge lands at a negative x — this is the boundary that
                // makes that a clipped pixel instead of a stray wash over the
                // headers.
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(Colors::timeline_content_background()),
                )
                .children((tail_start_y < grid_height).then(|| {
                    let tail = div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(tail_start_y))
                        .bottom_0()
                        .bg(Colors::timeline_empty_body_background());
                    // The space below the last track starts a marquee too,
                    // anchored to the last track drawn. A click there without a
                    // drag still does nothing.
                    match (tail_anchor_track_id.clone(), tail_marquee.clone()) {
                        (Some(anchor), Some(start_marquee)) => tail
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                move |event: &gpui::MouseDownEvent, window, cx| {
                                    let LanePressIntent::Marquee { additive, .. } =
                                        lane_press_intent(
                                            active_tool,
                                            None,
                                            event.click_count,
                                            &event.modifiers,
                                        )
                                    else {
                                        return;
                                    };
                                    start_marquee(
                                        &MarqueePress {
                                            track_id: anchor.clone(),
                                            window_x: event.position.x.into(),
                                            window_y: event.position.y.into(),
                                            additive,
                                            on_lane: false,
                                            create_clip_on_click: false,
                                            bypass_snap: false,
                                        },
                                        window,
                                        cx,
                                    );
                                },
                            )
                            .into_any_element(),
                        _ => tail.into_any_element(),
                    }
                }))
                .child(arrangement_surface),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(-state.viewport.scroll_y))
                .flex()
                .flex_col()
                .w_full()
                .children(rows),
        )
        .children(move_hint)
}

/// Where a dragged track will land, said so it cannot be missed: across the
/// header column and the lanes, with a label in the header column saying what
/// the drop will do.
///
/// * Between tracks — a bold insertion line with a marker, "Move here" (and
///   the folder it would land in), or "No change" where the drop would leave
///   the order as it is.
/// * Onto a folder — the folder's whole row framed, "Into <folder>".
///
/// Motion confirms each change and then rests: the line slides from its old
/// place to its new one (`motion::MICRO_MS`) and its glow flashes once
/// (`motion::SLOW_MS`). Nothing loops, so the timeline is not redrawn every
/// frame while the pointer is still.
fn track_move_hint(
    state: &TimelineState,
    row_layout: &TrackRowLayout,
    grid_height: f32,
) -> Option<gpui::AnyElement> {
    use gpui::{Animation, AnimationExt, ease_out_quint};
    use std::time::Duration;

    let dragging = state.dragging_track_id.as_deref()?;
    let scroll_y = state.viewport.scroll_y;
    let accent = Colors::accent_primary();
    let generation = state.drag_indicator_generation as usize;
    let slide = Duration::from_millis(crate::theme::motion::MICRO_MS);
    let flash = Duration::from_millis(crate::theme::motion::SLOW_MS);
    let label = |text: String, tone: gpui::Rgba| {
        div()
            .px(px(7.0))
            .py(px(2.0))
            .rounded(px(crate::theme::radius::CONTROL_SM))
            .border(px(1.0))
            .border_color(Colors::with_alpha(tone, 0.7))
            .bg(Colors::surface_raised())
            .shadow_lg()
            .text_size(px(crate::theme::typography::DENSE_LABEL))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_primary())
            .whitespace_nowrap()
            .child(text)
    };

    if let Some(folder_id) = state.drag_folder_target_id.as_deref() {
        let row = row_layout.row_for_track(folder_id)?;
        let name = state.tracks.get(row.index).map(|t| t.name.clone())?;
        let top = row.y - scroll_y;
        return Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(top))
                .h(px(row.height))
                .rounded(px(crate::theme::radius::CONTROL_SM))
                .border(px(2.0))
                .border_color(accent)
                .bg(Colors::with_alpha(accent, 0.10))
                .child(
                    div()
                        .absolute()
                        .left(px(26.0))
                        .top(px(-10.0))
                        .child(label(format!("Into {name}"), accent)),
                )
                .with_animation(
                    ("track-move-folder", generation),
                    Animation::new(flash).with_easing(ease_out_quint()),
                    move |this, t| this.bg(Colors::with_alpha(accent, 0.28 - 0.18 * t)),
                )
                .into_any_element(),
        );
    }

    let index = state.drag_target_index?;
    // The top of the row the line sits above, or the end of the last track.
    let line_y = |index: usize| -> f32 {
        row_layout
            .row_for_index(index)
            .map(|row| row.y)
            .unwrap_or(row_layout.total_height)
            - scroll_y
    };
    let limit = grid_height.max(DEFAULT_TRACK_HEIGHT);
    let to = line_y(index).clamp(0.0, limit);
    let from = state
        .drag_indicator_from_index
        .map(|i| line_y(i).clamp(0.0, limit))
        .unwrap_or(to);

    let origin = state.tracks.iter().position(|t| t.id == dragging);
    let block_end = origin.map(|o| state.group_tree().block_end(o));
    let unchanged = match (origin, block_end) {
        (Some(o), Some(end)) => (o..=end).contains(&index),
        _ => false,
    };
    let text = if unchanged {
        "No change".to_string()
    } else {
        // It lands in the folder of the row below the line.
        let tree = state.group_tree();
        match (index < state.tracks.len())
            .then(|| tree.ancestors(index).first().copied())
            .flatten()
            .and_then(|folder| state.tracks.get(folder))
        {
            Some(folder) => format!("Move here \u{00B7} in {}", folder.name),
            None => "Move here".to_string(),
        }
    };
    let tone = if unchanged {
        Colors::text_muted()
    } else {
        accent
    };

    const BAND: f32 = 14.0;
    Some(
        div()
            .absolute()
            .left_0()
            .right_0()
            .top(px(to - BAND / 2.0))
            .h(px(BAND))
            // Glow: flashes on each new place, then settles to a quiet band.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(BAND / 2.0 - 4.0))
                    .h(px(8.0))
                    .rounded(px(crate::theme::radius::PILL))
                    .bg(Colors::with_alpha(tone, 0.14))
                    .with_animation(
                        ("track-move-glow", generation),
                        Animation::new(flash).with_easing(ease_out_quint()),
                        move |this, t| this.bg(Colors::with_alpha(tone, 0.42 - 0.28 * t)),
                    ),
            )
            // The line itself.
            .child(
                div()
                    .absolute()
                    .left(px(12.0))
                    .right_0()
                    .top(px(BAND / 2.0 - 1.5))
                    .h(px(3.0))
                    .rounded(px(crate::theme::radius::PILL))
                    .bg(tone),
            )
            // The marker at the header end, where the drag handle is.
            .child(
                div()
                    .absolute()
                    .left(px(4.0))
                    .top(px(BAND / 2.0 - 5.0))
                    .size(px(10.0))
                    .rounded(px(crate::theme::radius::PILL))
                    .border(px(2.0))
                    .border_color(tone)
                    .bg(Colors::surface_panel()),
            )
            .child(
                div()
                    .absolute()
                    .left(px(26.0))
                    .top(px(if to < 20.0 { BAND } else { -18.0 }))
                    .max_w(px(HEADER_WIDTH - 32.0))
                    .overflow_hidden()
                    .child(label(text, tone)),
            )
            .with_animation(
                ("track-move-slide", generation),
                Animation::new(slide).with_easing(ease_out_quint()),
                move |this, t| this.top(px(from + (to - from) * t - BAND / 2.0)),
            )
            .into_any_element(),
    )
}
