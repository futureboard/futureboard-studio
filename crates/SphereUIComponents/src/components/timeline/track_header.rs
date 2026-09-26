use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, AppContext, DragMoveEvent, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::controls::fb_tooltip;
use crate::components::fader::{db_value_pill, horizontal_fader_with_drag_callbacks};
use crate::components::knob::format_pan_label;
use crate::components::spin_drag::SpinDrag;
use crate::components::text_input::{
    text_field_with_callbacks_and_ime, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::timeline_state::{
    is_arrangement_hidden_track, volume, TimelineState, TrackDragItem, TrackLaneMode, TrackState,
    TrackType, HEADER_WIDTH, TRACK_HEADER_CONTROLS_MIN_HEIGHT,
};
use crate::components::timeline::vu_meter::{vu_meter_with_levels, TrackMeterViews};
use crate::theme::{radius, size, space, typography, Colors};

type TrackCallback = std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>;
type TrackSelectCallback =
    std::sync::Arc<dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static>;
type VolumeCallback =
    std::sync::Arc<dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static>;
type VolumeCommitCallback =
    std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>;
type TrackContextCallback =
    std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>;
type TrackGroupCallback =
    std::sync::Arc<dyn Fn(&(String, String), &mut gpui::Window, &mut gpui::App) + 'static>;
/// `(track_id, take_id)`.
type TrackTakeCallback =
    std::sync::Arc<dyn Fn(&(String, String), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Bundle of callbacks the TrackHeader can fire. Keeping them in one struct
/// keeps the function signature manageable and lets new actions land without
/// re-threading every call site.
#[derive(Clone)]
pub struct TrackHeaderCallbacks {
    pub on_select_track: TrackSelectCallback,
    pub on_toggle_mute: TrackCallback,
    pub on_toggle_solo: TrackCallback,
    pub on_toggle_arm: TrackCallback,
    pub on_toggle_input: TrackCallback,
    /// Toggle the track between Clip and Automation edit mode.
    pub on_toggle_automation: TrackCallback,
    pub on_volume_change: VolumeCallback,
    pub on_volume_drag_start: VolumeCallback,
    pub on_volume_drag_preview: VolumeCallback,
    pub on_volume_drag_commit: VolumeCommitCallback,
    /// Discrete pan changes such as double-click reset.
    pub on_pan_change: VolumeCallback,
    pub on_pan_drag_start: TrackCallback,
    pub on_pan_drag_preview: VolumeCallback,
    pub on_pan_drag_commit: VolumeCommitCallback,
    pub on_assign_to_group: TrackGroupCallback,
    pub on_toggle_group_collapsed: TrackCallback,
    pub on_context_menu: Option<TrackContextCallback>,
    /// Open/close the track's take sub-lane.
    pub on_toggle_takes: TrackCallback,
    /// Make a take the one that is heard: `(track_id, take_id)`.
    pub on_select_take: TrackTakeCallback,
    /// Delete a take and the clip it holds: `(track_id, take_id)`.
    pub on_delete_take: TrackTakeCallback,
    /// Open the track's instrument plugin editor, or the instrument picker when
    /// the slot is empty. `None` hides the header button.
    pub on_open_instrument: Option<TrackCallback>,
    /// Open the inline name editor on a plain double-click of the name; see
    /// [`opens_track_rename`].
    pub on_begin_rename: TrackCallback,
    /// The rename in progress, drawn in place of its track's name. `None` when
    /// no header is being renamed, so the other rows capture nothing of it.
    pub rename: Option<std::rc::Rc<TrackHeaderRename>>,
}

/// Height of the inline name field: between the 20 px affordances beside the
/// name and the 24 px band of the M/S/R/I/A strip, so opening the editor never
/// changes the height of the name line, on a compact row or a full one.
pub const TRACK_NAME_FIELD_HEIGHT: f32 = 22.0;

/// What the header needs to draw the name editor the arrangement owns.
pub struct TrackHeaderRename {
    pub track_id: String,
    pub input: TextInputState,
    pub focused: bool,
    pub callbacks: TextInputCallbacks,
    pub ime_target: gpui::Entity<crate::components::timeline::Timeline>,
}

/// Whether a click on a track name opens the inline rename.
///
/// Only a plain, stationary double-click does. The name sits inside the drag
/// zone, and GPUI still delivers a child's click after the parent dragged, so
/// the click count alone would open the editor after two quick reorder drags;
/// [`is_reset_double_click`](crate::components::fader::is_reset_double_click)
/// requires the pointer to have stayed put. A modified double-click is the
/// header's Cmd/Shift selection gesture pressed twice, not a request to rename.
pub fn opens_track_rename(event: &gpui::ClickEvent, drag_active: bool) -> bool {
    let modified = match event {
        gpui::ClickEvent::Mouse(click) => {
            click.down.modifiers.modified() || click.up.modifiers.modified()
        }
        gpui::ClickEvent::Keyboard(_) => false,
    };
    !drag_active && !modified && crate::components::fader::is_reset_double_click(event)
}

pub struct TrackDragPreview {
    pub name: String,
    pub color: gpui::Rgba,
}

impl Render for TrackDragPreview {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .h(px(size::COMFORTABLE))
            .min_w(px(150.0))
            .px(px(space::BASE))
            .rounded(px(radius::CONTROL))
            .border(px(1.0))
            .border_color({
                let mut c = self.color;
                c.a = 0.72;
                c
            })
            .bg(Colors::surface_raised())
            .shadow_lg()
            .child(
                div()
                    .w(px(3.0))
                    .h(px(size::MICRO))
                    .rounded(px(radius::PILL))
                    .bg(self.color),
            )
            .child(
                div()
                    .max_w(px(220.0))
                    .overflow_hidden()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.name.clone()),
            )
    }
}

/// Glyph sizes inside the header's controls. Two only: one for a 16 px pill and
/// one for the 20-22 px affordances beside the name. A third size here is what
/// makes a compact row look assembled from spare parts.
const GLYPH_SM: f32 = 10.0;
const GLYPH_MD: f32 = 11.0;

/// Height of the pan readout. Below the 16 px pill tier because it shares a row
/// with a 24 px fader and has to leave the plate its breathing room.
const PAN_PILL_H: f32 = 14.0;

/// Height of the take sub-lane's own header line, and of one take row.
///
/// The sub-lane lives in the room the user makes by dragging the track taller
/// — it never pushes the arrangement around. That is what keeps a take list
/// from silently re-laying-out every lane below it the first time somebody
/// records twice.
const TAKE_STRIP_HEADER_H: f32 = 16.0;
const TAKE_ROW_H: f32 = 18.0;

/// Vertical space the two-row header needs before anything else can be shown.
const TAKE_STRIP_BASE_H: f32 = TRACK_HEADER_CONTROLS_MIN_HEIGHT;

/// How far a grouped child's inset frame sits inside the header column, and how
/// much room its accent strip and indent need inside that frame. Derived from
/// the spacing scale rather than hand-tuned per edge, so the folder frame lines
/// up with everything else in the column.
const GROUP_FRAME_INSET: f32 = space::SNUG;
const GROUP_FRAME_CAP: f32 = space::TIGHT;

/// One latching track-state toggle inside the M/S/R/I/A strip.
///
/// The five toggles are a *touching group*, so the contract's rule for any
/// touching pair applies: the segments share one hairline instead of each
/// carrying its own border, and only the group's outer corners round. Their
/// corners are not rounded here at all — the group clips them, which is what
/// keeps a shared edge from leaving a notch of background between two arcs.
///
/// Latched state is carried on fill *and* glyph — `Colors::latched`'s wash
/// under the semantic hue at full strength. The third channel, the latched
/// border, moves to the strip: the divider next to a latched segment takes that
/// hue, so the boundary reads as belonging to the lit toggle. A per-segment
/// border cannot survive here, because the neighbour's border would overlap it
/// on the shared edge and half of it would disappear.
///
/// `divider` is `None` for the first segment — there is nothing to its left but
/// the group's own frame.
fn latch_segment(
    id: gpui::ElementId,
    label: &'static str,
    icon: Option<&'static str>,
    active: bool,
    semantic: gpui::Rgba,
    divider: Option<gpui::Rgba>,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let base = Colors::surface_panel_alt();
    let rest = if active {
        Colors::latched(base, semantic).0
    } else {
        Colors::button_bg()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let fg = if active {
        semantic
    } else {
        Colors::text_muted()
    };

    let seg = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .w(px(size::DENSE))
        .h(px(size::DENSE))
        .bg(rest)
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(fg)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_mouse_down(gpui::MouseButton::Left, on_click)
        .when_some(divider, |seg, color| {
            seg.border_l(px(1.0)).border_color(color)
        });

    if let Some(path) = icon {
        seg.child(
            svg()
                .path(path)
                .w(px(GLYPH_SM))
                .h(px(GLYPH_SM))
                .text_color(fg),
        )
    } else {
        seg.child(label)
    }
}

/// Colour of the hairline between two segments.
///
/// A divider that touches a latched toggle takes that toggle's latched border
/// hue, so the strip still encodes state on a border channel even though no
/// segment owns one.
fn latch_divider(left: Option<gpui::Rgba>, right: Option<gpui::Rgba>) -> gpui::Rgba {
    let base = Colors::surface_panel_alt();
    match (left, right) {
        (Some(hue), _) | (None, Some(hue)) => Colors::latched(base, hue).1,
        (None, None) => Colors::border_subtle(),
    }
}

/// The five toggle handlers, bundled so the strip's signature stays readable.
struct LatchStripHandlers<M, S, R, I, A> {
    on_mute: M,
    on_solo: S,
    on_arm: R,
    on_input: I,
    on_automation: A,
}

/// The M/S/R/I/A strip: five latching toggles as one touching group.
///
/// The group owns the frame, the corner radius and the hit area. The hit area
/// has to live here rather than on each toggle: `size::hit_target` grows a
/// control with transparent padding, and padding on a segment would push it off
/// its neighbour — the one thing this strip exists to prevent. So the segments
/// are `size::DENSE` (20 px) squares and the strip carries the 2 px above and
/// below that lifts the row to the 24 px comfortable target.
#[allow(clippy::too_many_arguments)]
fn latch_strip<M, S, R, I, A>(
    track: &TrackState,
    is_automation: bool,
    id_num: usize,
    handlers: LatchStripHandlers<M, S, R, I, A>,
) -> impl IntoElement
where
    M: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    S: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    R: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    I: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    A: Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
{
    // Hue when latched, `None` when not — the divider colouring reads this to
    // decide which side of each seam is lit.
    let states: [Option<gpui::Rgba>; 5] = [
        track.muted.then(Colors::state_mute),
        track.solo.then(Colors::state_solo),
        track.armed.then(Colors::state_arm),
        track
            .input_monitor
            .is_active(track.armed)
            .then(Colors::state_monitor),
        is_automation.then(Colors::state_automation),
    ];

    div()
        // Transparent padding, not a taller strip: the visible group stays
        // 20 px so the header reads tight, and the clickable band is 24.
        .py(px(size::hit_target(size::DENSE)))
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                // No gap. This is the whole point of the strip: five toggles a
                // player hits by muscle memory, sharing edges the way a console
                // does, instead of five islands two pixels apart.
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                // Clips the segments' square corners to the group's rounded
                // ones, so the outer corners round and every inner edge stays
                // square without a single per-segment radius.
                .overflow_hidden()
                .child(latch_segment(
                    ("mute-btn", id_num).into(),
                    "M",
                    None,
                    track.muted,
                    Colors::state_mute(),
                    None,
                    handlers.on_mute,
                ))
                .child(latch_segment(
                    ("solo-btn", id_num).into(),
                    "S",
                    None,
                    track.solo,
                    Colors::state_solo(),
                    Some(latch_divider(states[0], states[1])),
                    handlers.on_solo,
                ))
                .child(latch_segment(
                    ("arm-btn", id_num).into(),
                    "R",
                    None,
                    track.armed,
                    Colors::state_arm(),
                    Some(latch_divider(states[1], states[2])),
                    handlers.on_arm,
                ))
                .child(latch_segment(
                    ("input-btn", id_num).into(),
                    "I",
                    None,
                    track.input_monitor.is_active(track.armed),
                    Colors::state_monitor(),
                    Some(latch_divider(states[2], states[3])),
                    handlers.on_input,
                ))
                // Automation mode toggle — switches the lane between Clip and
                // Automation editing.
                .child(latch_segment(
                    ("auto-btn", id_num).into(),
                    "A",
                    None,
                    is_automation,
                    Colors::state_automation(),
                    Some(latch_divider(states[3], states[4])),
                    handlers.on_automation,
                )),
        )
}

/// The track's take sub-lane, or `None` when the track has no takes or the row
/// is too short to hold the strip.
///
/// It is a sub-lane *of the header*: it occupies the vertical room the user made
/// by dragging the track taller, and shows as many take rows as fit. A track
/// with eight takes on a default-height row shows the header line and the count;
/// drag it taller and the rows appear. Nothing here can overflow the row,
/// because the number of rows drawn is derived from the room there is.
fn take_sublane(
    track: &TrackState,
    row_height: f32,
    on_toggle: TrackCallback,
    on_select: TrackTakeCallback,
    on_delete: TrackTakeCallback,
) -> Option<gpui::AnyElement> {
    if track.takes.is_empty() {
        return None;
    }
    let spare = row_height - TAKE_STRIP_BASE_H;
    if spare < TAKE_STRIP_HEADER_H {
        return None;
    }
    let visible_rows = if track.takes_expanded {
        (((spare - TAKE_STRIP_HEADER_H) / TAKE_ROW_H)
            .floor()
            .max(0.0) as usize)
            .min(track.takes.len())
    } else {
        0
    };
    let hidden = track.takes.len().saturating_sub(visible_rows);

    let toggle_id = track.id.clone();
    let caret = if track.takes_expanded { "▾" } else { "▸" };
    let depth = track.take_stack_depth();
    let summary = if depth > 1 {
        format!("{} takes · {depth} deep", track.takes.len())
    } else {
        format!("{} takes", track.takes.len())
    };

    let header_line = div()
        .id(gpui::ElementId::Name(
            format!("track-takes-toggle-{}", track.id).into(),
        ))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .h(px(TAKE_STRIP_HEADER_H))
        .px(px(space::SNUG))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(Colors::surface_control_hover()))
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_muted())
        .child(caret)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(summary),
        )
        .when(hidden > 0 && track.takes_expanded, |line| {
            line.child(
                div()
                    .text_color(Colors::text_faint())
                    .child(format!("+{hidden}")),
            )
        })
        .on_click(move |_event, window, cx| on_toggle(&toggle_id, window, cx));

    // Newest first: the pass the player just did is the one they are looking
    // for, and it is the one that is active.
    let rows: Vec<gpui::AnyElement> = track
        .takes_newest_first()
        .take(visible_rows)
        .map(|take| {
            let select = on_select.clone();
            let delete = on_delete.clone();
            let select_ids = (track.id.clone(), take.id.clone());
            let delete_ids = (track.id.clone(), take.id.clone());
            let active = take.active;
            div()
                .id(gpui::ElementId::Name(
                    format!("track-take-{}-{}", track.id, take.id).into(),
                ))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .h(px(TAKE_ROW_H))
                .px(px(space::SNUG))
                .rounded(px(radius::CONTROL_SM))
                .when(active, |row| row.bg(Colors::accent_muted()))
                .hover(|style| style.bg(Colors::surface_control_hover()))
                .cursor(gpui::CursorStyle::PointingHand)
                .text_size(px(typography::DENSE_CAPTION))
                .child(
                    // Filled dot for the take that is heard. The state never
                    // rests on colour alone — the dot is present or it is not.
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
                        .id(gpui::ElementId::Name(
                            format!("track-take-delete-{}-{}", track.id, take.id).into(),
                        ))
                        .flex_none()
                        .w(px(12.0))
                        .h(px(12.0))
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
                        .child("×")
                        .on_click(move |_event, window, cx| {
                            // Without this the click also lands on the row and
                            // activates the take it is deleting.
                            cx.stop_propagation();
                            delete(&delete_ids, window, cx);
                        }),
                )
                .on_click(move |_event, window, cx| select(&select_ids, window, cx))
                .into_any_element()
        })
        .collect();

    Some(
        div()
            .flex()
            .flex_col()
            .w_full()
            .flex_none()
            .border_t(px(1.0))
            .border_color(Colors::divider())
            .child(header_line)
            .children(rows)
            .into_any_element(),
    )
}

/// Compact instrument affordance on an Instrument track header.
///
/// Click opens the loaded plugin editor, Solfege dock, or Soundfont Player —
/// or the instrument picker when the slot is still empty. Icon-only so the
/// name row stays the track name, not a second plugin label.
fn instrument_header_button(
    track: &TrackState,
    id_num: usize,
    on_open: TrackCallback,
) -> impl IntoElement {
    let loaded = track.solfege.is_some()
        || track.builtin_soundfont_player
        || track
            .instrument_insert()
            .is_some_and(|slot| !slot.is_empty());
    let tooltip = if track.solfege.is_some() {
        "Solfege".to_string()
    } else if track.builtin_soundfont_player {
        "Soundfont Player".to_string()
    } else if let Some(slot) = track.instrument_insert().filter(|slot| !slot.is_empty()) {
        slot.display_name.clone()
    } else {
        "Instrument".to_string()
    };
    let rest = if loaded {
        Colors::accent_muted()
    } else {
        Colors::button_bg()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let fg = if loaded {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    let track_id = track.id.clone();

    div()
        .id(("track-instrument", id_num))
        .flex()
        .items_center()
        .justify_center()
        .w(px(size::DENSE))
        .h(px(size::DENSE))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(if loaded {
            Colors::with_alpha(Colors::accent_primary(), 0.45)
        } else {
            Colors::border_subtle()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .tooltip(fb_tooltip(tooltip))
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on_open(&track_id, window, cx);
        })
        .child(
            svg()
                .path(assets::ICON_KEYBOARD_PATH)
                .w(px(GLYPH_MD))
                .h(px(GLYPH_MD))
                .text_color(fg),
        )
}

pub fn track_header(
    track: &TrackState,
    index: usize,
    state: &TimelineState,
    row_height: f32,
    callbacks: TrackHeaderCallbacks,
    meters: &TrackMeterViews,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("TrackHeader");
    let track_id = track.id.clone();
    // The arrangement shows a marquee's preview while one is in flight.
    let is_selected = state.display_selection().is_track_selected(&track.id);
    let is_automation = track.lane_mode == TrackLaneMode::Automation;
    let is_group = track.track_type == TrackType::Group;
    let is_group_child = track.parent_group_id.is_some();
    let is_first_group_child = track.parent_group_id.as_deref().is_some_and(|group_id| {
        state
            .tracks
            .get(..index)
            .into_iter()
            .flatten()
            .rev()
            .find(|candidate| !is_arrangement_hidden_track(candidate))
            .is_none_or(|previous| previous.parent_group_id.as_deref() != Some(group_id))
    });
    let is_last_group_child = track.parent_group_id.as_deref().is_some_and(|group_id| {
        state
            .tracks
            .iter()
            .skip(index + 1)
            .find(|candidate| !is_arrangement_hidden_track(candidate))
            .is_none_or(|next| next.parent_group_id.as_deref() != Some(group_id))
    });
    // Adaptive header: the volume/pan/meter/dB control row only fits at the
    // default row height or taller. Below that we show the compact single-row
    // header so controls never overlap, clip, or float outside the row.
    let show_controls = row_height >= TRACK_HEADER_CONTROLS_MIN_HEIGHT;
    let is_dragging = state.dragging_track_id.as_deref() == Some(track.id.as_str());
    let is_drop_target =
        state.drag_target_index == Some(index) || state.drag_target_index == Some(index + 1);
    // Selection, drag, and drop-target are *tints*: translucent washes designed
    // to sit on the header's surface, not to be it. Using one as the whole
    // background left a selected header with no opaque pixel of its own, so
    // whatever the arrangement painted behind the header column showed straight
    // through it. The base stays opaque and the tint goes on top.
    let header_tint = if is_dragging {
        Some(Colors::with_alpha(Colors::text_primary(), 0.07))
    } else if is_automation {
        // Quiet graphite tint so the active automation track reads as active
        // without flooding the header with accent hue.
        Some(Colors::surface_selected_soft())
    } else if is_selected {
        Some(Colors::track_selected_overlay())
    } else if is_drop_target && state.dragging_track_id.is_some() {
        Some(Colors::with_alpha(Colors::text_primary(), 0.05))
    } else {
        None
    };
    let header_bg = header_tint.unwrap_or_else(Colors::surface_panel);
    let group_frame_bg = if is_dragging || is_selected || is_automation {
        header_bg
    } else {
        Colors::with_alpha(Colors::surface_canvas(), 0.22)
    };
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        track.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    // Build click handlers up-front so the closure types stay simple.
    let select_id = track_id.clone();
    let on_select_root = {
        let cb = callbacks.on_select_track.clone();
        move |event: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(
                &(
                    select_id.clone(),
                    event.modifiers.control || event.modifiers.platform,
                    event.modifiers.shift,
                ),
                window,
                cx,
            );
        }
    };

    let mute_id = track_id.clone();
    let on_mute = {
        let cb = callbacks.on_toggle_mute.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&mute_id, window, cx);
        }
    };

    let solo_id = track_id.clone();
    let on_solo = {
        let cb = callbacks.on_toggle_solo.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&solo_id, window, cx);
        }
    };

    let arm_id = track_id.clone();
    let on_arm = {
        let cb = callbacks.on_toggle_arm.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&arm_id, window, cx);
        }
    };

    let input_id = track_id.clone();
    let on_input = {
        let cb = callbacks.on_toggle_input.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&input_id, window, cx);
        }
    };

    let automation_id = track_id.clone();
    let on_automation = {
        let cb = callbacks.on_toggle_automation.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&automation_id, window, cx);
        }
    };

    let vol_id = track_id.clone();
    let on_volume_drag_start = {
        let cb = callbacks.on_volume_drag_start.clone();
        let vol_id = vol_id.clone();
        move |new_norm: &f32, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&(vol_id.clone(), *new_norm), window, cx);
        }
    };
    let on_volume_drag_preview = {
        let cb = callbacks.on_volume_drag_preview.clone();
        let vol_id = vol_id.clone();
        move |new_norm: &f32, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&(vol_id.clone(), *new_norm), window, cx);
        }
    };
    let on_volume_drag_commit = {
        let cb = callbacks.on_volume_drag_commit.clone();
        let vol_id = vol_id.clone();
        move |window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&vol_id, window, cx);
        }
    };
    let reset_vol_id = track_id.clone();
    let on_volume_reset = {
        let cb = callbacks.on_volume_change.clone();
        move |window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(
                &(
                    reset_vol_id.clone(),
                    crate::components::timeline::timeline_state::volume::db_to_norm(0.0),
                ),
                window,
                cx,
            );
        }
    };
    let pan_id = track_id.clone();
    let pan_drag_key = format!("track-pan-{}", track.id);
    let on_pan_change = callbacks.on_pan_change.clone();
    let on_pan_drag_start = callbacks.on_pan_drag_start.clone();
    let on_pan_drag_preview = callbacks.on_pan_drag_preview.clone();
    let on_pan_drag_commit = callbacks.on_pan_drag_commit.clone();
    let context_id = track_id.clone();
    let on_context = callbacks.on_context_menu.clone();
    let drag_track_id = track_id.clone();
    let drag_name = track.name.clone();
    let drag_color = track.color;
    let drop_group_id = track_id.clone();
    let assign_to_group = callbacks.on_assign_to_group.clone();
    let collapse_group_id = track_id.clone();
    let toggle_group_collapsed = callbacks.on_toggle_group_collapsed.clone();
    // The inline rename draws in place of this track's name only; every other
    // row keeps its plain name and the double-click that opens the editor.
    let rename = callbacks
        .rename
        .as_ref()
        .filter(|rename| rename.track_id == track.id)
        .cloned();
    let begin_rename = callbacks.on_begin_rename.clone();
    let rename_track_id = track_id.clone();

    div()
        .flex()
        .flex_row()
        .w(px(HEADER_WIDTH))
        .h(px(row_height))
        // Clip to the row so a mid-resize frame can never paint controls
        // outside the row bounds; the adaptive layout keeps content within.
        .overflow_hidden()
        .relative()
        // Always opaque. The header column is the one region the arrangement
        // must never show through, and a state tint cannot carry that on its
        // own — see `header_tint`.
        .bg(Colors::surface_panel())
        .opacity(if is_dragging { 0.62 } else { 1.0 })
        // Stronger right border so the header column reads as a distinct
        // pane rather than blending into the lane area. The inner accent
        // strip on the right keeps the overall feel subtle.
        .border_r(px(1.0))
        .border_color(Colors::border_strong())
        .when(!is_group_child, |header| header.border_b(px(1.0)))
        .id(("track-header", id_num))
        .when(is_group, |header| {
            let group_id_for_can_drop = drop_group_id.clone();
            header
                .can_drop(move |dragged, _window, _cx| {
                    dragged.downcast_ref::<TrackDragItem>().is_some_and(|drag| {
                        !drag.is_group && drag.track_id != group_id_for_can_drop
                    })
                })
                .drag_over::<TrackDragItem>(|style, _drag, _window, _cx| {
                    style
                        .bg(Colors::surface_selected())
                        .border_color(Colors::accent_primary())
                })
                .on_drop::<TrackDragItem>(move |drag, window, cx| {
                    assign_to_group(&(drag.track_id.clone(), drop_group_id.clone()), window, cx);
                })
        })
        .on_mouse_down(gpui::MouseButton::Left, on_select_root)
        .when_some(on_context, |this, cb| {
            this.on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                cb(&(context_id.clone(), x, y), window, cx);
            })
        })
        // State tint over the opaque base. Group children get theirs from the
        // inset frame below instead, so the tint would double up here.
        .when_some(header_tint.filter(|_| !is_group_child), |header, tint| {
            header.child(div().absolute().inset_0().bg(tint))
        })
        // Child rows share one continuous inset surface. The Folder header
        // itself keeps the standard full-row Track Header geometry.
        .when(is_group_child, |header| {
            header.child(
                div()
                    .absolute()
                    .left(px(GROUP_FRAME_INSET))
                    .right(px(GROUP_FRAME_INSET))
                    .top(px(if is_first_group_child {
                        GROUP_FRAME_CAP
                    } else {
                        0.0
                    }))
                    .bottom(px(if is_last_group_child {
                        GROUP_FRAME_CAP
                    } else {
                        0.0
                    }))
                    .bg(group_frame_bg)
                    .border_l(px(1.0))
                    .border_r(px(1.0))
                    .border_color(Colors::border_strong())
                    .when(is_first_group_child, |frame| {
                        frame.border_t(px(1.0)).rounded_t(px(radius::CONTROL))
                    })
                    .when(is_last_group_child, |frame| {
                        frame.border_b(px(1.0)).rounded_b(px(radius::CONTROL))
                    }),
            )
        })
        // Left accent strip — same column as the track lane stripe
        .when(!is_group_child, |header| {
            header.child(div().w(px(3.0)).h_full().bg(track.color))
        })
        .when(is_group_child, |header| {
            header.child(
                div()
                    .absolute()
                    .left(px(GROUP_FRAME_INSET + 1.0))
                    .top(px(if is_first_group_child {
                        GROUP_FRAME_CAP + 1.0
                    } else {
                        0.0
                    }))
                    .bottom(px(if is_last_group_child {
                        GROUP_FRAME_CAP + 1.0
                    } else {
                        0.0
                    }))
                    .w(px(3.0))
                    .bg(track.color),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                // Two-row layout spreads; compact (single row) centers vertically.
                .when(show_controls, |c| c.justify_between())
                .when(!show_controls, |c| c.justify_center())
                .flex_1()
                .min_w_0()
                // Grouped children indent past their frame's border and accent
                // strip; the ungrouped case is plain panel padding.
                .pl(px(if is_group_child {
                    GROUP_FRAME_INSET + space::BLOCK
                } else {
                    space::BASE
                }))
                .pr(px(if is_group_child {
                    GROUP_FRAME_INSET + space::BASE
                } else {
                    space::BASE
                }))
                .py(px(space::SNUG))
                // Row 1: name + type badge + per-track buttons
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .w_full()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(space::TIGHT))
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .id(("track-drag-zone", id_num))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .on_drag(
                                    TrackDragItem {
                                        track_id: drag_track_id,
                                        origin_index: index,
                                        name: drag_name.clone(),
                                        color: drag_color,
                                        is_group,
                                    },
                                    move |drag, _offset, _window, cx| {
                                        cx.new(|_| TrackDragPreview {
                                            name: drag.name.clone(),
                                            color: drag.color,
                                        })
                                    },
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        // A 20 px affordance, not a 22x30 slab:
                                        // the grip is the quietest control in
                                        // the row and was the tallest thing in
                                        // it, which is what made the name row
                                        // sit off-centre against the mixer's.
                                        .w(px(size::DENSE))
                                        .h(px(size::DENSE))
                                        .rounded(px(radius::CONTROL_SM))
                                        .id(("track-drag-handle", id_num))
                                        .cursor(gpui::CursorStyle::PointingHand)
                                        .hover(|s| s.bg(Colors::surface_hover()))
                                        .child(
                                            svg()
                                                .path(assets::ICON_GRIP_VERTICAL_PATH)
                                                .w(px(GLYPH_MD))
                                                .h(px(GLYPH_MD))
                                                .text_color(Colors::text_faint()),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(space::TIGHT))
                                        .min_w(px(0.0))
                                        .flex_1()
                                        .overflow_hidden()
                                        .when(is_group, |row| {
                                            row.child(
                                                div()
                                                    .id(("folder-collapse", id_num))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .w(px(size::MICRO))
                                                    .h(px(size::MICRO))
                                                    .rounded(px(radius::CONTROL_SM))
                                                    .hover(|style| {
                                                        style.bg(Colors::surface_hover())
                                                    })
                                                    .on_mouse_down(
                                                        gpui::MouseButton::Left,
                                                        move |_event, window, cx| {
                                                            toggle_group_collapsed(
                                                                &collapse_group_id,
                                                                window,
                                                                cx,
                                                            );
                                                            cx.stop_propagation();
                                                        },
                                                    )
                                                    .occlude()
                                                    .child(
                                                        svg()
                                                            .path(if track.group_collapsed {
                                                                assets::ICON_CHEVRON_RIGHT_PATH
                                                            } else {
                                                                assets::ICON_CHEVRON_DOWN_PATH
                                                            })
                                                            .w(px(GLYPH_SM))
                                                            .h(px(GLYPH_SM))
                                                            .text_color(Colors::text_secondary()),
                                                    ),
                                            )
                                            .child(
                                                svg()
                                                    .path(assets::ICON_FOLDER_PATH)
                                                    .w(px(GLYPH_MD))
                                                    .h(px(GLYPH_MD))
                                                    .text_color(Colors::accent_primary()),
                                            )
                                        })
                                        .child(match rename {
                                            // The field stops its own presses,
                                            // so editing never reselects the
                                            // track or starts a reorder drag.
                                            Some(rename) => div()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .child(text_field_with_callbacks_and_ime(
                                                    &rename.input,
                                                    rename.focused,
                                                    rename.callbacks.clone(),
                                                    rename.ime_target.clone(),
                                                ))
                                                .into_any_element(),
                                            None => div()
                                                .id(("track-name", id_num))
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .overflow_hidden()
                                                .truncate()
                                                .text_size(px(typography::UI_SM))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(Colors::text_primary())
                                                .child(track.name.clone())
                                                .on_click(move |event, window, cx| {
                                                    if opens_track_rename(
                                                        event,
                                                        cx.has_active_drag(),
                                                    ) {
                                                        begin_rename(&rename_track_id, window, cx);
                                                    }
                                                })
                                                .into_any_element(),
                                        }),
                                ),
                        )
                        .when_some(
                            callbacks
                                .on_open_instrument
                                .clone()
                                .filter(|_| track.track_type == TrackType::Instrument),
                            |row, on_open| {
                                row.child(instrument_header_button(track, id_num, on_open))
                            },
                        )
                        .child(latch_strip(
                            track,
                            is_automation,
                            id_num,
                            LatchStripHandlers {
                                on_mute,
                                on_solo,
                                on_arm,
                                on_input,
                                on_automation,
                            },
                        )),
                )
                // Row 2: horizontal volume fader + pan pill + meter + dB pill.
                // Only rendered when the row is tall enough to hold it; the
                // compact header (short rows) shows just row 1.
                .when(show_controls, |col| {
                    col.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(space::SNUG))
                            .w_full()
                            .px(px(space::SNUG))
                            // 24px horizontal fader + 2px vertical padding on
                            // each side keeps the two-row header at its exact
                            // 72px intrinsic height.
                            .py(px(space::HAIR))
                            .rounded(px(radius::CONTROL))
                            // The same recessed plane the transport readout and
                            // the mixer's fader well use. It was a 16%-alpha
                            // wash with a 3%-alpha border — a plate you could
                            // not actually see, which is why the control row
                            // read as loose parts instead of one instrument.
                            .bg(Colors::surface_canvas())
                            .border(px(1.0))
                            .border_color(Colors::border_subtle())
                            // Mixer fader geometry, rotated for the compact
                            // horizontal TrackHeader control row.
                            .child(horizontal_fader_with_drag_callbacks(
                                format!("track-vol-{}", track.id),
                                state.display_track_volume(track),
                                Colors::accent_primary(),
                                Some(on_volume_drag_start),
                                Some(on_volume_drag_preview),
                                Some(on_volume_drag_commit),
                                Some(on_volume_reset),
                            ))
                            // Pan readout — compact bordered label matching the
                            // dB pill alongside it. Vertical drag scrubs pan;
                            // Shift gives fine control and double-click resets C.
                            .child({
                                let border = if is_selected {
                                    let mut c = track.color;
                                    c.a = 0.55;
                                    c
                                } else {
                                    Colors::border_default()
                                };
                                let drag_key = pan_drag_key.clone();
                                let drag_start_cb = on_pan_drag_start.clone();
                                let drag_preview_cb = on_pan_drag_preview.clone();
                                let drag_commit_cb = on_pan_drag_commit.clone();
                                let drag_track_id = pan_id.clone();
                                let start_track_id = pan_id.clone();
                                let commit_track_id = pan_id.clone();
                                let commit_track_id_out = pan_id.clone();
                                let reset_cb = on_pan_change.clone();
                                let reset_track_id = pan_id.clone();
                                let start_pan = track.pan;
                                div()
                                    .id(("track-pan-spinner", id_num))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .min_w(px(size::COMFORTABLE))
                                    .px(px(space::TIGHT))
                                    .h(px(PAN_PILL_H))
                                    // A 14 px control takes the small tier. At
                                    // `CONTROL` it read rounder than the 16 px
                                    // state pills stacked directly above it.
                                    .rounded(px(radius::CONTROL_SM))
                                    // One step up from the plate it sits in, so the readout
                                    // reads as a control on the plate rather than
                                    // a hole through it.
                                    .bg(Colors::surface_panel_alt())
                                    .border(px(1.0))
                                    .border_color(border)
                                    .text_size(px(typography::DENSE_CAPTION))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::text_secondary())
                                    .cursor(gpui::CursorStyle::ResizeUpDown)
                                    .hover(|style| {
                                        style
                                            .bg(Colors::surface_control_hover())
                                            .border_color(Colors::border_strong())
                                    })
                                    .child(format_pan_label(track.pan))
                                    .on_drag(
                                        SpinDrag::new(drag_key.clone(), f64::from(start_pan)),
                                        move |drag, _offset, window, cx| {
                                            drag.begin();
                                            drag_start_cb(&start_track_id, window, cx);
                                            cx.new(|_| drag.clone())
                                        },
                                    )
                                    .on_drag_move::<SpinDrag>(
                                        move |event: &DragMoveEvent<SpinDrag>, window, cx| {
                                            let drag = event.drag(cx);
                                            if !drag.matches(&drag_key) {
                                                return;
                                            }
                                            let current_y: f32 = event.event.position.y.into();
                                            let sensitivity = if event.event.modifiers.shift {
                                                0.002
                                            } else {
                                                2.0 / 150.0
                                            };
                                            let next = drag.value_at(
                                                current_y,
                                                sensitivity,
                                                -1.0,
                                                1.0,
                                                Some(0.001),
                                            )
                                                as f32;
                                            drag_preview_cb(
                                                &(drag_track_id.clone(), next),
                                                window,
                                                cx,
                                            );
                                        },
                                    )
                                    .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
                                        drag_commit_cb(&commit_track_id, window, cx)
                                    })
                                    .on_mouse_up_out(
                                        gpui::MouseButton::Left,
                                        move |_, window, cx| {
                                            on_pan_drag_commit(&commit_track_id_out, window, cx)
                                        },
                                    )
                                    .on_click(move |event, window, cx| {
                                        if event.click_count() >= 2 {
                                            reset_cb(&(reset_track_id.clone(), 0.0), window, cx);
                                        }
                                    })
                            })
                            // Compact meter. Its own entity, so a level change
                            // repaints the bar and not the arrangement behind
                            // it; the inline draw is the fallback for the frame
                            // before the entity exists.
                            .child(match meters.get(track.id.as_str()) {
                                Some(meter) => meter.clone().into_any_element(),
                                None => {
                                    vu_meter_with_levels(track.meter_level_l, track.meter_level_r)
                                        .into_any_element()
                                }
                            })
                            // Bordered dB pill
                            .child(db_value_pill(
                                volume::format_db(state.display_track_volume(track)),
                                is_selected,
                            )),
                    )
                })
                // Row 3: the take sub-lane, in whatever room is left below the
                // control row. Absent on a track with no takes, and on a row
                // too short to hold it.
                .children(take_sublane(
                    track,
                    row_height,
                    callbacks.on_toggle_takes.clone(),
                    callbacks.on_select_take.clone(),
                    callbacks.on_delete_take.clone(),
                )),
        )
}

#[cfg(test)]
mod rename_gesture_tests {
    use super::opens_track_rename;
    use gpui::{point, px, Modifiers};

    fn click(travel: f32, click_count: usize, down: Modifiers, up: Modifiers) -> gpui::ClickEvent {
        gpui::ClickEvent::Mouse(gpui::MouseClickEvent {
            down: gpui::MouseDownEvent {
                position: point(px(40.0), px(12.0)),
                modifiers: down,
                ..Default::default()
            },
            up: gpui::MouseUpEvent {
                position: point(px(40.0 + travel), px(12.0)),
                modifiers: up,
                click_count,
                ..Default::default()
            },
        })
    }

    fn plain() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn a_plain_stationary_double_click_opens_the_editor() {
        assert!(opens_track_rename(&click(0.0, 2, plain(), plain()), false));
        assert!(opens_track_rename(&click(2.0, 2, plain(), plain()), false));
    }

    #[test]
    fn a_single_click_only_selects() {
        assert!(!opens_track_rename(&click(0.0, 1, plain(), plain()), false));
    }

    /// Two quick reorder drags started on the name arrive as a double-click.
    #[test]
    fn drags_never_open_the_editor() {
        assert!(!opens_track_rename(
            &click(40.0, 2, plain(), plain()),
            false
        ));
        assert!(!opens_track_rename(&click(0.0, 2, plain(), plain()), true));
    }

    /// A modified double-click is the header's selection gesture twice over.
    #[test]
    fn a_modified_double_click_does_not_open_the_editor() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert!(!opens_track_rename(&click(0.0, 2, cmd, cmd), false));
        assert!(!opens_track_rename(&click(0.0, 2, shift, plain()), false));
        assert!(!opens_track_rename(&click(0.0, 2, plain(), alt), false));
    }
}
