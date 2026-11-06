use std::sync::Arc;

use gpui::{
    bounds, div, point, px, size, App, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::timeline_state::{
    automation_target_menu_command, AutomationPickerModel, AutomationPickerPluginGroup,
    AutomationTarget,
};
use crate::overlay::{
    compute_overlay_position, pointer_anchor, OverlayPlacement, OverlaySize, OVERLAY_WINDOW_MARGIN,
};
use crate::theme::Colors;

pub type AutomationTargetPickerCommandCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
pub type AutomationTargetPickerCloseCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

const PICKER_WIDTH: f32 = 360.0;
const PICKER_MAX_HEIGHT: f32 = 520.0;
const ROW_HEIGHT: f32 = 26.0;
const SECTION_PAD: f32 = 8.0;

fn query_matches(query: &str, haystack: &str) -> bool {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    haystack.to_ascii_lowercase().contains(&q)
}

fn filter_plugin_group(
    group: &AutomationPickerPluginGroup,
    query: &str,
) -> Vec<(String, bool, AutomationTarget)> {
    group
        .parameters
        .iter()
        .filter(|row| {
            query_matches(query, &row.param_title) || query_matches(query, &group.plugin_name)
        })
        .map(|row| {
            (
                row.param_title.clone(),
                row.already_added,
                row.target.clone(),
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn automation_target_picker_overlay(
    model: &AutomationPickerModel,
    track_id: &str,
    query: &str,
    search_input: &TextInputState,
    search_focused: bool,
    anchor_x: f32,
    anchor_y: f32,
    viewport_width: f32,
    viewport_height: f32,
    on_command: AutomationTargetPickerCommandCb,
    on_close: AutomationTargetPickerCloseCb,
    search_callbacks: TextInputCallbacks,
) -> impl IntoElement {
    let window_bounds = bounds(
        point(px(0.0), px(0.0)),
        size(px(viewport_width), px(viewport_height)),
    );
    let position = compute_overlay_position(
        pointer_anchor(anchor_x, anchor_y).bounds,
        OverlaySize {
            width: PICKER_WIDTH,
            height: PICKER_MAX_HEIGHT,
        },
        window_bounds,
        OverlayPlacement::Pointer,
        OVERLAY_WINDOW_MARGIN,
    );
    let left: f32 = position.x.into();
    let top: f32 = position.y.into();

    let mut body = div().flex().flex_col().gap(px(4.0));

    if let Some(last) = model.last_touched.as_ref() {
        let label = format!("Add Last Touched: {}", last.display_label());
        let cmd = automation_target_menu_command(track_id, &last.automation_target());
        let on_command = on_command.clone();
        body = body.child(picker_row(
            &label,
            false,
            Some(Arc::new(move |window, cx| on_command(&cmd, window, cx))),
        ));
        body = body.child(div().h(px(1.0)).bg(Colors::border_subtle()));
    }

    if let Some(group) = model.instrument.as_ref() {
        let rows = filter_plugin_group(group, query);
        if !rows.is_empty() || group.parameters.is_empty() {
            body = body.child(plugin_section(group, track_id, &rows, &on_command));
        }
    }

    let effect_groups: Vec<_> = model
        .effects
        .iter()
        .map(|group| (group, filter_plugin_group(group, query)))
        .filter(|(group, rows)| !rows.is_empty() || group.parameters.is_empty())
        .collect();
    if !effect_groups.is_empty() {
        body = body.child(separator_label("EFFECTS"));
        for (group, rows) in effect_groups {
            body = body.child(plugin_section(group, track_id, &rows, &on_command));
        }
    }

    body = body.child(
        div()
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_secondary())
            .px(px(SECTION_PAD))
            .pt(px(6.0))
            .child("Track"),
    );
    for (target, label, already_added) in &model.track_targets {
        let label = label.clone();
        let cmd = automation_target_menu_command(track_id, target);
        let on_command = on_command.clone();
        body = body.child(picker_row(
            &label,
            *already_added,
            if *already_added {
                None
            } else {
                Some(Arc::new(move |window, cx| on_command(&cmd, window, cx)))
            },
        ));
    }

    let close_target = on_close.clone();
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(PICKER_WIDTH))
        .max_h(px(PICKER_MAX_HEIGHT))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .rounded(px(crate::theme::radius::CONTROL))
        .shadow_md()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down_out(move |_, window, cx| {
            close_target(&(), window, cx);
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px(px(SECTION_PAD))
                .py(px(6.0))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("Add Automation"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(Colors::text_muted())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                            on_close(&(), window, cx);
                        })
                        .child("Esc"),
                ),
        )
        .child(
            div()
                .px(px(SECTION_PAD))
                .py(px(6.0))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .child(text_field_with_callbacks(
                    search_input,
                    search_focused,
                    search_callbacks,
                )),
        )
        .child(
            div()
                .id("automation-target-picker-body")
                .max_h(px(PICKER_MAX_HEIGHT - 72.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(body),
        )
}

fn separator_label(text: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(SECTION_PAD))
        .py(px(6.0))
        .child(div().flex_1().h(px(1.0)).bg(Colors::border_subtle()))
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_muted())
                .child(text.to_string()),
        )
        .child(div().flex_1().h(px(1.0)).bg(Colors::border_subtle()))
}

fn plugin_section(
    group: &AutomationPickerPluginGroup,
    track_id: &str,
    rows: &[(String, bool, AutomationTarget)],
    on_command: &AutomationTargetPickerCommandCb,
) -> impl IntoElement {
    let mut section = div()
        .flex()
        .flex_col()
        .px(px(SECTION_PAD))
        .pb(px(4.0))
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .pb(px(4.0))
                .child(group.plugin_name.clone()),
        );

    if group.parameters.is_empty() {
        section = section.child(
            div()
                .text_size(px(11.0))
                .text_color(Colors::text_muted())
                .py(px(4.0))
                .child("Loading parameters…"),
        );
    } else if rows.is_empty() {
        section = section.child(
            div()
                .text_size(px(11.0))
                .text_color(Colors::text_muted())
                .py(px(4.0))
                .child("No matching parameters"),
        );
    } else {
        for (label, already_added, target) in rows {
            let cmd = automation_target_menu_command(track_id, target);
            let on_command = on_command.clone();
            section = section.child(picker_row(
                label,
                *already_added,
                if *already_added {
                    None
                } else {
                    Some(Arc::new(move |window, cx| on_command(&cmd, window, cx)))
                },
            ));
        }
    }

    section
}

fn picker_row(
    label: &str,
    disabled: bool,
    on_click: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
) -> impl IntoElement {
    let text_color = if disabled {
        Colors::text_muted()
    } else {
        Colors::text_primary()
    };
    let suffix = if disabled { " (already added)" } else { "" };
    let mut row = div()
        .h(px(ROW_HEIGHT))
        .flex()
        .items_center()
        .px(px(8.0))
        .rounded(px(crate::theme::radius::CONTROL_SM))
        .text_size(px(11.0))
        .text_color(text_color)
        .child(format!("{label}{suffix}"));
    if let Some(handler) = on_click {
        row = row
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::surface_raised()))
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                handler(window, cx)
            });
    }
    row
}
