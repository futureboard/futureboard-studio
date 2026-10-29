//! GPUI entity for the mixer tree sidebar — isolated invalidation from strip scroller.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, uniform_list, App, AppContext, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window,
};

use crate::assets;
use crate::components::icon_button::icon_button;
use crate::components::mixer_tree_cache::{MixerTreeRenderCache, MixerTreeVisibleRow};
use crate::components::mixer_tree_sidebar::{
    clamp_mixer_tree_sidebar_width, MixerTreeCallbacks, MixerTreeResizeDrag,
    MIXER_TREE_COLLAPSED_RAIL_WIDTH,
};
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks, TextInputCallbacks, TextInputContextCb,
    TextInputState,
};
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::TimelineState;
use crate::layout::StudioLayout;
use crate::theme::Colors;

const TREE_ROW_HEIGHT: f32 = 24.0;
const TREE_INDENT: f32 = 12.0;
const TREE_LEFT_PAD: f32 = 4.0;
const DISCLOSURE_W: f32 = 12.0;
const TREE_PANEL_BG: fn() -> gpui::Rgba = Colors::surface_panel_alt;

/// Where the tree reads its channels from.
enum TreeSource {
    /// The session's timeline.
    Timeline(Entity<Timeline>),
    /// A fixed session, for the offscreen UI preview.
    #[cfg(feature = "ui-preview")]
    Fixed(Box<TimelineState>),
}

pub struct MixerTreeSidebar {
    source: TreeSource,
    cache: MixerTreeRenderCache,
    collapsed: bool,
    width_px: f32,
    show_only_selected_group: bool,
    filter_input: TextInputState,
    filter_focused: bool,
    filter_context_menu: TextInputContextCb,
    scroll: UniformListScrollHandle,
    callbacks: Option<MixerTreeCallbacks>,
    on_resize_start: Option<Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
    on_resize_move: Option<Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
    on_resize_end: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
    last_filter_applied: String,
}

impl MixerTreeSidebar {
    pub fn new(
        _owner: Entity<StudioLayout>,
        timeline: Entity<Timeline>,
        focus_handle: FocusHandle,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self::with_source(TreeSource::Timeline(timeline), focus_handle)
    }

    /// The tree over a fixed session, for the offscreen UI preview.
    #[cfg(feature = "ui-preview")]
    pub(crate) fn for_preview(
        state: TimelineState,
        callbacks: MixerTreeCallbacks,
        focus_handle: FocusHandle,
    ) -> Self {
        let mut sidebar = Self::with_source(TreeSource::Fixed(Box::new(state)), focus_handle);
        sidebar.callbacks = Some(callbacks);
        if let TreeSource::Fixed(state) = &sidebar.source {
            // The first sync the layout makes for a session: a stereo output.
            sidebar.cache.sync_routing_key(
                1,
                state.track_names_revision,
                2,
                "",
                false,
                state.selection.selected_track_id.as_deref(),
            );
            sidebar.cache.recompute(&state.tracks, &state.mixer_tree);
        }
        sidebar
    }

    /// Runs `f` on the session the tree shows.
    fn with_state<R>(&self, cx: &App, f: impl FnOnce(&TimelineState) -> R) -> R {
        match &self.source {
            TreeSource::Timeline(timeline) => f(&timeline.read(cx).state),
            #[cfg(feature = "ui-preview")]
            TreeSource::Fixed(state) => f(state),
        }
    }

    /// Recomputes the cached rows from the session.
    fn recompute(&mut self, cx: &App) {
        let mut cache = std::mem::take(&mut self.cache);
        self.with_state(cx, |state| {
            cache.recompute(&state.tracks, &state.mixer_tree);
        });
        self.cache = cache;
    }

    fn with_source(source: TreeSource, focus_handle: FocusHandle) -> Self {
        Self {
            source,
            cache: MixerTreeRenderCache::default(),
            collapsed: false,
            width_px: crate::components::mixer_tree_sidebar::MIXER_TREE_SIDEBAR_DEFAULT_WIDTH,
            show_only_selected_group: false,
            filter_input: TextInputState::new("mixer-tree-filter-entity", focus_handle)
                .with_placeholder("Filter channels…"),
            filter_focused: false,
            filter_context_menu: Arc::new(|_, _, _| {}),
            scroll: UniformListScrollHandle::new(),
            callbacks: None,
            on_resize_start: None,
            on_resize_move: None,
            on_resize_end: None,
            last_filter_applied: String::new(),
        }
    }

    pub fn is_filter_focused(&self, window: &Window) -> bool {
        self.filter_input.is_focused(window)
    }

    pub fn set_session_hooks(
        &mut self,
        callbacks: MixerTreeCallbacks,
        on_resize_start: Option<Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
        on_resize_move: Option<Arc<dyn Fn(f32, &mut Window, &mut App) + 'static>>,
        on_resize_end: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
    ) {
        self.callbacks = Some(callbacks);
        self.on_resize_start = on_resize_start;
        self.on_resize_move = on_resize_move;
        self.on_resize_end = on_resize_end;
    }

    pub fn sync_chrome(&mut self, collapsed: bool, width_px: f32, show_only_selected_group: bool) {
        self.collapsed = collapsed;
        self.width_px = width_px;
        if self.show_only_selected_group != show_only_selected_group {
            self.show_only_selected_group = show_only_selected_group;
            self.cache.mark_routing_dirty();
        }
    }

    pub fn mark_expansion_dirty(&mut self) {
        self.cache.mark_expansion_dirty();
    }

    pub fn mark_selection_dirty(&mut self) {
        self.cache.mark_selection_dirty();
    }

    pub fn mark_routing_dirty(&mut self) {
        self.cache.mark_routing_dirty();
    }

    pub fn recompute_expansion(&mut self, cx: &Context<Self>) {
        self.cache.mark_expansion_dirty();
        self.recompute(cx);
    }

    pub fn recompute_selection(&mut self, cx: &Context<Self>) {
        self.cache.mark_selection_dirty();
        self.recompute(cx);
    }

    pub fn sync_routing_from_layout(
        &mut self,
        cx: &Context<Self>,
        routing_gen: u64,
        output_channels: u32,
    ) -> bool {
        let filter = self.filter_input.value.trim().to_string();
        let (selected, names_revision) = self.with_state(cx, |state| {
            (
                state.selection.selected_track_id.clone(),
                state.track_names_revision,
            )
        });
        self.cache.sync_routing_key(
            routing_gen,
            names_revision,
            output_channels,
            &filter,
            self.show_only_selected_group,
            selected.as_deref(),
        );
        if !self.cache.dirty.any() {
            return false;
        }
        self.recompute(cx);
        true
    }

    fn apply_filter_debounce(&mut self, cx: &Context<Self>) {
        let current = self.filter_input.value.clone();
        if current == self.last_filter_applied {
            return;
        }
        self.last_filter_applied = current.clone();
        self.cache.filter = current;
        self.cache.mark_filter_dirty();
        self.recompute(cx);
    }
}

impl Render for MixerTreeSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("MixerTreeSidebar");
        crate::perf::count("mixer_tree_layout_count", 1);
        crate::perf::count("mixer_tree_paint_count", 1);

        self.filter_focused = self.filter_input.is_focused(window);
        self.apply_filter_debounce(cx);

        let callbacks = match self.callbacks.clone() {
            Some(c) => c,
            None => {
                return div().into_any_element();
            }
        };

        if self.collapsed {
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
                    .on_click(move |_e, w, app| toggle(w, app)),
                )
                .into_any_element();
        }

        let rows = self.cache.visible_rows.clone();
        let row_count = rows.len();
        let hovered = self.cache.hovered_row;
        let scroll = self.scroll.clone();

        let collapse_sidebar = callbacks.on_toggle_sidebar.clone();
        let collapse_all = callbacks.on_collapse_all.clone();
        let expand_all = callbacks.on_expand_all.clone();
        let show_only = callbacks.on_show_only_selected_group.clone();
        let reset_vis = callbacks.on_reset_visibility.clone();

        let filter_mouse_callbacks =
            bind_mouse_selection(cx.entity().clone(), |this| &mut this.filter_input);
        let filter_callbacks = TextInputCallbacks {
            on_mouse_down_out: None,
            on_context_command: None,
            on_context_menu: Some(self.filter_context_menu.clone()),
            on_mouse: filter_mouse_callbacks.on_mouse,
        };

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
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
                    .child(
                        crate::i18n::I18n::from_app(cx).tr_or("mixer.tree.channels", "CHANNELS"),
                    ),
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
                self.show_only_selected_group,
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
                &self.filter_input,
                self.filter_focused,
                filter_callbacks,
            ));

        let cbs = callbacks.clone();
        let sidebar_entity = cx.entity().clone();
        let list = uniform_list("mixer-tree-list", row_count, move |range, _window, _cx| {
            range
                .map(|i| {
                    tree_row_element(
                        &rows[i],
                        cbs.clone(),
                        i,
                        hovered == Some(i),
                        sidebar_entity.clone(),
                    )
                    .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&scroll)
        .size_full()
        .bg(TREE_PANEL_BG());

        let mut panel = div()
            .flex_shrink_0()
            .w(px(clamp_mixer_tree_sidebar_width(self.width_px)))
            .h_full()
            .flex()
            .flex_col()
            .bg(TREE_PANEL_BG())
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .child(toolbar)
            .child(search)
            .child(div().flex_1().min_h_0().overflow_hidden().child(list));

        if let (Some(on_start), Some(on_move), Some(on_end)) = (
            self.on_resize_start.clone(),
            self.on_resize_move.clone(),
            self.on_resize_end.clone(),
        ) {
            let on_move_cb = on_move.clone();
            let on_end_cb = on_end.clone();
            panel = panel
                .child(
                    div()
                        .absolute()
                        .right(px(0.0))
                        .top_0()
                        .bottom_0()
                        .w(px(4.0))
                        .id("mixer-tree-resize-handle")
                        .cursor(gpui::CursorStyle::ResizeLeftRight)
                        .hover(|s| s.bg(Colors::accent_soft()))
                        .on_mouse_down(gpui::MouseButton::Left, move |e, w, app| {
                            let x: f32 = e.position.x.into();
                            on_start(x, w, app);
                        })
                        .on_drag(MixerTreeResizeDrag, |_drag, _offset, _window, app| {
                            app.new(|_| MixerTreeResizeDrag)
                        }),
                )
                .on_drag_move::<MixerTreeResizeDrag>(move |event, w, app| {
                    let x: f32 = event.event.position.x.into();
                    on_move_cb(x, w, app);
                })
                .on_mouse_up(gpui::MouseButton::Left, move |_e, w, app| {
                    on_end_cb(w, app);
                });
        }

        panel.into_any_element()
    }
}

fn toolbar_icon(
    icon_path: &'static str,
    id: &'static str,
    action: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
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
    btn.on_click(move |_e, w, app| action(w, app))
}

fn tree_row_element(
    row: &MixerTreeVisibleRow,
    callbacks: MixerTreeCallbacks,
    row_index: usize,
    hovered: bool,
    sidebar_entity: Entity<MixerTreeSidebar>,
) -> impl IntoElement {
    let node_id = row.node_id.as_str();
    let channel_id = row.channel_id.as_deref();
    let depth = row.depth as usize;
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
    let on_visibility = callbacks.on_toggle_visibility.clone();

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

    let toggle_id = node_id.to_string();
    let toggle_expanded = row.expanded;
    let has_children = row.has_children;
    let select_id = channel_id.map(str::to_string);
    let mute_id = channel_id.map(str::to_string);
    let solo_id = channel_id.map(str::to_string);
    let visibility_id = channel_id.map(str::to_string);
    let label = row.label.clone();
    let is_group = channel_id.is_none();
    let rest = TREE_PANEL_BG();
    let selected_bg = Colors::composite(rest, Colors::state_selected());

    let mut row_el = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(TREE_ROW_HEIGHT))
        .pl(px(TREE_LEFT_PAD + depth as f32 * TREE_INDENT))
        .pr(px(4.0))
        .gap(px(4.0))
        .id(("mixer-tree-row", row_index))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_mouse_move({
            let entity = sidebar_entity.clone();
            move |_event, _window, cx| {
                let _ = entity.update(cx, |sidebar, cx| {
                    if sidebar.cache.set_hovered_row(Some(row_index)) {
                        sidebar.cache.clear_hover_dirty();
                        cx.notify();
                    }
                });
            }
        })
        .when(hovered && !row.selected, |el| {
            el.bg(Colors::composite(rest, Colors::state_hover()))
        })
        // Selected: the lifted row and the accent edge, as everywhere else.
        .border_l(px(2.0))
        .border_color(if row.selected {
            Colors::accent_primary()
        } else {
            gpui::transparent_black().into()
        })
        .when(row.selected, |el| el.bg(selected_bg))
        .child(disclosure)
        // Shown in the mixer, or hidden from it: the strip's own switch.
        .children(visibility_id.clone().map(|id| {
            let visible = row.visible_in_mixer;
            div()
                .id(("mixer-tree-visibility", row_index))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(18.0))
                .rounded(px(crate::theme::radius::MICRO))
                .hover(|s| s.bg(Colors::state_hover()))
                .tooltip(crate::components::controls::fb_tooltip(if visible {
                    "Hide this channel's strip"
                } else {
                    "Show this channel's strip"
                }))
                .child(
                    gpui::svg()
                        .path(if visible {
                            assets::ICON_EYE_PATH
                        } else {
                            assets::ICON_EYE_OFF_PATH
                        })
                        .size(px(12.0))
                        .text_color(if visible {
                            Colors::text_secondary()
                        } else {
                            Colors::text_faint()
                        }),
                )
                .on_click(move |_e, w, app| {
                    app.stop_propagation();
                    on_visibility(&id, w, app);
                })
        }))
        .when_some(row.track_color, |el, color| {
            el.child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded(px(crate::theme::radius::MICRO))
                    .bg(color)
                    .flex_shrink_0(),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(if is_group { 11.5 } else { 12.0 }))
                .when(is_group, |el| el.font_weight(gpui::FontWeight::SEMIBOLD))
                .text_color(text_color)
                .child(label),
        )
        .children(channel_id.is_some().then(|| {
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .child(tree_toggle(
                    ("mixer-tree-mute", row_index).into(),
                    "M",
                    if row.muted {
                        TreeToggle::On
                    } else {
                        TreeToggle::Off
                    },
                    Colors::state_mute(),
                    {
                        let mute_id = mute_id.clone();
                        move |w, app| {
                            if let Some(id) = mute_id.as_ref() {
                                on_mute(id, w, app);
                            }
                        }
                    },
                ))
                .child(tree_toggle(
                    ("mixer-tree-solo", row_index).into(),
                    "S",
                    // Three states, matching the strip's S button: engaged,
                    // sounded by a soloed parent instrument, and off.
                    if row.solo {
                        TreeToggle::On
                    } else if row.solo_implied {
                        TreeToggle::Implied
                    } else {
                        TreeToggle::Off
                    },
                    Colors::state_solo(),
                    {
                        let solo_id = solo_id.clone();
                        move |w, app| {
                            if let Some(id) = solo_id.as_ref() {
                                on_solo(id, w, app);
                            }
                        }
                    },
                ))
                .into_any_element()
        }));

    row_el = row_el.on_click({
        let select_id = select_id.clone();
        move |event, w, app| {
            if let Some(id) = select_id.as_ref() {
                if event.click_count() >= 2 {
                    on_focus(id, w, app);
                } else {
                    on_select(id, w, app);
                }
            } else if has_children {
                on_toggle_expand(&(toggle_id.clone(), !toggle_expanded), w, app);
            }
        }
    });

    row_el
}

/// A tree row's M or S state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeToggle {
    Off,
    /// Engaged because a parent decided for it: a VSTi output channel under
    /// its soloed instrument.
    Implied,
    On,
}

/// A tree row's M / S button, in the strips' latched colours: solid when on,
/// washed when implied, outlined when off.
fn tree_toggle(
    id: gpui::ElementId,
    label: &'static str,
    state: TreeToggle,
    semantic: gpui::Rgba,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (bg, border, text) = match state {
        TreeToggle::On => (semantic, semantic, Colors::on_color(semantic)),
        TreeToggle::Implied => {
            let (fill, border) = Colors::latched(TREE_PANEL_BG(), semantic);
            (fill, border, semantic)
        }
        TreeToggle::Off => (
            gpui::transparent_black().into(),
            Colors::border_default(),
            Colors::text_muted(),
        ),
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .rounded(px(crate::theme::radius::MICRO))
        .bg(bg)
        .border(px(1.0))
        .border_color(border)
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.border_color(Colors::border_strong()))
        .child(label)
        .on_click(move |_e, w, app| on_click(w, app))
}
