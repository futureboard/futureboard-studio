use crate::assets;
use crate::components::timeline::automation_lane::{
    AUTOMATION_CONTENT_X, AUTOMATION_SUBLANE_RAIL_X, AUTOMATION_TRACK_STRIP_W,
};
use crate::components::timeline::timeline_state::{
    LastTouchedPluginParam, TimelineState, AUTOMATION_CONTROL_LANE_HEIGHT, HEADER_WIDTH,
    MASTER_TRACK_ID,
};
use crate::theme::{radius, space, typography, Colors};
use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled,
};

/// Actions fired from the automation control lane. UI-only row — never serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationControlAction {
    /// Open the automation target picker (`x`, `y` are window-space anchors).
    OpenTargetPicker,
    /// Add automation for the last touched VST3 parameter on this track.
    AddLastTouched,
    /// Collapse the automation section (hide control + sub-lanes).
    HideAutomation,
    /// Request confirmation before removing all automation lanes.
    RequestClearAll,
}

/// Payload: `(track_id, action, window_x, window_y)`.
pub type AutomationControlCallback = std::sync::Arc<
    dyn Fn(&(String, AutomationControlAction, f32, f32), &mut gpui::Window, &mut gpui::App)
        + 'static,
>;

/// UI-only management row rendered directly below the parent track and above
/// automation sub-lanes. Not an audio track, not an envelope lane.
pub fn automation_control_lane(
    track_id: &str,
    track_color: gpui::Rgba,
    lane_height: f32,
    state: &TimelineState,
    on_action: Option<AutomationControlCallback>,
) -> impl IntoElement {
    let track_id = track_id.to_string();
    let last_touched = state
        .last_touched_plugin_param_for_track(&track_id)
        .cloned();
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        track_id.hash(&mut hasher);
        hasher.finish() as usize
    };

    let lane_count = state
        .automation_lanes_of(&track_id)
        .map_or(0, |lanes| lanes.iter().filter(|l| l.visible).count());
    let header = control_header(
        &track_id,
        track_color,
        lane_count,
        last_touched.as_ref(),
        id_num,
        on_action,
    );

    // Right-side management strip stays on the neutral timeline canvas, not a
    // tinted block — only the left header carries any tint.
    let timeline_bg = div()
        .flex_1()
        .h_full()
        .bg(Colors::automation_canvas_bg())
        .border_b(px(1.0))
        .border_color(Colors::with_alpha(Colors::automation_separator(), 0.7));

    let is_master = track_id == MASTER_TRACK_ID;
    div()
        .flex()
        .flex_row()
        .w_full()
        .h(px(lane_height))
        // The master block starts a section of its own below the tracks.
        .when(is_master, |row| {
            row.border_t(px(1.0)).border_color(Colors::border_strong())
        })
        // No row-level fill: the header paints its own (opaque) label surface and
        // the right strip is translucent so the timeline grid shows through.
        .border_b(px(1.0))
        .border_color(Colors::with_alpha(Colors::automation_separator(), 0.7))
        .child(header)
        .child(timeline_bg)
}

/// How a control-row button reads at rest. Only Add carries the accent;
/// Clear All is neutral until hovered, when it reads as the danger it is.
#[derive(Clone, Copy)]
enum ControlButtonStyle {
    Primary,
    Neutral,
    Danger,
}

#[allow(clippy::too_many_arguments)]
fn control_header(
    track_id: &str,
    track_color: gpui::Rgba,
    lane_count: usize,
    last_touched: Option<&LastTouchedPluginParam>,
    id_num: usize,
    on_action: Option<AutomationControlCallback>,
) -> impl IntoElement {
    // The master's block has no track row above it, so its control row is
    // its header: it names the master, and hangs off nothing.
    let is_master = track_id == MASTER_TRACK_ID;
    // Title: the section's glyph, its name and how many lanes it holds — the
    // count is what says there is more under a collapsed section.
    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .min_w(px(0.0))
        .child(
            svg()
                .path(if is_master {
                    assets::ICON_VOLUME_2_PATH
                } else {
                    assets::ICON_AUTOMATION_PATH
                })
                .flex_none()
                .w(px(14.0))
                .h(px(14.0))
                .text_color(Colors::state_automation()),
        )
        .child(
            div()
                .truncate()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(if is_master {
                    "Master Automation"
                } else {
                    "Automation"
                }),
        )
        // The count as a badge in the section's hue, so a collapsed section
        // still says how much is under it.
        .when(lane_count > 0, |row| {
            let hue = Colors::state_automation();
            row.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(16.0))
                    .h(px(16.0))
                    .px(px(space::TIGHT))
                    .rounded(px(crate::theme::radius::PILL))
                    .bg(Colors::with_alpha(hue, 0.16))
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(hue)
                    .child(lane_count.to_string()),
            )
        });

    let buttons = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .flex_none()
        .children(last_touched.map(|touched| {
            // The parameter just moved in a plug-in, one click from a lane.
            let short = truncate_label(&touched.display_label(), 14);
            control_chip(
                ("automation-ctrl-last", id_num).into(),
                short,
                "Add a lane for the parameter you last touched",
                track_id,
                AutomationControlAction::AddLastTouched,
                on_action.clone(),
            )
        }))
        .child(control_button(
            ("automation-ctrl-add", id_num).into(),
            assets::ICON_PLUS_PATH,
            "Add parameter lane",
            ControlButtonStyle::Primary,
            track_id,
            AutomationControlAction::OpenTargetPicker,
            on_action.clone(),
        ))
        .child(control_button(
            ("automation-ctrl-hide", id_num).into(),
            assets::ICON_CHEVRON_UP_PATH,
            "Hide automation",
            ControlButtonStyle::Neutral,
            track_id,
            AutomationControlAction::HideAutomation,
            on_action.clone(),
        ))
        .child(control_button(
            ("automation-ctrl-clear", id_num).into(),
            assets::ICON_TRASH_PATH,
            if is_master {
                "Remove all master automation…"
            } else {
                "Remove all automation on this track…"
            },
            ControlButtonStyle::Danger,
            track_id,
            AutomationControlAction::RequestClearAll,
            on_action,
        ));

    div()
        .relative()
        .w(px(HEADER_WIDTH))
        .h(px(AUTOMATION_CONTROL_LANE_HEIGHT))
        .flex_none()
        .bg(Colors::automation_lane_header_bg())
        .border_r(px(1.0))
        .border_color(Colors::border_subtle())
        // The track's colour strip, carried down from its header.
        .when(!is_master, |header| {
            header.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(AUTOMATION_TRACK_STRIP_W))
                    .bg(Colors::with_alpha(track_color, 0.6)),
            )
        })
        // The guide every lane row continues, hanging from the section glyph
        // so the lanes read as what it holds.
        .child(
            div()
                .absolute()
                .left(px(AUTOMATION_SUBLANE_RAIL_X))
                .top(px(AUTOMATION_CONTROL_LANE_HEIGHT / 2.0 + 8.0))
                .bottom_0()
                .w(px(1.0))
                .bg(Colors::automation_rail()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::SNUG))
                .w_full()
                .h_full()
                .pl(px(AUTOMATION_CONTENT_X))
                .pr(px(space::SNUG))
                .child(title)
                .child(buttons),
        )
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    let mut out: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn fire(
    cb: Option<AutomationControlCallback>,
    track_id: String,
    action: AutomationControlAction,
) -> impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    move |event, window, cx| {
        cx.stop_propagation();
        if let Some(cb) = cb.as_ref() {
            let x: f32 = event.position.x.into();
            let y: f32 = event.position.y.into();
            cb(&(track_id.clone(), action, x, y), window, cx);
        }
    }
}

/// A 20 px icon button of the control row.
fn control_button(
    id: gpui::ElementId,
    icon: &'static str,
    tooltip: &'static str,
    style: ControlButtonStyle,
    track_id: &str,
    action: AutomationControlAction,
    cb: Option<AutomationControlCallback>,
) -> impl IntoElement {
    let rest = Colors::automation_lane_header_bg();
    let (fill, glyph, hover_fill, hover_glyph) = match style {
        ControlButtonStyle::Primary => {
            let (fill, _) = Colors::latched(rest, Colors::state_automation());
            (
                fill,
                Colors::state_automation(),
                Colors::composite(fill, Colors::state_hover()),
                Colors::state_automation(),
            )
        }
        ControlButtonStyle::Neutral => (
            Colors::with_alpha(rest, 0.0),
            Colors::text_muted(),
            Colors::composite(rest, Colors::state_hover()),
            Colors::text_secondary(),
        ),
        ControlButtonStyle::Danger => (
            Colors::with_alpha(rest, 0.0),
            Colors::text_muted(),
            Colors::composite(rest, Colors::with_alpha(Colors::status_error(), 0.18)),
            Colors::status_error(),
        ),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(20.0))
        .h(px(20.0))
        .rounded(px(radius::CONTROL))
        .bg(fill)
        .text_color(glyph)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover_fill).text_color(hover_glyph))
        .tooltip(crate::components::fb_tooltip(tooltip))
        .on_mouse_down(
            gpui::MouseButton::Left,
            fire(cb, track_id.to_string(), action),
        )
        .child(svg().path(icon).w(px(12.0)).h(px(12.0)).text_color(glyph))
}

/// A text chip of the control row (the last-touched parameter).
fn control_chip(
    id: gpui::ElementId,
    label: String,
    tooltip: &'static str,
    track_id: &str,
    action: AutomationControlAction,
    cb: Option<AutomationControlCallback>,
) -> impl IntoElement {
    let rest = Colors::automation_lane_header_bg();
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .h(px(20.0))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_secondary())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .tooltip(crate::components::fb_tooltip(tooltip))
        .on_mouse_down(
            gpui::MouseButton::Left,
            fire(cb, track_id.to_string(), action),
        )
        .child(
            svg()
                .path(assets::ICON_PLUS_PATH)
                .w(px(10.0))
                .h(px(10.0))
                .text_color(Colors::text_muted()),
        )
        .child(div().truncate().child(label))
}
