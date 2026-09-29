use crate::assets;
use crate::components::timeline::timeline_state::TimelineTool;
use crate::theme::Colors;
use gpui::{
    div, px, svg, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled,
};

pub fn floating_tools_bar(
    active_tool: TimelineTool,
    on_select_tool: std::sync::Arc<
        dyn Fn(&TimelineTool, &mut gpui::Window, &mut gpui::App) + 'static,
    >,
    on_drag_start: std::sync::Arc<dyn Fn(&(f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>,
    waveform_zoom: f32,
    on_waveform_zoom: std::sync::Arc<dyn Fn(&i32, &mut gpui::Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    let tools = [
        (
            TimelineTool::Pointer,
            "Select [1]",
            assets::ICON_MOUSE_POINTER_PATH,
            true,
        ),
        (
            TimelineTool::Pen,
            "Draw [2]",
            assets::ICON_PENCIL_PATH,
            false,
        ),
        (
            TimelineTool::Cut,
            "Cut [3]",
            assets::ICON_SCISSORS_PATH,
            false,
        ),
        (TimelineTool::Glue, "Glue [4]", assets::ICON_LINK_PATH, true),
        (
            TimelineTool::Mute,
            "Mute [5]",
            assets::ICON_VOLUME_X_PATH,
            false,
        ),
        (
            TimelineTool::Time,
            "Stretch [6]",
            assets::ICON_CLOCK_PATH,
            false,
        ),
        (
            TimelineTool::Automation,
            "Automation [7]",
            assets::ICON_AUTOMATION_PATH,
            false,
        ),
    ];

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(1.0))
        .rounded(px(crate::theme::radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_default())
        .bg(Colors::surface_panel_alt())
        .px(px(4.0))
        .py(px(4.0))
        .shadow_xl()
        .child({
            let on_drag_start = on_drag_start.clone();
            div()
                .id("timeline-tools-drag-grip")
                .flex()
                .items_center()
                .justify_center()
                .w(px(16.0))
                .h(px(28.0))
                .cursor(gpui::CursorStyle::PointingHand)
                .text_color(Colors::text_faint())
                .hover(|style| style.text_color(Colors::text_secondary()))
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    let point: (f32, f32) = (event.position.x.into(), event.position.y.into());
                    on_drag_start(&point, window, cx);
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .w(px(3.0))
                        .h(px(14.0))
                        .border_l(px(1.0))
                        .border_r(px(1.0))
                        .border_color(Colors::text_faint()),
                )
        })
        .children(
            tools
                .into_iter()
                .enumerate()
                .map(move |(i, (tool, label, icon, separator))| {
                    let active = active_tool == tool;
                    let on_select = on_select_tool.clone();

                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .relative()
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(28.0))
                                .h(px(28.0))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .id(label)
                                .cursor(gpui::CursorStyle::PointingHand)
                                .text_size(px(14.0))
                                .text_color(if active {
                                    Colors::accent_primary()
                                } else {
                                    Colors::text_muted()
                                })
                                .bg(if active {
                                    Colors::accent_soft()
                                } else {
                                    gpui::transparent_black().into()
                                })
                                .hover(|style| {
                                    if !active {
                                        style.bg(Colors::surface_hover())
                                    } else {
                                        style
                                    }
                                })
                                .on_click(move |_, window, cx| {
                                    on_select(&tool, window, cx);
                                })
                                .child(svg().path(icon).w(px(14.0)).h(px(14.0)).text_color(
                                    if active {
                                        Colors::accent_primary()
                                    } else {
                                        Colors::text_muted()
                                    },
                                ))
                                .children(if active {
                                    Some(
                                        div()
                                            .absolute()
                                            .bottom(px(1.0))
                                            .h(px(1.5))
                                            .w(px(16.0))
                                            .rounded(px(crate::theme::radius::PILL))
                                            .bg(Colors::accent_primary()),
                                    )
                                } else {
                                    None
                                }),
                        )
                        .children(if separator && i < 6 {
                            Some(
                                div()
                                    .mx(px(4.0))
                                    .h(px(16.0))
                                    .w(px(1.0))
                                    .bg(Colors::divider()),
                            )
                        } else {
                            None
                        })
                }),
        )
        .child(
            div()
                .mx(px(4.0))
                .h(px(16.0))
                .w(px(1.0))
                .bg(Colors::divider()),
        )
        .child(waveform_zoom_control(waveform_zoom, on_waveform_zoom))
}

/// Waveform zoom: taller waveforms for reading quiet audio, without touching
/// clip gain. − and + step it; the readout goes back to 1×.
fn waveform_zoom_control(
    zoom: f32,
    on_step: std::sync::Arc<dyn Fn(&i32, &mut gpui::Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    let zoomed = zoom > 1.0 + 1.0e-3;
    let step_button = |id: &'static str, icon: &'static str, tip: &'static str, steps: i32| {
        let on_step = on_step.clone();
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(20.0))
            .h(px(28.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.bg(Colors::surface_hover()))
            .tooltip(crate::components::fb_tooltip(tip))
            .on_click(move |_, window, cx| on_step(&steps, window, cx))
            .child(
                svg()
                    .path(icon)
                    .w(px(12.0))
                    .h(px(12.0))
                    .text_color(Colors::text_muted()),
            )
    };
    let reset = on_step.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .child(step_button(
            "waveform-zoom-out",
            assets::ICON_MINUS_PATH,
            "Waveform zoom out",
            -1,
        ))
        .child(
            div()
                .id("waveform-zoom-reset")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .h(px(28.0))
                .px(px(4.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(Colors::surface_hover()))
                .tooltip(crate::components::fb_tooltip(
                    "Waveform zoom — view only, clip gain is unchanged. Click for 1×",
                ))
                .on_click(move |_, window, cx| reset(&0, window, cx))
                .child(
                    svg()
                        .path(assets::ICON_AUDIO_LINES_PATH)
                        .w(px(14.0))
                        .h(px(14.0))
                        .text_color(if zoomed {
                            Colors::text_primary()
                        } else {
                            Colors::text_muted()
                        }),
                )
                .child(
                    div()
                        .min_w(px(22.0))
                        .text_size(px(crate::theme::typography::UI_XS))
                        .text_color(if zoomed {
                            Colors::text_primary()
                        } else {
                            Colors::text_muted()
                        })
                        .child(format!("{:.1}×", zoom)),
                ),
        )
        .child(step_button(
            "waveform-zoom-in",
            assets::ICON_PLUS_PATH,
            "Waveform zoom in",
            1,
        ))
}
