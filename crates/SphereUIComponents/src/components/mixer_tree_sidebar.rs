//! Collapsible mixer channel tree sidebar — left navigator inside the Mixer tab.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, uniform_list, App, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
};

use crate::assets;
use crate::components::icon_button::icon_button;
use crate::components::mixer_tree_model::{MixerTreeModel, MixerTreeRow};
use crate::components::text_input::{
    text_field_with_callbacks, TextInputCallbacks, TextInputContextCb, TextInputState,
};
use crate::components::timeline::timeline_state::MixerTreeViewState;
use crate::theme::Colors;

pub const MIXER_TREE_SIDEBAR_DEFAULT_WIDTH: f32 = 240.0;
pub const MIXER_TREE_SIDEBAR_MIN_WIDTH: f32 = 180.0;
pub const MIXER_TREE_SIDEBAR_MAX_WIDTH: f32 = 360.0;
pub const MIXER_TREE_COLLAPSED_RAIL_WIDTH: f32 = 28.0;
const TREE_ROW_HEIGHT: f32 = 24.0;
const TREE_INDENT: f32 = 12.0;
const TREE_LEFT_PAD: f32 = 4.0;
const DISCLOSURE_W: f32 = 12.0;
const TREE_PANEL_BG: fn() -> gpui::Rgba = Colors::surface_panel_alt;

#[derive(Clone, Debug, Default)]
pub struct MixerTreeResizeDrag;

impl gpui::Render for MixerTreeResizeDrag {
    fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

pub type MixerTreeActionCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
pub type MixerTreeChannelCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
pub type MixerTreeToggleCb = Arc<dyn Fn(&(String, bool), &mut Window, &mut App) + 'static>;

/// Bundled tree-sidebar host passed into [`crate::components::mixer_panel::mixer_panel`].
pub struct MixerPanelTreeHost<'a> {
    pub enabled: bool,
    pub collapsed: bool,
    pub width_px: f32,
    pub model: &'a MixerTreeModel,
    pub view: &'a MixerTreeViewState,
    pub filter_input: &'a TextInputState,
    pub filter_focused: bool,
    pub filter_context_menu: TextInputContextCb,
    pub show_only_selected_group: bool,
    pub scroll: UniformListScrollHandle,
    pub callbacks: MixerTreeCallbacks,
    pub on_resize_start: Option<Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
    pub on_resize_move: Option<Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
    pub on_resize_end: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
    pub focus_channel_id: Option<&'a str>,
}

#[derive(Clone)]
pub struct MixerTreeCallbacks {
    pub on_select_channel: MixerTreeChannelCb,
    pub on_focus_channel: MixerTreeChannelCb,
    pub on_toggle_expand: MixerTreeToggleCb,
    pub on_toggle_visibility: MixerTreeChannelCb,
    pub on_toggle_pin: MixerTreeChannelCb,
    pub on_toggle_mute: MixerTreeChannelCb,
    pub on_toggle_solo: MixerTreeChannelCb,
    pub on_collapse_all: MixerTreeActionCb,
    pub on_expand_all: MixerTreeActionCb,
    pub on_show_only_selected_group: MixerTreeActionCb,
    pub on_reset_visibility: MixerTreeActionCb,
    pub on_toggle_sidebar: MixerTreeActionCb,
}

pub fn clamp_mixer_tree_sidebar_width(width: f32) -> f32 {
    width.clamp(MIXER_TREE_SIDEBAR_MIN_WIDTH, MIXER_TREE_SIDEBAR_MAX_WIDTH)
}

pub fn mixer_tree_sidebar(
    model: &MixerTreeModel,
    view: &MixerTreeViewState,
    selected_channel_id: Option<&str>,
    filter_input: &TextInputState,
    filter_focused: bool,
    on_filter_context_menu: TextInputContextCb,
    show_only_selected_group: bool,
    collapsed: bool,
    width_px: f32,
    scroll: &UniformListScrollHandle,
    callbacks: MixerTreeCallbacks,
) -> impl IntoElement {
    let rows = model.flatten(view, selected_channel_id);
    let row_count = rows.len();
    let rows_for_list = rows.clone();

    if collapsed {
        let toggle = callbacks.on_toggle_sidebar.clone();
        return div()
            .flex_shrink_0()
            .w(px(MIXER_TREE_COLLAPSED_RAIL_WIDTH))
            .h_full()
            .bg(TREE_PANEL_BG())
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .flex()
            .flex_col()
            .items_center()
            .pt(px(6.0))
            .child(
                icon_button(
                    Some(assets::ICON_CHEVRON_RIGHT_PATH),
                    "»",
                    px(22.0),
                    px(22.0),
                    px(12.0),
                    Colors::text_muted(),
                )
                .id("mixer-tree-expand-rail")
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_e, w, cx| toggle(w, cx)),
            );
    }

    let collapse_sidebar = callbacks.on_toggle_sidebar.clone();
    let collapse_all = callbacks.on_collapse_all.clone();
    let expand_all = callbacks.on_expand_all.clone();
    let show_only = callbacks.on_show_only_selected_group.clone();
    let reset_vis = callbacks.on_reset_visibility.clone();

    let filter_callbacks = TextInputCallbacks {
        on_mouse_down_out: None,
        on_context_command: None,
        on_context_menu: Some(on_filter_context_menu),
        on_mouse: None,
    };

    let toolbar = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .gap(px(2.0))
        .h(px(26.0))
        .px(px(6.0))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child("MIX"),
        )
        .child(div().flex_1())
        .child(toolbar_icon(
            assets::ICON_MINUS_PATH,
            "mixer-tree-collapse-all",
            collapse_all,
            false,
        ))
        .child(toolbar_icon(
            assets::ICON_PLUS_PATH,
            "mixer-tree-expand-all",
            expand_all,
            false,
        ))
        .child(toolbar_icon(
            assets::ICON_SLIDERS_HORIZONTAL_PATH,
            "mixer-tree-show-selected-group",
            show_only,
            show_only_selected_group,
        ))
        .child(toolbar_icon(
            assets::ICON_VOLUME_2_PATH,
            "mixer-tree-reset-visibility",
            reset_vis,
            false,
        ))
        .child(toolbar_icon(
            assets::ICON_MINUS_PATH,
            "mixer-tree-collapse-sidebar",
            collapse_sidebar,
            false,
        ));

    let search = div()
        .px(px(6.0))
        .py(px(4.0))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(text_field_with_callbacks(
            filter_input,
            filter_focused,
            filter_callbacks,
        ));

    let cbs = callbacks.clone();
    let list = uniform_list("mixer-tree-list", row_count, move |range, _window, _cx| {
        range
            .map(|i| tree_row(&rows_for_list[i], cbs.clone(), i).into_any_element())
            .collect::<Vec<_>>()
    })
    .track_scroll(scroll)
    .size_full()
    .bg(TREE_PANEL_BG());

    div()
        .flex_shrink_0()
        .w(px(clamp_mixer_tree_sidebar_width(width_px)))
        .h_full()
        .flex()
        .flex_col()
        .bg(TREE_PANEL_BG())
        .border_r(px(1.0))
        .border_color(Colors::border_subtle())
        .child(toolbar)
        .child(search)
        .child(div().flex_1().min_h_0().overflow_hidden().child(list))
}

fn toolbar_icon(
    icon_path: &'static str,
    id: &'static str,
    action: MixerTreeActionCb,
    active: bool,
) -> impl IntoElement {
    let color = if active {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    let mut btn = icon_button(Some(icon_path), "·", px(18.0), px(18.0), px(11.0), color)
        .id(id)
        .cursor(gpui::CursorStyle::PointingHand);
    if active {
        btn = btn.bg(Colors::accent_soft());
    }
    btn.on_click(move |_e, w, cx| action(w, cx))
}

fn tree_row(
    row: &MixerTreeRow,
    callbacks: MixerTreeCallbacks,
    row_index: usize,
) -> impl IntoElement {
    let node_id = row.node.id.clone();
    let channel_id = row.node.channel_id.clone();
    let depth = row.depth;
    let accent = row.node.kind.accent_color();
    let text_color = if row.selected {
        Colors::text_primary()
    } else if !row.visible_in_mixer {
        Colors::text_faint()
    } else {
        Colors::text_secondary()
    };

    let on_toggle_expand = callbacks.on_toggle_expand.clone();
    let on_select = callbacks.on_select_channel.clone();
    let on_focus = callbacks.on_focus_channel.clone();
    let on_mute = callbacks.on_toggle_mute.clone();
    let on_solo = callbacks.on_toggle_solo.clone();

    let disclosure = div()
        .w(px(DISCLOSURE_W))
        .h(px(DISCLOSURE_W))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.0))
        .text_color(Colors::text_muted())
        .child(if row.has_children {
            if row.expanded {
                "▾"
            } else {
                "▸"
            }
        } else {
            ""
        });

    let select_id = channel_id.clone();
    let toggle_id = node_id.clone();
    let toggle_expanded = row.expanded;
    let has_children = row.has_children;
    let mute_id = channel_id.clone();
    let solo_id = channel_id.clone();

    let mut row_el = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(TREE_ROW_HEIGHT))
        .pl(px(TREE_LEFT_PAD + depth as f32 * TREE_INDENT))
        .pr(px(4.0))
        .gap(px(2.0))
        .id(("mixer-tree-row", row_index))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_hover()))
        .when(row.selected, |el| {
            el.bg(Colors::accent_soft())
                .border_l(px(2.0))
                .border_color(Colors::accent_primary())
        })
        .when(!row.selected, |el| {
            el.border_l(px(2.0)).border_color(accent)
        })
        .child(disclosure)
        .when_some(row.node.track_color, |el, color| {
            el.child(
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded(px(crate::theme::radius::PILL))
                    .bg(color)
                    .flex_shrink_0(),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.0))
                .text_color(text_color)
                .child(row.node.display_name.clone()),
        )
        .children(channel_id.as_ref().map(|_| {
            div()
                .flex_shrink_0()
                .w(px(31.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(1.0))
                .child(
                    icon_button(
                        None,
                        "M",
                        px(14.0),
                        px(14.0),
                        px(9.0),
                        if row.muted {
                            Colors::accent_primary()
                        } else {
                            Colors::text_faint()
                        },
                    )
                    .text_size(px(9.0))
                    .id(("mixer-tree-mute", row_index))
                    .on_click(move |_e, w, cx| {
                        if let Some(id) = mute_id.as_ref() {
                            on_mute(id, w, cx);
                        }
                    })
                    .into_any_element(),
                )
                .child(
                    icon_button(
                        None,
                        "S",
                        px(14.0),
                        px(14.0),
                        px(9.0),
                        if row.solo {
                            Colors::accent_primary()
                        } else {
                            Colors::text_faint()
                        },
                    )
                    .text_size(px(9.0))
                    .id(("mixer-tree-solo", row_index))
                    .on_click(move |_e, w, cx| {
                        if let Some(id) = solo_id.as_ref() {
                            on_solo(id, w, cx);
                        }
                    })
                    .into_any_element(),
                )
                .into_any_element()
        }));

    row_el = row_el.on_click(move |event, w, cx| {
        if let Some(id) = select_id.as_ref() {
            if event.click_count() >= 2 {
                on_focus(id, w, cx);
            } else {
                on_select(id, w, cx);
            }
        } else if has_children {
            on_toggle_expand(&(toggle_id.clone(), !toggle_expanded), w, cx);
        }
    });

    row_el
}
