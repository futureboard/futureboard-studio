//! The arrangement's edit tools as a docked strip above the ruler — the
//! Classic tool bar.
//!
//! Region contract — owner: the studio shell, which stacks this strip on top
//! of the arrangement (directly above its ruler) when Appearance → Tool Bar is
//! Classic. State owner: the [`Timeline`] (active tool, time zoom, track
//! heights, waveform zoom); this view only reads it and calls the same
//! [`Timeline::select_tool`] the floating bar calls and the zoom setters the
//! wheel gestures share. Coordinate space: its own; it sits outside the
//! arrangement, whose pointer maths reads the measured lane and ruler origins
//! and so follows the arrangement down by the strip's height on its own. Size
//! source: [`CLASSIC_TOOL_STRIP_HEIGHT`], fixed; it takes the arrangement's
//! width. Scroll owner: none — it clips if the arrangement is narrower than its
//! controls.
//!
//! The right end holds three zoom sliders, as in Logic Pro: waveform height,
//! track height (vertical) and time (horizontal). Each is a log-scale slider
//! over the same limits as its wheel gesture; double-click resets it.
//!
//! A cached view of its own, observing the timeline but repainting only when
//! the tool or a zoom actually changes, so an edit in the arrangement does not
//! cost a strip rebuild.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

use super::floating_tools_bar::TIMELINE_TOOLS;
use super::timeline_state::TimelineTool;
use super::Timeline;
use crate::assets;
use crate::components::controls::fb_tooltip;
use crate::components::slider::slider_with_drag_callbacks;
use crate::theme::{radius, size, space, typography, Colors};

/// Height of the docked strip: a comfortable button with a tight inset above
/// and below, the lower one holding the selection marker.
pub const CLASSIC_TOOL_STRIP_HEIGHT: f32 = size::COMFORTABLE + 2.0 * space::TIGHT;

/// Rail width of the waveform and vertical zoom sliders.
const ZOOM_SLIDER_W: f32 = 72.0;
/// The time zoom covers a much wider range, so it gets a longer rail.
const TIME_ZOOM_SLIDER_W: f32 = 96.0;
/// A slider position change smaller than this is not worth a repaint.
const ZOOM_REPAINT_EPSILON: f32 = 1.0e-3;

/// What the strip last drew.
#[derive(Clone, Copy, PartialEq)]
struct Shown {
    tool: TimelineTool,
    /// Slider positions, `0.0..=1.0`: time, track height, waveform.
    horizontal: f32,
    vertical: f32,
    waveform: f32,
    /// The waveform zoom factor, for the readout.
    waveform_factor: f32,
}

impl Shown {
    fn of(timeline: &Timeline) -> Self {
        let (horizontal, vertical, waveform) = timeline.zoom_slider_positions();
        Self {
            tool: timeline.active_tool(),
            horizontal,
            vertical,
            waveform,
            waveform_factor: timeline.waveform_zoom(),
        }
    }

    /// Whether `next` draws differently from `self`.
    fn differs_from(&self, next: &Self) -> bool {
        self.tool != next.tool
            || (self.horizontal - next.horizontal).abs() > ZOOM_REPAINT_EPSILON
            || (self.vertical - next.vertical).abs() > ZOOM_REPAINT_EPSILON
            || (self.waveform - next.waveform).abs() > ZOOM_REPAINT_EPSILON
    }
}

pub struct ClassicToolStrip {
    timeline: Entity<Timeline>,
    shown: Shown,
    _observer: Subscription,
}

impl ClassicToolStrip {
    pub fn new(timeline: &Entity<Timeline>, cx: &mut Context<Self>) -> Self {
        let shown = Shown::of(timeline.read(cx));
        let _observer = cx.observe(timeline, |this, timeline, cx| {
            let next = Shown::of(timeline.read(cx));
            if this.shown.differs_from(&next) {
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

/// One labelled zoom slider: an icon naming the axis, then the rail.
///
/// `set` receives the new `0.0..=1.0` position while dragging (Shift for
/// fine), `reset` runs on a double-click. The tooltip sits on the icon so it
/// never covers the rail mid-drag.
fn zoom_slider(
    id: &'static str,
    icon: &'static str,
    tip: &'static str,
    position: f32,
    rail_w: f32,
    set: impl Fn(&f32, &mut Window, &mut App) + 'static,
    reset: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(space::TIGHT))
        .child(
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(size::DENSE))
                .tooltip(fb_tooltip(tip))
                .child(
                    svg()
                        .path(icon)
                        .size(px(14.0))
                        .text_color(Colors::text_muted()),
                ),
        )
        .child(div().flex().flex_none().items_center().w(px(rail_w)).child(
            slider_with_drag_callbacks(
                format!("{id}-rail"),
                position,
                Colors::accent_primary(),
                None::<fn(&f32, &mut Window, &mut App)>,
                Some(set),
                None::<fn(&mut Window, &mut App)>,
                Some(reset),
            ),
        ))
}

impl Render for ClassicToolStrip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.shown;
        let zoomed = shown.waveform_factor > 1.0 + 1.0e-3;

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
            strip = strip.child(self.tool_button(index, tool, label, icon, tool == shown.tool));
            if group_end {
                strip = strip.child(divider());
            }
        }

        // Zoom at the far end: waveform height (view only — clip gain is
        // unchanged), then track height and time.
        let waveform = {
            let timeline = self.timeline.clone();
            let reset = self.timeline.clone();
            zoom_slider(
                "classic-waveform-zoom",
                assets::ICON_AUDIO_LINES_PATH,
                "Waveform zoom — view only, clip gain is unchanged. Double-click for 1×",
                shown.waveform,
                ZOOM_SLIDER_W,
                move |norm, _window, cx| {
                    let _ = timeline.update(cx, |timeline, cx| {
                        timeline.set_waveform_zoom_norm(*norm, cx)
                    });
                },
                move |_window, cx| {
                    let _ = reset.update(cx, |timeline, cx| timeline.step_waveform_zoom(0, cx));
                },
            )
        };
        let vertical = {
            let timeline = self.timeline.clone();
            let reset = self.timeline.clone();
            zoom_slider(
                "classic-vertical-zoom",
                assets::ICON_MOVE_VERTICAL_PATH,
                "Track height zoom. Double-click for the default height",
                shown.vertical,
                ZOOM_SLIDER_W,
                move |norm, window, cx| {
                    let _ = timeline.update(cx, |timeline, cx| {
                        timeline.set_vertical_zoom_norm(*norm, window, cx)
                    });
                },
                move |window, cx| {
                    let _ =
                        reset.update(cx, |timeline, cx| timeline.reset_vertical_zoom(window, cx));
                },
            )
        };
        let horizontal = {
            let timeline = self.timeline.clone();
            let reset = self.timeline.clone();
            zoom_slider(
                "classic-horizontal-zoom",
                assets::ICON_MOVE_HORIZONTAL_PATH,
                "Time zoom. Double-click for the default zoom",
                shown.horizontal,
                TIME_ZOOM_SLIDER_W,
                move |norm, window, cx| {
                    let _ = timeline.update(cx, |timeline, cx| {
                        timeline.set_horizontal_zoom_norm(*norm, window, cx)
                    });
                },
                move |window, cx| {
                    let _ = reset.update(cx, |timeline, cx| {
                        timeline.reset_horizontal_zoom(window, cx)
                    });
                },
            )
        };

        strip
            .child(div().flex_1().min_w(px(space::BASE)))
            .child(waveform)
            .child(
                div()
                    .flex_none()
                    .min_w(px(28.0))
                    .text_size(px(typography::UI_XS))
                    .text_color(if zoomed {
                        Colors::text_primary()
                    } else {
                        Colors::text_muted()
                    })
                    .child(format!("{:.1}×", shown.waveform_factor)),
            )
            .child(divider())
            .child(vertical)
            .child(div().flex_none().w(px(space::SNUG)))
            .child(horizontal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strip has to hold a comfortable tool button with room under it for
    /// the selection marker.
    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "documents the tool strip's layout constants as a named test"
    )]
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
