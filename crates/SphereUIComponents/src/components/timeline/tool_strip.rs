//! The arrangement's edit tools as a docked strip above the ruler — the
//! Classic tool bar.
//!
//! Region contract — owner: the studio shell, which stacks this strip on top
//! of the arrangement (directly above its ruler) when Appearance → Tool Bar is
//! Classic. State owner: the [`Timeline`] (active tool, waveform zoom); this
//! view only reads it and calls the same [`Timeline::select_tool`] /
//! [`Timeline::step_waveform_zoom`] the floating bar calls. Coordinate space:
//! its own; it sits outside the arrangement, whose pointer maths reads the
//! measured lane and ruler origins and so follows the arrangement down by the
//! strip's height on its own. Size source: [`CLASSIC_TOOL_STRIP_HEIGHT`],
//! fixed; it takes the arrangement's width. Scroll owner: none — it clips if
//! the arrangement is narrower than its controls.
//!
//! A cached view of its own, observing the timeline but repainting only when
//! the tool or the zoom actually changes, so an edit in the arrangement does
//! not cost a strip rebuild.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

use super::floating_tools_bar::TIMELINE_TOOLS;
use super::timeline_state::TimelineTool;
use super::Timeline;
use crate::assets;
use crate::components::controls::fb_tooltip;
use crate::theme::{radius, size, space, typography, Colors};

/// Height of the docked strip: a comfortable button with a tight inset above
/// and below, the lower one holding the selection marker.
pub const CLASSIC_TOOL_STRIP_HEIGHT: f32 = size::COMFORTABLE + 2.0 * space::TIGHT;

/// Width of a waveform-zoom step button: narrower than a tool, because it is
/// a stepper rather than a mode.
const STEP_WIDTH: f32 = size::DENSE;

pub struct ClassicToolStrip {
    timeline: Entity<Timeline>,
    /// What the strip last drew: the active tool and the waveform zoom.
    shown: (TimelineTool, f32),
    _observer: Subscription,
}

impl ClassicToolStrip {
    pub fn new(timeline: &Entity<Timeline>, cx: &mut Context<Self>) -> Self {
        let shown = {
            let timeline = timeline.read(cx);
            (timeline.active_tool(), timeline.waveform_zoom())
        };
        let _observer = cx.observe(timeline, |this, timeline, cx| {
            let timeline = timeline.read(cx);
            let next = (timeline.active_tool(), timeline.waveform_zoom());
            if next != this.shown {
                this.shown = next;
                cx.notify();
            }
        });
        Self {
            timeline: timeline.clone(),
            shown,
            _observer,
        }
    }

    /// The strip's root style, for the shell's `.cached(..)`.
    pub fn cached_style() -> gpui::StyleRefinement {
        gpui::StyleRefinement::default()
            .flex()
            .flex_row()
            .flex_none()
            .w_full()
            .h(px(CLASSIC_TOOL_STRIP_HEIGHT))
    }

    fn tool_button(
        &self,
        index: usize,
        tool: TimelineTool,
        label: &'static str,
        icon: &'static str,
        active: bool,
    ) -> impl IntoElement {
        let base = Colors::surface_panel();
        let rest = if active {
            Colors::composite(base, Colors::state_selected())
        } else {
            Colors::with_alpha(base, 0.0)
        };
        let hover = Colors::composite(if active { rest } else { base }, Colors::state_hover());
        let pressed = Colors::composite(if active { rest } else { base }, Colors::state_recessed());
        let timeline = self.timeline.clone();
        div()
            .id(("classic-tool", index))
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(size::COMFORTABLE))
            .rounded(px(radius::CONTROL))
            .bg(rest)
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |style| style.bg(hover))
            .active(move |style| style.bg(pressed))
            .tooltip(fb_tooltip(label))
            .on_click(move |_, window, cx| {
                let _ = timeline.update(cx, |timeline, cx| timeline.select_tool(tool, window, cx));
            })
            // Selection is the fill plus a marker on the strip's lower edge,
            // like a tab over the ruler — never a border change.
            .when(active, |button| {
                button.child(
                    div()
                        .absolute()
                        .bottom(px(-space::TIGHT))
                        .left(px(space::SNUG))
                        .right(px(space::SNUG))
                        .h(px(2.0))
                        .rounded(px(radius::PILL))
                        .bg(Colors::accent_primary()),
                )
            })
            .child(svg().path(icon).size(px(16.0)).text_color(if active {
                Colors::accent_primary()
            } else {
                Colors::text_secondary()
            }))
    }

    fn zoom_step(
        &self,
        id: &'static str,
        icon: &'static str,
        tip: &'static str,
        steps: i32,
    ) -> impl IntoElement {
        let base = Colors::surface_panel();
        let hover = Colors::composite(base, Colors::state_hover());
        let timeline = self.timeline.clone();
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(px(STEP_WIDTH))
            .h(px(size::COMFORTABLE))
            .rounded(px(radius::CONTROL_SM))
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |style| style.bg(hover))
            .tooltip(fb_tooltip(tip))
            .on_click(move |_, _window, cx| {
                let _ = timeline.update(cx, |timeline, cx| timeline.step_waveform_zoom(steps, cx));
            })
            .child(
                svg()
                    .path(icon)
                    .size(px(12.0))
                    .text_color(Colors::text_muted()),
            )
    }
}

/// A short hairline between tool groups.
fn divider() -> impl IntoElement {
    div()
        .flex_none()
        .w(px(1.0))
        .h(px(size::MICRO))
        .mx(px(space::TIGHT))
        .bg(Colors::border_subtle())
}

impl Render for ClassicToolStrip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (active_tool, zoom) = self.shown;
        let zoomed = zoom > 1.0 + 1.0e-3;

        let mut strip = div()
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::HAIR))
            .px(px(space::BASE))
            .overflow_hidden()
            .bg(Colors::surface_panel())
            .border_b(px(1.0))
            .border_color(Colors::border_subtle());
        for (index, (tool, label, icon, group_end)) in TIMELINE_TOOLS.into_iter().enumerate() {
            strip = strip.child(self.tool_button(index, tool, label, icon, tool == active_tool));
            if group_end {
                strip = strip.child(divider());
            }
        }

        // Waveform zoom at the far end: out, the readout (click for 1×), in.
        // View only — clip gain is unchanged.
        let reset_timeline = self.timeline.clone();
        let reset_hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());
        strip
            .child(div().flex_1().min_w(px(space::BASE)))
            .child(self.zoom_step(
                "classic-waveform-zoom-out",
                assets::ICON_MINUS_PATH,
                "Waveform zoom out",
                -1,
            ))
            .child(
                div()
                    .id("classic-waveform-zoom-reset")
                    .flex()
                    .flex_row()
                    .flex_none()
                    .items_center()
                    .gap(px(space::TIGHT))
                    .h(px(size::COMFORTABLE))
                    .px(px(space::TIGHT))
                    .rounded(px(radius::CONTROL_SM))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(move |style| style.bg(reset_hover))
                    .tooltip(fb_tooltip(
                        "Waveform zoom — view only, clip gain is unchanged. Click for 1×",
                    ))
                    .on_click(move |_, _window, cx| {
                        let _ = reset_timeline
                            .update(cx, |timeline, cx| timeline.step_waveform_zoom(0, cx));
                    })
                    .child(
                        svg()
                            .path(assets::ICON_AUDIO_LINES_PATH)
                            .size(px(14.0))
                            .text_color(if zoomed {
                                Colors::text_primary()
                            } else {
                                Colors::text_muted()
                            }),
                    )
                    .child(
                        div()
                            .min_w(px(22.0))
                            .text_size(px(typography::UI_XS))
                            .text_color(if zoomed {
                                Colors::text_primary()
                            } else {
                                Colors::text_muted()
                            })
                            .child(format!("{zoom:.1}×")),
                    ),
            )
            .child(self.zoom_step(
                "classic-waveform-zoom-in",
                assets::ICON_PLUS_PATH,
                "Waveform zoom in",
                1,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strip has to hold a comfortable tool button with room under it for
    /// the selection marker.
    #[test]
    fn the_strip_fits_a_button_and_its_marker() {
        assert!(CLASSIC_TOOL_STRIP_HEIGHT >= size::COMFORTABLE + 2.0 * 2.0);
        assert!((CLASSIC_TOOL_STRIP_HEIGHT - size::COMFORTABLE) / 2.0 >= 2.0);
    }

    /// Both tool bars draw from one list: every tool appears once.
    #[test]
    fn every_tool_appears_once() {
        let mut tools: Vec<String> = TIMELINE_TOOLS
            .iter()
            .map(|(tool, _, _, _)| format!("{tool:?}"))
            .collect();
        let count = tools.len();
        tools.sort();
        tools.dedup();
        assert_eq!(tools.len(), count);
    }
}
