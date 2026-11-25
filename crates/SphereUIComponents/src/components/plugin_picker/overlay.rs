//! Insert picker hosts.
//!
//! One body ([`super::body`]), two hosts: the Add Insert window
//! (`plugin_picker_panel`) and the in-window fallback (`plugin_picker_overlay`).

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{div, px, svg, App, InteractiveElement, IntoElement, ParentElement, Styled, Window};

use crate::assets;
use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::plugin_picker::body::{picker_body, ROW_H};
use crate::components::plugin_picker::filter::compute_filter_result;
use crate::components::plugin_picker::prefs::PluginPickerPrefs;
use crate::components::plugin_picker::search_index::PluginSearchIndex;
use crate::components::plugin_picker::state::{
    CatalogStatus, PickerFilter, PluginPickerScrollHandles, PluginPickerState,
};
use crate::components::plugin_picker::PluginPickerCallbacks;
use crate::components::text_input::{TextInputCallbacks, TextInputState};
use crate::theme::{radius, space, typography, Colors};

type VoidCb = Arc<dyn Fn(&(), &mut Window, &mut App) + 'static>;

/// The picker drawn over the studio window, for hosts without the Add Insert
/// window. Same body; this only adds the modal frame and the click-away.
#[allow(clippy::too_many_arguments)]
pub fn plugin_picker_overlay(
    state: &PluginPickerState,
    index: Option<Arc<PluginSearchIndex>>,
    prefs: &PluginPickerPrefs,
    catalog_status: CatalogStatus,
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    callbacks: PluginPickerCallbacks,
    au_scan_error: Option<&str>,
    scroll: &PluginPickerScrollHandles,
) -> impl IntoElement {
    let close_backdrop = callbacks.on_close.clone();
    let modal_width = prefs.window_width.max(760.0);
    let modal_height = prefs.window_height.max(480.0);
    let body = picker_body(
        state,
        index,
        prefs,
        catalog_status,
        search_input,
        search_focused,
        search_callbacks,
        callbacks,
        au_scan_error,
        scroll,
    );

    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .flex()
        .items_start()
        .justify_center()
        .pt(px(space::PAGE + space::SECTION))
        .px(px(space::SECTION))
        .pb(px(space::BLOCK))
        .id("plugin-picker-overlay")
        .bg(Colors::state_scrim())
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            close_backdrop(&(), window, cx);
        })
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(modal_width))
                .h(px(modal_height))
                .overflow_hidden()
                .rounded(px(radius::DIALOG))
                .border(px(1.0))
                .border_color(Colors::border_normal())
                .bg(Colors::surface_window())
                .shadow_xl()
                .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                    cx.stop_propagation();
                })
                .child(body),
        )
}

/// The picker as the whole content of the Add Insert window.
#[allow(clippy::too_many_arguments)]
pub fn plugin_picker_panel(
    state: &PluginPickerState,
    index: Option<Arc<PluginSearchIndex>>,
    prefs: &PluginPickerPrefs,
    catalog_status: CatalogStatus,
    search_input: &TextInputState,
    search_focused: bool,
    search_callbacks: TextInputCallbacks,
    callbacks: PluginPickerCallbacks,
    au_scan_error: Option<&str>,
    scroll: &PluginPickerScrollHandles,
) -> impl IntoElement {
    let on_drop_plugin = callbacks.on_drop_plugin.clone();
    div()
        .flex()
        .flex_col()
        .size_full()
        .overflow_hidden()
        .bg(Colors::surface_window())
        .on_drop::<crate::components::plugin_picker::PluginDragItem>(move |item, window, cx| {
            on_drop_plugin(item, window, cx)
        })
        .child(picker_body(
            state,
            index,
            prefs,
            catalog_status,
            search_input,
            search_focused,
            search_callbacks,
            callbacks,
            au_scan_error,
            scroll,
        ))
}

pub(super) fn notice_banner(message: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .flex_shrink_0()
        .px(px(space::LOOSE))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::divider())
        .bg(Colors::with_alpha(Colors::status_warning(), 0.08))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::status_warning())
        .child(message)
}

pub(super) fn empty_state_body(
    catalog_status: CatalogStatus,
    total: usize,
    state: &PluginPickerState,
    au_scan_error: Option<&str>,
    callbacks: &PluginPickerCallbacks,
) -> impl IntoElement {
    let (title, hint) = match &catalog_status {
        CatalogStatus::Loading => ("Loading plug-ins…".to_string(), None),
        CatalogStatus::MissingDatabase => (
            "No plug-in library yet".to_string(),
            Some("Open the Plugin Manager and run a scan.".to_string()),
        ),
        CatalogStatus::Error(err) => (
            "The plug-in library could not be read".to_string(),
            Some(err.clone()),
        ),
        CatalogStatus::Ready if total == 0 => (
            "No plug-ins found".to_string(),
            Some("Scan for plug-ins in the Plugin Manager.".to_string()),
        ),
        CatalogStatus::Ready if !state.query.trim().is_empty() => (
            format!("Nothing matches “{}”", state.query.trim()),
            Some("Try a shorter name, a vendor, or another kind tab.".to_string()),
        ),
        CatalogStatus::Ready if state.filters.sidebar == PickerFilter::Favorites => (
            "No favorites here yet".to_string(),
            Some("Star a plug-in to keep it one click away.".to_string()),
        ),
        CatalogStatus::Ready if state.filters.sidebar == PickerFilter::RecentlyUsed => (
            "Nothing used recently".to_string(),
            Some("Plug-ins you insert will show up here.".to_string()),
        ),
        CatalogStatus::Ready if au_scan_error.is_some() => (
            "Audio Units unavailable".to_string(),
            Some("VST3 and CLAP plug-ins are still available.".to_string()),
        ),
        CatalogStatus::Ready => (
            "Nothing in this view".to_string(),
            Some("Pick another entry on the left or another kind tab.".to_string()),
        ),
    };
    let offer_manager = matches!(
        catalog_status,
        CatalogStatus::MissingDatabase | CatalogStatus::Error(_)
    ) || (matches!(catalog_status, CatalogStatus::Ready) && total == 0);

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(space::SNUG))
        .size_full()
        .px(px(space::BLOCK))
        .child(
            svg()
                .path(assets::ICON_SEARCH_PATH)
                .w(px(20.0))
                .h(px(20.0))
                .text_color(Colors::text_faint()),
        )
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_secondary())
                .child(title),
        )
        .when_some(hint, |el, h| {
            el.child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child(h),
            )
        })
        .when(offer_manager, |el| {
            el.child(recovery_actions(
                matches!(catalog_status, CatalogStatus::Error(_)),
                callbacks.on_retry_load.clone(),
                callbacks.on_open_plugin_manager.clone(),
                callbacks.on_rebuild_database.clone(),
            ))
        })
}

fn recovery_actions(
    failed: bool,
    on_retry: VoidCb,
    on_open_manager: VoidCb,
    on_rebuild: VoidCb,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap(px(space::BASE))
        .pt(px(space::LOOSE))
        .child(fb_button(
            "plugin-picker-open-mgr",
            "Open Plugin Manager",
            FbButtonKind::Primary,
            true,
            move |_, window, cx| on_open_manager(&(), window, cx),
        ))
        .when(failed, |row| {
            row.child(fb_button(
                "plugin-picker-retry",
                "Retry",
                FbButtonKind::Default,
                true,
                move |_, window, cx| on_retry(&(), window, cx),
            ))
            .child(fb_button(
                "plugin-picker-rebuild",
                "Rebuild Library",
                FbButtonKind::Default,
                true,
                move |_, window, cx| on_rebuild(&(), window, cx),
            ))
        })
}

pub fn page_size_for_height(height: f32) -> usize {
    (height / ROW_H).max(1.0) as usize
}

pub fn visible_plugin_id_at(
    state: &PluginPickerState,
    index: &PluginSearchIndex,
    prefs: &PluginPickerPrefs,
) -> Option<String> {
    let result = compute_filter_result(index, &state.query, &state.filters, prefs, false);
    let plugin_index = result.indices.get(state.highlighted_index)?;
    index.plugin_at(*plugin_index).map(|p| p.id.clone())
}

/// The picker panel the Add Insert window and the overlay both draw, with more
/// plug-ins and vendors than its list and rail hold: dragging either bar's
/// thumb scrolls it.
#[cfg(test)]
mod scrollbar_tests {
    use super::*;
    use crate::components::timeline::timeline_state::TrackType;
    use gpui::{
        Context, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
        Point, Render, ScrollHandle, TestAppContext, VisualTestContext, WindowHandle,
    };
    use std::path::PathBuf;
    use SpherePluginHost::{
        PluginFormat, PluginKind, PluginScanStatus, PluginStatus, RegistryPlugin,
    };

    struct Rig {
        state: PluginPickerState,
        index: Arc<PluginSearchIndex>,
        prefs: PluginPickerPrefs,
        search: TextInputState,
        scroll: PluginPickerScrollHandles,
    }

    fn noop<T: ?Sized + 'static>() -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
        Arc::new(|_, _, _| {})
    }

    impl Render for Rig {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let callbacks = PluginPickerCallbacks {
                on_close: noop(),
                on_select: noop(),
                on_pick: noop(),
                on_select_filter: noop(),
                on_select_kind: noop(),
                on_select_facet: noop(),
                on_toggle_favorite: noop(),
                on_retry_load: noop(),
                on_open_plugin_manager: noop(),
                on_rebuild_database: noop(),
                on_drop_plugin: noop(),
            };
            div().size_full().child(plugin_picker_panel(
                &self.state,
                Some(self.index.clone()),
                &self.prefs,
                CatalogStatus::Ready,
                &self.search,
                false,
                TextInputCallbacks::default(),
                callbacks,
                None,
                &self.scroll,
            ))
        }
    }

    fn plugin(i: usize) -> RegistryPlugin {
        let id = format!("plug-{i:03}");
        RegistryPlugin {
            id: id.clone(),
            name: id.clone(),
            vendor: format!("Vendor {:02}", i % 60),
            format: PluginFormat::Vst3,
            category: "EQ".to_string(),
            is_ara: false,
            midi_output: false,
            raw_category: None,
            sub_categories: None,
            kind: PluginKind::Effect,
            path: PathBuf::from(format!("C:/Plugins/{id}.vst3")),
            class_id: None,
            version: None,
            sdk_metadata_loaded: true,
            preset_path: PathBuf::from(format!("C:/Cache/{id}.pst")),
            scanned_at_ms: 0,
            status: PluginStatus::PresetReady,
            scan_status: PluginScanStatus::Success,
            error_message: None,
        }
    }

    fn open(cx: &mut TestAppContext) -> (WindowHandle<Rig>, VisualTestContext) {
        let window = cx.open_window(gpui::size(px(960.0), px(680.0)), |_, cx| {
            let state = PluginPickerState::open_for("t1", "Track 1", TrackType::Audio, 0, true);
            Rig {
                state,
                index: Arc::new(PluginSearchIndex::from_plugins(
                    (0..200).map(plugin).collect(),
                )),
                prefs: PluginPickerPrefs::default(),
                search: TextInputState::new("picker-search", cx.focus_handle()),
                scroll: PluginPickerScrollHandles::default(),
            }
        });
        cx.run_until_parked();
        // The bars size themselves from the last layout: draw once more.
        window.update(cx, |_, _, cx| cx.notify()).expect("window");
        cx.run_until_parked();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (window, vcx)
    }

    fn drag(vcx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        vcx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        for step in 1..=8 {
            let position = from + (to - from) * (step as f32 / 8.0);
            vcx.simulate_event(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            });
        }
        vcx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: to,
            modifiers: Modifiers::default(),
            click_count: 1,
        });
        vcx.run_until_parked();
    }

    /// Drags the thumb of the bar over `handle`'s region down 80 px and
    /// returns how far the region scrolled.
    fn drag_bar(
        window: WindowHandle<Rig>,
        vcx: &mut VisualTestContext,
        handle: impl Fn(&Rig) -> ScrollHandle,
    ) -> f32 {
        let (bounds, max) = window
            .update(vcx, |rig, _, _| {
                let h = handle(rig);
                (h.bounds(), f32::from(h.max_offset().y))
            })
            .expect("window");
        assert!(max > 0.5, "the region must overflow for this test");
        let x = f32::from(bounds.right()) - crate::components::scrollbar::THICKNESS * 0.5;
        let y = f32::from(bounds.top()) + 6.0;
        drag(
            vcx,
            gpui::point(px(x), px(y)),
            gpui::point(px(x), px(y + 80.0)),
        );
        window
            .update(vcx, |rig, _, _| -f32::from(handle(rig).offset().y))
            .expect("window")
    }

    #[gpui::test]
    fn the_plugin_list_thumb_drags(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        let scrolled = drag_bar(window, &mut vcx, |rig| {
            rig.scroll.list.0.borrow().base_handle.clone()
        });
        assert!(scrolled > 80.0, "the list scrolled {scrolled} px");
    }

    #[gpui::test]
    fn the_filter_rail_thumb_drags(cx: &mut TestAppContext) {
        let (window, mut vcx) = open(cx);
        let scrolled = drag_bar(window, &mut vcx, |rig| rig.scroll.sidebar.clone());
        assert!(scrolled > 80.0, "the rail scrolled {scrolled} px");
    }
}
