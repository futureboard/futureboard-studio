use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    bounds, div, point, px, size, svg, App, InteractiveElement, IntoElement, ParentElement,
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
/// Fold or unfold one group of the picker's tree, by its key.
pub type AutomationTargetPickerToggleCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;

const GROUP_TRACK: &str = "track";
const GROUP_SENDS: &str = "sends";

/// The key a plug-in's group folds by.
pub fn plugin_group_key(insert_id: &str) -> String {
    format!("plugin:{insert_id}")
}

/// Whether a group of the tree shows its rows. `flipped` holds the groups the
/// user toggled away from their default (the track's own groups open, each
/// plug-in folded); a search opens every group it finds something in.
pub fn picker_group_open(
    flipped: &HashSet<String>,
    key: &str,
    default_open: bool,
    searching: bool,
) -> bool {
    searching || default_open != flipped.contains(key)
}

const PICKER_WIDTH: f32 = 360.0;
const PICKER_MAX_HEIGHT: f32 = 520.0;
const ROW_HEIGHT: f32 = 26.0;
const SECTION_PAD: f32 = 8.0;
/// How far a group's rows sit in from its header.
const GROUP_INDENT: f32 = 15.0;

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
    open_groups: &HashSet<String>,
    on_toggle_group: AutomationTargetPickerToggleCb,
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

    // A tree: the track's own controls and its sends first, open, then each
    // plug-in folded under its name with its parameter count. A sampler lists
    // hundreds of parameters, and flat they buried everything after them.
    let searching = !query.trim().is_empty();
    let (sends, mixer): (Vec<_>, Vec<_>) = model
        .track_targets
        .iter()
        .filter(|(_, label, _)| query_matches(query, label))
        .map(|(target, label, already_added)| (label.clone(), *already_added, target.clone()))
        .partition(|(_, _, target)| {
            matches!(
                target,
                AutomationTarget::SendLevel { .. } | AutomationTarget::SendEnabled { .. }
            )
        });
    let tree = TreeContext {
        track_id,
        flipped: open_groups,
        searching,
        on_command: &on_command,
        on_toggle: &on_toggle_group,
    };
    if !mixer.is_empty() {
        body = body.child(tree.group(GROUP_TRACK, "Track", true, &mixer, false));
    }
    if !sends.is_empty() {
        body = body.child(tree.group(GROUP_SENDS, "Sends", true, &sends, false));
    }

    if let Some(group) = model.instrument.as_ref() {
        let rows = filter_plugin_group(group, query);
        if !rows.is_empty() || (group.parameters.is_empty() && !searching) {
            body = body.child(separator_label("INSTRUMENT"));
            body = body.child(tree.plugin(group, &rows));
        }
    }

    let effect_groups: Vec<_> = model
        .effects
        .iter()
        .map(|group| (group, filter_plugin_group(group, query)))
        .filter(|(group, rows)| !rows.is_empty() || (group.parameters.is_empty() && !searching))
        .collect();
    if !effect_groups.is_empty() {
        body = body.child(separator_label("EFFECTS"));
        for (group, rows) in effect_groups {
            body = body.child(tree.plugin(group, &rows));
        }
    }
    if searching && body_is_empty(model, query) {
        body = body.child(
            div()
                .px(px(SECTION_PAD))
                .py(px(8.0))
                .text_size(px(11.0))
                .text_color(Colors::text_muted())
                .child("No matching parameters"),
        );
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
        // The arrangement under the picker must not hear its wheel or its
        // clicks: a wheel over the list scrolled the timeline instead, and a
        // press between rows reached the lane below.
        .occlude()
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

/// Whether a search finds nothing anywhere in the picker.
fn body_is_empty(model: &AutomationPickerModel, query: &str) -> bool {
    !model
        .track_targets
        .iter()
        .any(|(_, label, _)| query_matches(query, label))
        && model
            .instrument
            .iter()
            .chain(model.effects.iter())
            .all(|group| filter_plugin_group(group, query).is_empty())
}

/// What every group of the picker's tree is drawn with.
struct TreeContext<'a> {
    track_id: &'a str,
    flipped: &'a HashSet<String>,
    searching: bool,
    on_command: &'a AutomationTargetPickerCommandCb,
    on_toggle: &'a AutomationTargetPickerToggleCb,
}

impl TreeContext<'_> {
    /// A plug-in's parameters, folded unless the user opened it.
    fn plugin(
        &self,
        group: &AutomationPickerPluginGroup,
        rows: &[(String, bool, AutomationTarget)],
    ) -> gpui::AnyElement {
        self.group(
            &plugin_group_key(&group.insert_id),
            &group.plugin_name,
            false,
            rows,
            group.parameters.is_empty(),
        )
    }

    /// One foldable group: a header with its chevron and row count, and its
    /// rows under it while open.
    fn group(
        &self,
        key: &str,
        title: &str,
        default_open: bool,
        rows: &[(String, bool, AutomationTarget)],
        loading: bool,
    ) -> gpui::AnyElement {
        let open = picker_group_open(self.flipped, key, default_open, self.searching);
        let mut section = div()
            .flex()
            .flex_col()
            .px(px(SECTION_PAD))
            .child(self.header(key, title, open, (!loading).then_some(rows.len())));
        if !open {
            return section.into_any_element();
        }
        let mut list = div().flex().flex_col().pl(px(GROUP_INDENT));
        if loading {
            list = list.child(
                div()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .text_size(px(11.0))
                    .text_color(Colors::text_muted())
                    .child("Loading parameters…"),
            );
        }
        for (label, already_added, target) in rows {
            let cmd = automation_target_menu_command(self.track_id, target);
            let on_command = self.on_command.clone();
            list = list.child(picker_row(
                label,
                *already_added,
                (!*already_added).then(|| -> Arc<dyn Fn(&mut Window, &mut App) + 'static> {
                    Arc::new(move |window, cx| on_command(&cmd, window, cx))
                }),
            ));
        }
        section = section.child(list);
        section.into_any_element()
    }

    fn header(&self, key: &str, title: &str, open: bool, count: Option<usize>) -> impl IntoElement {
        let chevron = if open {
            crate::assets::ICON_CHEVRON_DOWN_PATH
        } else {
            crate::assets::ICON_CHEVRON_RIGHT_PATH
        };
        let mut header = div()
            .h(px(ROW_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(6.0))
            .rounded(px(crate::theme::radius::CONTROL_SM));
        // A search holds every group it matches open, so a fold would show
        // nothing until the search is cleared: the headers only fold without
        // one.
        if !self.searching {
            let on_toggle = self.on_toggle.clone();
            let key = key.to_string();
            header = header
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_raised()))
                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                    on_toggle(&key, window, cx);
                });
        }
        header
            .child(
                svg()
                    .path(chevron)
                    .w(px(9.0))
                    .h(px(9.0))
                    .flex_none()
                    .text_color(Colors::text_muted()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(title.to_string()),
            )
            .children(count.map(|count| {
                div()
                    .flex_none()
                    .text_size(px(10.0))
                    .text_color(Colors::text_muted())
                    .child(count.to_string())
            }))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::AutomationPickerPluginGroup;
    use gpui::{Context, Render};
    use std::cell::Cell;
    use std::rc::Rc;

    /// An arrangement stand-in that counts the wheel events it hears, with
    /// the picker open over it.
    struct Behind {
        wheels: Rc<Cell<usize>>,
        model: AutomationPickerModel,
        search: TextInputState,
    }

    impl Render for Behind {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let wheels = self.wheels.clone();
            div()
                .id("behind")
                .relative()
                .size_full()
                .on_scroll_wheel(move |_, _, _| wheels.set(wheels.get() + 1))
                .child(automation_target_picker_overlay(
                    &self.model,
                    "track-1",
                    "",
                    &self.search,
                    false,
                    20.0,
                    20.0,
                    800.0,
                    700.0,
                    Arc::new(|_, _, _| {}),
                    Arc::new(|_, _, _| {}),
                    TextInputCallbacks::default(),
                    &HashSet::new(),
                    Arc::new(|_, _, _| {}),
                ))
        }
    }

    /// The track's own groups start open and a plug-in folded; a toggle
    /// flips each from there, and a search opens them all.
    #[test]
    fn groups_fold_from_their_defaults_and_a_search_opens_them() {
        let none = HashSet::new();
        assert!(picker_group_open(&none, GROUP_TRACK, true, false));
        assert!(!picker_group_open(&none, "plugin:insert-1", false, false));

        let flipped: HashSet<String> = [GROUP_TRACK.to_string(), plugin_group_key("insert-1")]
            .into_iter()
            .collect();
        assert!(!picker_group_open(&flipped, GROUP_TRACK, true, false));
        assert!(picker_group_open(&flipped, "plugin:insert-1", false, false));
        assert!(picker_group_open(&flipped, GROUP_TRACK, true, true));
        assert!(picker_group_open(&none, "plugin:insert-1", false, true));
    }

    /// A search that matches nothing anywhere says so; one that matches a
    /// send does not.
    #[test]
    fn an_empty_search_result_is_detected() {
        let model = AutomationPickerModel {
            track_targets: vec![(
                AutomationTarget::SendLevel {
                    send_id: "send-1".to_string(),
                },
                "Send \u{2192} Reverb".to_string(),
                false,
            )],
            ..Default::default()
        };
        assert!(!body_is_empty(&model, "reverb"));
        assert!(body_is_empty(&model, "cutoff"));
    }

    /// A wheel over the picker scrolls its list, not the arrangement under
    /// it; a wheel beside it still reaches the arrangement.
    #[gpui::test]
    fn the_arrangement_under_the_picker_does_not_hear_its_wheel(cx: &mut gpui::TestAppContext) {
        let wheels = Rc::new(Cell::new(0));
        let seen = wheels.clone();
        let model = AutomationPickerModel {
            instrument: Some(AutomationPickerPluginGroup {
                insert_id: "insert-1".to_string(),
                plugin_name: "Sampler".to_string(),
                parameters: Vec::new(),
            }),
            track_targets: vec![(AutomationTarget::TrackPan, "Pan".to_string(), false)],
            ..Default::default()
        };
        let window = cx.open_window(gpui::size(px(800.0), px(700.0)), move |_, cx| Behind {
            wheels: seen,
            model,
            search: TextInputState::new("search", cx.focus_handle()),
        });
        cx.run_until_parked();
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        let wheel = |x: f32, y: f32| gpui::ScrollWheelEvent {
            position: point(px(x), px(y)),
            delta: gpui::ScrollDelta::Lines(point(0.0, -3.0)),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        };
        vcx.simulate_event(wheel(120.0, 160.0));
        vcx.run_until_parked();
        assert_eq!(wheels.get(), 0, "the picker keeps its wheel");
        vcx.simulate_event(wheel(700.0, 600.0));
        vcx.run_until_parked();
        assert_eq!(wheels.get(), 1, "outside it the arrangement still scrolls");
    }
}
