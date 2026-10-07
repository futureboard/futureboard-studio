//! Rodhareist's native editor view.
//!
//! Laid out after the Rodhareist plug-in design reference, inside the shared
//! native plug-in shell:
//!
//! ```txt
//! ┌ preset bar ─ preset name ············ power · trims · host tempo ┐
//! ├ presets ┬ signal path: INPUT meter · IN ─●─●─●─ OUT · MAIN meter ┤
//! │ (bank)  ├ EDIT: categories │ models │ parameter sliders          ┤
//! └─────────┴─────────────────────────────────────────────────────────┘
//! ```
//!
//! Owns no plug-in state: everything here reads the window's [`Params`] and
//! hands back wire edits through the window's `*_cb` builders. The design's
//! second path (a split) and editable tempo are not drawn: the DSP has one
//! serial path, and tempo follows the host transport.
//!
//! The colours are the plug-in's own identity, scoped to this editor's
//! bounds (DESIGN.md, "Built-in plugin editor signature"); Studio chrome
//! around it keeps the theme tokens.

use std::collections::HashMap;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, relative, svg, AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    Rgba, StatefulInteractiveElement, Styled,
};
use rodharerist::{CabModel, Params, StageKind, ToneEngineKind};

use crate::components::controls::fb_tooltip;
use crate::components::rodhareist_blocks::{self as blocks, Category, ParamSpec, Tint};
use crate::components::rodhareist_window::{IoMeter, RodhareistEditorWindow};
use crate::components::slider::slider_with_reset;

/// The plug-in's palette and proportions, from the design reference.
mod pal {
    use gpui::{rgb, rgba, Rgba};

    pub fn bg() -> Rgba {
        rgb(0x0d0e12)
    }
    pub fn panel() -> Rgba {
        rgb(0x14161b)
    }
    pub fn path_bg() -> Rgba {
        rgb(0x11131a)
    }
    pub fn column() -> Rgba {
        rgb(0x101217)
    }
    pub fn header() -> Rgba {
        rgb(0x181b21)
    }
    pub fn node() -> Rgba {
        rgb(0x191c23)
    }
    pub fn hover() -> Rgba {
        rgb(0x21242c)
    }
    pub fn pressed() -> Rgba {
        rgb(0x0f1014)
    }
    pub fn picked() -> Rgba {
        rgb(0x2a2d32)
    }
    pub fn meter_bg() -> Rgba {
        rgb(0x1c1f26)
    }
    pub fn wire() -> Rgba {
        rgb(0x343842)
    }
    pub fn port() -> Rgba {
        rgb(0x3a3e47)
    }
    pub fn line() -> Rgba {
        rgba(0xffffff12)
    }
    pub fn line_strong() -> Rgba {
        rgba(0xffffff17)
    }
    pub fn text() -> Rgba {
        rgb(0xd9d6cf)
    }
    pub fn muted() -> Rgba {
        rgb(0x8d9096)
    }
    pub fn dim() -> Rgba {
        rgb(0x6f7278)
    }
    pub fn bypassed() -> Rgba {
        rgb(0x2e3136)
    }
    pub fn amber() -> Rgba {
        rgb(0xe7a838)
    }
    pub fn amber_ring() -> Rgba {
        rgba(0xe7a83833)
    }
    pub fn teal() -> Rgba {
        rgb(0x4fc3c4)
    }
    pub fn neutral() -> Rgba {
        rgb(0xb9b4a8)
    }
    pub fn on_accent() -> Rgba {
        rgb(0x14110a)
    }
    pub fn green() -> Rgba {
        rgb(0x4fb87d)
    }
    pub fn red() -> Rgba {
        rgb(0xd65a4a)
    }

    pub const R_PANEL: f32 = 18.0;
    pub const R_NODE: f32 = 14.0;
    pub const R_CONTROL: f32 = 12.0;
    pub const R_ROW: f32 = 10.0;
    pub const R_PORT: f32 = 9.0;
}

fn tint(t: Tint) -> Rgba {
    match t {
        Tint::Amber => pal::amber(),
        Tint::Teal => pal::teal(),
        Tint::Neutral => pal::neutral(),
    }
}

/// Which chain slot the edit panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainFocus {
    /// The block in this chain position.
    Block(StageKind),
    /// An empty position, waiting for a block.
    Empty(usize),
}

/// What the editor actually shows: the requested focus while it still
/// matches the chain, else the first block (or the first slot of an empty
/// chain) — so an undo or preset that removes the focused block never
/// leaves the editor pointing at nothing.
fn resolve_focus(order: &[Option<StageKind>], requested: Option<ChainFocus>) -> ChainFocus {
    match requested {
        Some(ChainFocus::Block(kind)) if order.contains(&Some(kind)) => ChainFocus::Block(kind),
        Some(ChainFocus::Empty(slot)) if order.get(slot) == Some(&None) => ChainFocus::Empty(slot),
        _ => order
            .iter()
            .flatten()
            .next()
            .map(|kind| ChainFocus::Block(*kind))
            .unwrap_or(ChainFocus::Empty(0)),
    }
}

/// The stage of `category` that picking it should put in `slot`: the one
/// already there, else the first instance not yet on the path.
fn stage_for(order: &[Option<StageKind>], slot: usize, category: Category) -> Option<StageKind> {
    if let Some(Some(kind)) = order.get(slot) {
        if Category::of(*kind) == category {
            return Some(*kind);
        }
    }
    category
        .kinds()
        .iter()
        .copied()
        .find(|kind| !order.contains(&Some(*kind)))
}

fn focus_slot(order: &[Option<StageKind>], focus: ChainFocus) -> usize {
    match focus {
        ChainFocus::Block(kind) => order.iter().position(|s| *s == Some(kind)).unwrap_or(0),
        ChainFocus::Empty(slot) => slot,
    }
}

fn caption(text: impl Into<gpui::SharedString>) -> gpui::Div {
    div()
        .text_size(px(11.0))
        .text_color(pal::muted())
        .child(text.into())
}

/// A pressable row with the design's hover/press layers.
fn row_button(id: impl Into<gpui::ElementId>, rest: Rgba) -> gpui::Stateful<gpui::Div> {
    let hover = if rest.a == 0.0 { pal::hover() } else { rest };
    div()
        .id(id)
        .flex()
        .items_center()
        .rounded(px(pal::R_ROW))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(|s| s.bg(pal::pressed()))
}

fn clear() -> Rgba {
    gpui::rgba(0x00000000)
}

// ── Preset bar ──────────────────────────────────────────────────────────────

fn power_button(
    id: impl Into<gpui::ElementId>,
    on: bool,
    color: Rgba,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(32.0))
        .rounded_full()
        .bg(rgb_node_button())
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(pal::hover()))
        .active(|s| s.bg(pal::pressed()))
        .tooltip(fb_tooltip(label))
        .on_click(on_click)
        .child(
            svg()
                .path(crate::assets::ICON_POWER_PATH)
                .size(px(16.0))
                .text_color(if on { color } else { pal::dim() }),
        )
        .into_any_element()
}

fn rgb_node_button() -> Rgba {
    gpui::rgb(0x1e2128)
}

fn trim_control(
    window: &RodhareistEditorWindow,
    values: &HashMap<&'static str, f32>,
    id: &'static str,
    label: &'static str,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let Some(spec) = blocks::trim_spec(id, label) else {
        return div().into_any_element();
    };
    let value = values.get(id).copied().unwrap_or(0.0);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(caption(label))
        .child(div().w(px(96.0)).child(slider_with_reset(
            format!("rodhareist-{id}"),
            norm(value, &spec),
            pal::amber(),
            window.slider_cb(cx, spec.id, spec.min, spec.max),
            Some(window.reset_cb(cx, spec.id)),
        )))
        .child(
            div()
                .w(px(60.0))
                .text_size(px(12.0))
                .text_color(pal::text())
                .child(blocks::format_value(value, &spec)),
        )
        .into_any_element()
}

fn preset_bar(
    window: &RodhareistEditorWindow,
    p: &Params,
    values: &HashMap<&'static str, f32>,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let bank = rodharerist::factory_presets();
    let (name, edited) = match window.preset_status() {
        Some((index, edited)) => (bank[index].name, edited),
        None => ("No preset", false),
    };
    let tempo = window
        .host_tempo()
        .map(|bpm| format!("{bpm:.1}"))
        .unwrap_or_else(|| "—".to_string());
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(48.0))
        .px(px(16.0))
        .gap(px(12.0))
        .border_b(px(1.0))
        .border_color(pal::line())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .w(px(320.0))
                .h(px(34.0))
                .px(px(12.0))
                .rounded(px(pal::R_CONTROL))
                .bg(pal::panel())
                .border(px(1.0))
                .border_color(pal::line_strong())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(15.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if window.preset_status().is_some() {
                            pal::text()
                        } else {
                            pal::muted()
                        })
                        .child(name),
                )
                .when(edited, |el| el.child(caption("EDITED"))),
        )
        .child(div().flex_1())
        .child(trim_control(window, values, "input_trim", "IN", cx))
        .child(trim_control(window, values, "output_trim", "OUT", cx))
        .child(caption("TEMPO"))
        .child(
            div()
                .id("rodhareist-tempo")
                .flex()
                .items_center()
                .justify_center()
                .w(px(72.0))
                .h(px(34.0))
                .rounded(px(pal::R_CONTROL))
                .border(px(1.0))
                .border_color(pal::line_strong())
                .text_size(px(14.0))
                .text_color(pal::text())
                .tooltip(fb_tooltip("Host tempo — follows the transport"))
                .child(tempo),
        )
        .child(power_button(
            "rodhareist-power",
            p.power,
            pal::amber(),
            if p.power {
                "Rodhareist on — click to bypass"
            } else {
                "Rodhareist bypassed — click to turn on"
            },
            window.toggle_cb(cx, "power"),
        ))
        .into_any_element()
}

// ── Preset browser ──────────────────────────────────────────────────────────

fn preset_browser(
    window: &RodhareistEditorWindow,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let bank = rodharerist::factory_presets();
    let active = window.preset_status().map(|(index, _)| index);
    let mut list = div()
        .id("rodhareist-preset-list")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .py(px(3.0));
    for (index, preset) in bank.iter().enumerate() {
        let on = active == Some(index);
        list = list.child(
            row_button(
                ("rodhareist-preset", index),
                if on { pal::amber() } else { clear() },
            )
            .mx(px(6.0))
            .my(px(3.0))
            .h(px(32.0))
            .px(px(12.0))
            .gap(px(6.0))
            .on_click(window.load_preset_cb(cx, index))
            .child(
                div()
                    .w(px(30.0))
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(if on { pal::on_accent() } else { pal::muted() })
                    .child(preset.id),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(14.0))
                    .text_color(if on { pal::on_accent() } else { pal::text() })
                    .child(preset.name),
            ),
        );
    }
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(232.0))
        .min_h(px(0.0))
        .rounded(px(pal::R_PANEL))
        .bg(pal::panel())
        .border(px(1.0))
        .border_color(pal::line())
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .flex_shrink_0()
                .h(px(32.0))
                .px(px(12.0))
                .border_b(px(1.0))
                .border_color(pal::line())
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(pal::text())
                        .child("PRESETS"),
                )
                .child(caption(bank.len().to_string())),
        )
        .child(list)
        .into_any_element()
}

// ── Signal path ─────────────────────────────────────────────────────────────

/// -48..0 dBFS onto 0..1, the meter scale the labels print.
fn meter_fraction(level: f32) -> f32 {
    if level <= 1.0e-6 {
        return 0.0;
    }
    ((20.0 * level.log10() + 48.0) / 48.0).clamp(0.0, 1.0)
}

/// One vertical level bar: RMS fill, coloured by how hot it runs, with the
/// peak as a hairline. Square, so the top pixel is the value.
fn level_bar(rms: f32, peak: f32) -> AnyElement {
    let fill = meter_fraction(rms);
    let peak = meter_fraction(peak);
    let color = if fill > 0.9 {
        pal::red()
    } else if fill > 0.72 {
        pal::amber()
    } else {
        pal::green()
    };
    div()
        .relative()
        .w(px(8.0))
        .h_full()
        .bg(pal::meter_bg())
        .child(
            div()
                .absolute()
                .left_0()
                .bottom_0()
                .w_full()
                .h(relative(fill))
                .bg(color),
        )
        .when(peak > 0.0, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .w_full()
                    .h(px(2.0))
                    .bottom(relative(peak))
                    .bg(pal::text()),
            )
        })
        .into_any_element()
}

fn meter_scale() -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .justify_between()
        .h_full()
        .text_size(px(9.0))
        .text_color(pal::muted());
    for mark in ["0", "-12", "-24", "-36", "-48"] {
        col = col.child(mark);
    }
    col.into_any_element()
}

fn meter_column(
    window: &RodhareistEditorWindow,
    label: &'static str,
    rms: f32,
    peak: f32,
    clip: bool,
    scale_left: bool,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let bars = div()
        .flex()
        .flex_row()
        .gap(px(4.0))
        .flex_1()
        .min_h(px(0.0))
        .when(scale_left, |el| el.child(meter_scale()))
        .child(level_bar(rms, peak))
        .when(!scale_left, |el| el.child(meter_scale()));
    div()
        .flex()
        .flex_col()
        .items_center()
        .flex_shrink_0()
        .gap(px(6.0))
        .w(px(54.0))
        .child(caption(label))
        .child(bars)
        .child(
            div()
                .id(("rodhareist-clip", scale_left as usize))
                .px(px(6.0))
                .rounded(px(4.0))
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .bg(if clip { pal::red() } else { pal::meter_bg() })
                .text_color(if clip { pal::on_accent() } else { pal::dim() })
                .cursor(gpui::CursorStyle::PointingHand)
                .tooltip(fb_tooltip(if clip {
                    "Clipped — click to clear"
                } else {
                    "Clip indicator"
                }))
                .on_click(window.clear_clip_cb(cx))
                .child("CLIP"),
        )
        .into_any_element()
}

fn port(label: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(36.0))
        .h(px(26.0))
        .rounded(px(pal::R_PORT))
        .border(px(1.0))
        .border_color(pal::port())
        .text_size(px(10.0))
        .text_color(pal::muted())
        .child(label)
        .into_any_element()
}

fn wire_segment() -> AnyElement {
    div()
        .flex_1()
        .min_w(px(4.0))
        .h(px(2.0))
        .bg(pal::wire())
        .into_any_element()
}

fn path_node(
    window: &RodhareistEditorWindow,
    p: &Params,
    slot: usize,
    kind: StageKind,
    selected: bool,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let category = Category::of(kind);
    let accent = tint(category.tint());
    let on = blocks::is_on(p, kind);
    let model = blocks::model_name(p, kind).unwrap_or("No amp");
    let mut glow = accent;
    glow.a = 0.14;
    let node = div()
        .id(("rodhareist-node", slot))
        .flex()
        .items_center()
        .justify_center()
        .size(px(44.0))
        .rounded(px(pal::R_NODE))
        .bg(pal::node())
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if on { accent } else { pal::dim() })
        .when(selected, |el| {
            el.border(px(2.0))
                .border_color(pal::amber())
                .shadow(vec![gpui::BoxShadow {
                    color: pal::amber_ring().into(),
                    offset: gpui::point(px(0.0), px(0.0)),
                    blur_radius: px(0.0),
                    spread_radius: px(4.0),
                    inset: false,
                }])
        })
        .when(!selected, |el| {
            el.border(px(1.0))
                .border_color(if on { accent } else { pal::bypassed() })
                .when(on, |el| {
                    el.shadow(vec![gpui::BoxShadow {
                        color: glow.into(),
                        offset: gpui::point(px(0.0), px(0.0)),
                        blur_radius: px(16.0),
                        spread_radius: px(0.0),
                        inset: false,
                    }])
                })
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(pal::hover()))
        .tooltip(fb_tooltip(format!(
            "Slot {} · {}: {}{}",
            slot + 1,
            blocks::block_name(kind),
            model,
            if on { "" } else { " (bypassed)" }
        )))
        .on_click(window.focus_cb(cx, ChainFocus::Block(kind)))
        .child(category.glyph());
    div()
        .flex()
        .flex_col()
        .items_center()
        .flex_shrink_0()
        .gap(px(4.0))
        .w(px(56.0))
        .child(node)
        .child(
            div()
                .text_size(px(11.0))
                .whitespace_nowrap()
                .text_color(if on { pal::text() } else { pal::dim() })
                .child(blocks::block_name(kind)),
        )
        .into_any_element()
}

/// An empty chain position: a socket on the wire.
fn path_socket(
    window: &RodhareistEditorWindow,
    slot: usize,
    selected: bool,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    div()
        .id(("rodhareist-socket", slot))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        // Hit area wider than the socket; the label row keeps it on the
        // nodes' wire line.
        .w(px(22.0))
        .h(px(44.0))
        .cursor(gpui::CursorStyle::PointingHand)
        .tooltip(fb_tooltip(format!(
            "Slot {} · empty — click to add a block",
            slot + 1
        )))
        .on_click(window.focus_cb(cx, ChainFocus::Empty(slot)))
        .child(
            div()
                .size(px(12.0))
                .rounded_full()
                .bg(pal::path_bg())
                .border(px(if selected { 2.0 } else { 1.0 }))
                .border_color(if selected { pal::amber() } else { pal::port() }),
        )
        .into_any_element()
}

fn signal_path(
    window: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let io: IoMeter = window.io_meter();
    // The wire runs at the node centre; empty sockets and the ports sit on
    // it via the same 44 px row, with the node labels hanging below.
    let mut lane = div()
        .flex()
        .flex_row()
        .items_start()
        .min_w(px(0.0))
        .child(div().h(px(44.0)).flex().items_center().child(port("IN")));
    for (slot, entry) in p.stage_order.iter().enumerate() {
        lane = lane.child(
            div()
                .h(px(44.0))
                .flex_1()
                .min_w(px(4.0))
                .flex()
                .items_center()
                .child(wire_segment()),
        );
        lane = lane.child(match entry {
            Some(kind) => path_node(
                window,
                p,
                slot,
                *kind,
                focus == ChainFocus::Block(*kind),
                cx,
            ),
            None => path_socket(window, slot, focus == ChainFocus::Empty(slot), cx),
        });
    }
    lane = lane
        .child(
            div()
                .h(px(44.0))
                .w(px(20.0))
                .flex()
                .items_center()
                .child(wire_segment()),
        )
        .child(div().h(px(44.0)).flex().items_center().child(port("OUT")));

    let routing = div()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(14.0))
        .flex_1()
        .min_w(px(0.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(caption("PATH"))
                .child(caption(format!(
                    "{} of {} slots",
                    p.stage_order.iter().flatten().count(),
                    rodharerist::PATH_SLOTS
                ))),
        )
        .child(lane);

    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .gap(px(16.0))
        .h(px(170.0))
        .px(px(18.0))
        .py(px(14.0))
        .rounded(px(pal::R_PANEL))
        .bg(pal::path_bg())
        .border(px(1.0))
        .border_color(pal::line())
        .child(meter_column(
            window, "INPUT", io.in_rms, io.in_peak, io.in_clip, true, cx,
        ))
        .child(routing)
        .child(meter_column(
            window,
            "MAIN",
            io.out_rms,
            io.out_peak,
            io.out_clip,
            false,
            cx,
        ))
        .into_any_element()
}

// ── Edit panel ──────────────────────────────────────────────────────────────

fn category_column(
    window: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let order = p.stage_order;
    let slot = focus_slot(&order, focus);
    let current = match focus {
        ChainFocus::Block(kind) => Some(Category::of(kind)),
        ChainFocus::Empty(_) => None,
    };
    let mut col = div()
        .id("rodhareist-categories")
        .flex_shrink_0()
        .w(px(188.0))
        .h_full()
        .overflow_y_scroll()
        .py(px(8.0))
        .bg(pal::column())
        .border_r(px(1.0))
        .border_color(pal::line());
    for (index, category) in Category::ALL.iter().copied().enumerate() {
        let active = current == Some(category);
        let accent = tint(category.tint());
        let target = stage_for(&order, slot, category);
        let enabled = active || target.is_some();
        let mut row = row_button(
            ("rodhareist-category", index),
            if active { accent } else { clear() },
        )
        .mx(px(8.0))
        .my(px(4.0))
        .h(px(36.0))
        .px(px(18.0))
        .gap(px(12.0))
        .child(div().size(px(8.0)).rounded_full().bg(if active {
            pal::on_accent()
        } else {
            accent
        }))
        .child(
            div()
                .text_size(px(14.0))
                .text_color(if active {
                    pal::on_accent()
                } else if enabled {
                    pal::text()
                } else {
                    pal::dim()
                })
                .child(category.name()),
        );
        row = match target {
            Some(kind) if !active => row
                .tooltip(fb_tooltip(match focus {
                    ChainFocus::Empty(_) => {
                        format!("Add {} to slot {}", blocks::block_name(kind), slot + 1)
                    }
                    ChainFocus::Block(_) => {
                        format!("Replace this block with {}", blocks::block_name(kind))
                    }
                }))
                .on_click(window.chain_edit_cb(
                    cx,
                    move |p| p.stage_order[slot] = Some(kind),
                    ChainFocus::Block(kind),
                )),
            None if !active => row.tooltip(fb_tooltip(format!(
                "Every {} block is already on the path",
                category.name()
            ))),
            _ => row,
        };
        col = col.child(row);
    }
    col.into_any_element()
}

fn model_column(
    window: &RodhareistEditorWindow,
    p: &Params,
    focus: ChainFocus,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let col = div()
        .id("rodhareist-models")
        .flex_shrink_0()
        .w(px(224.0))
        .h_full()
        .overflow_y_scroll()
        .py(px(8.0))
        .bg(pal::column())
        .border_r(px(1.0))
        .border_color(pal::line());
    let ChainFocus::Block(kind) = focus else {
        return col
            .child(
                div()
                    .px(px(18.0))
                    .py(px(10.0))
                    .text_size(px(13.0))
                    .text_color(pal::muted())
                    .child("Pick a category to add a block to this slot."),
            )
            .into_any_element();
    };
    let accent = tint(Category::of(kind).tint());
    let (list, selected) = blocks::models(p, kind);
    let mut col = col;
    for (index, model) in list.into_iter().enumerate() {
        let active = selected == Some(index);
        let mut row = row_button(
            ("rodhareist-model", index),
            if active { pal::picked() } else { clear() },
        )
        .relative()
        .mx(px(8.0))
        .my(px(4.0))
        .h(px(36.0))
        .px(px(18.0))
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(15.0))
                .text_color(if active { accent } else { pal::text() })
                .child(model.label),
        )
        .when(active, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.0))
                    .bg(accent),
            )
        });
        if !model.wire_id.is_empty() && !active {
            row = row.on_click(window.set_cb(cx, model.wire_id, model.value));
        }
        col = col.child(row);
    }
    col.into_any_element()
}

fn norm(value: f32, spec: &ParamSpec) -> f32 {
    ((value - spec.min) / (spec.max - spec.min)).clamp(0.0, 1.0)
}

fn param_row(
    window: &RodhareistEditorWindow,
    spec: ParamSpec,
    value: f32,
    accent: Rgba,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(24.0))
        .h(px(42.0))
        .when(spec.inactive, |el| el.opacity(0.4))
        .child(
            div()
                .w(px(110.0))
                .flex_shrink_0()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(pal::text())
                .child(spec.label),
        )
        .child(div().flex_1().min_w(px(0.0)).child(slider_with_reset(
            format!("rodhareist-param-{}", spec.id),
            norm(value, &spec),
            accent,
            window.slider_cb(cx, spec.id, spec.min, spec.max),
            Some(window.reset_cb(cx, spec.id)),
        )))
        .child(
            div()
                .w(px(84.0))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_size(px(13.0))
                .text_color(accent)
                .child(blocks::format_value(value, &spec)),
        )
        .into_any_element()
}

/// A row of choice chips (microphone type).
fn chip_row(
    window: &RodhareistEditorWindow,
    label: &'static str,
    choices: Vec<blocks::ModelChoice>,
    selected: Option<usize>,
    accent: Rgba,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let mut chips = div().flex().flex_row().gap(px(8.0));
    for (index, choice) in choices.into_iter().enumerate() {
        let active = selected == Some(index);
        let mut chip = row_button(
            ("rodhareist-chip", index),
            if active { pal::picked() } else { clear() },
        )
        .h(px(30.0))
        .px(px(14.0))
        .border(px(1.0))
        .border_color(if active { accent } else { pal::line_strong() })
        .text_size(px(13.0))
        .text_color(if active { accent } else { pal::text() })
        .child(choice.label);
        if !active {
            chip = chip.on_click(window.set_cb(cx, choice.wire_id, choice.value));
        }
        chips = chips.child(chip);
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(24.0))
        .h(px(42.0))
        .child(
            div()
                .w(px(110.0))
                .flex_shrink_0()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(pal::text())
                .child(label),
        )
        .child(chips)
        .into_any_element()
}

/// A file slot (IR or NAM capture): its load button and what is loaded.
#[allow(clippy::too_many_arguments)]
fn load_row(
    id: &'static str,
    label: &'static str,
    button: &'static str,
    loaded: Option<String>,
    loading: bool,
    error: Option<String>,
    accent: Rgba,
    on_load: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let status = if loading {
        ("Loading…".to_string(), pal::muted())
    } else if let Some(error) = error {
        (error, pal::red())
    } else if let Some(name) = loaded {
        (name, accent)
    } else {
        ("Nothing loaded".to_string(), pal::muted())
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(24.0))
        .h(px(42.0))
        .child(
            div()
                .w(px(110.0))
                .flex_shrink_0()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(pal::text())
                .child(label),
        )
        .child(
            row_button(id, clear())
                .flex_shrink_0()
                .h(px(30.0))
                .px(px(14.0))
                .border(px(1.0))
                .border_color(pal::line_strong())
                .text_size(px(13.0))
                .text_color(pal::text())
                .when(!loading, |el| el.on_click(on_load))
                .child(button),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(13.0))
                .text_color(status.1)
                .child(status.0),
        )
        .into_any_element()
}

fn slot_action(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(28.0))
        .rounded(px(pal::R_ROW))
        .tooltip(fb_tooltip(label))
        .when(enabled, |el| {
            el.cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(pal::hover()))
                .active(|s| s.bg(pal::pressed()))
                .on_click(on_click)
        })
        .child(svg().path(icon).size(px(14.0)).text_color(if enabled {
            pal::muted()
        } else {
            pal::bypassed()
        }))
        .into_any_element()
}

fn param_column(
    window: &RodhareistEditorWindow,
    p: &Params,
    values: &HashMap<&'static str, f32>,
    focus: ChainFocus,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(40.0))
        .px(px(24.0))
        .gap(px(12.0))
        .border_b(px(1.0))
        .border_color(pal::line())
        .bg(pal::header());
    let ChainFocus::Block(kind) = focus else {
        let ChainFocus::Empty(slot) = focus else {
            unreachable!()
        };
        return div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .child(
                header.child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(pal::text())
                        .child(format!("Slot {} — empty", slot + 1)),
                ),
            )
            .into_any_element();
    };

    let category = Category::of(kind);
    let accent = tint(category.tint());
    let on = blocks::is_on(p, kind);
    let slot = focus_slot(&p.stage_order, focus);
    let last = rodharerist::PATH_SLOTS - 1;
    let title = format!(
        "{} — {}",
        blocks::block_name(kind),
        blocks::model_name(p, kind).unwrap_or("No amp")
    );
    let header = header
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(pal::text())
                .child(title),
        )
        .child(slot_action(
            "rodhareist-block-left",
            "icons/chevron-left.svg",
            "Move block earlier in the path",
            slot > 0,
            window.chain_edit_cb(
                cx,
                move |p| p.stage_order.swap(slot, slot.saturating_sub(1)),
                ChainFocus::Block(kind),
            ),
        ))
        .child(slot_action(
            "rodhareist-block-right",
            "icons/chevron-right.svg",
            "Move block later in the path",
            slot < last,
            window.chain_edit_cb(
                cx,
                move |p| p.stage_order.swap(slot, (slot + 1).min(last)),
                ChainFocus::Block(kind),
            ),
        ))
        .child(slot_action(
            "rodhareist-block-remove",
            "icons/x.svg",
            "Remove block from the path",
            true,
            window.chain_edit_cb(
                cx,
                move |p| p.stage_order[slot] = None,
                ChainFocus::Empty(slot),
            ),
        ))
        .child(caption(if on { "ON" } else { "BYPASSED" }))
        .child(power_button(
            "rodhareist-block-power",
            on,
            accent,
            if on { "Bypass block" } else { "Enable block" },
            window.toggle_cb(cx, blocks::enable_id(kind)),
        ));

    let mut rows = div()
        .id("rodhareist-params")
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .pt(px(12.0))
        .pb(px(16.0))
        .px(px(32.0))
        .when(!on, |el| el.opacity(0.5));
    if kind == StageKind::Cab {
        rows = rows.child(chip_row(
            window,
            "Microphone",
            blocks::mic_choices(),
            blocks::mic_index(p),
            accent,
            cx,
        ));
        if p.cab_model == CabModel::Ir {
            rows = rows.child(load_row(
                "rodhareist-load-ir",
                "Impulse",
                "Load IR…",
                window
                    .ir_loaded_info()
                    .map(|(name, secs)| format!("{name} ({secs:.2} s)")),
                window.ir_loading(),
                window.ir_error(),
                accent,
                window.load_ir_cb(cx),
            ));
        }
    }
    if kind == StageKind::Amp && p.tone_engine == ToneEngineKind::NamCapture {
        rows = rows.child(load_row(
            "rodhareist-load-nam",
            "Capture",
            "Load Capture…",
            window.nam_loaded_info(),
            window.nam_loading(),
            window.nam_error(),
            accent,
            window.load_nam_cb(cx),
        ));
    }
    for spec in blocks::params(p, kind) {
        let value = values.get(spec.id).copied().unwrap_or(spec.min);
        rows = rows.child(param_row(window, spec, value, accent, cx));
    }
    if kind == StageKind::Amp && p.tone_engine == ToneEngineKind::NamCapture {
        rows = rows.child(chip_row(
            window,
            "Loudness",
            vec![
                blocks::ModelChoice {
                    label: "Raw",
                    wire_id: "nam_loudness_norm",
                    value: 0.0,
                },
                blocks::ModelChoice {
                    label: "Normalized",
                    wire_id: "nam_loudness_norm",
                    value: 1.0,
                },
            ],
            Some(p.nam_loudness_norm as usize),
            accent,
            cx,
        ));
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .child(header)
        .child(rows)
        .into_any_element()
}

fn edit_panel(
    window: &RodhareistEditorWindow,
    p: &Params,
    values: &HashMap<&'static str, f32>,
    focus: ChainFocus,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .rounded(px(pal::R_PANEL))
        .bg(pal::panel())
        .border(px(1.0))
        .border_color(pal::line())
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .h(px(32.0))
                .border_b(px(1.0))
                .border_color(pal::line())
                .bg(pal::header())
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(pal::text())
                .child("EDIT"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .child(category_column(window, p, focus, cx))
                .child(model_column(window, p, focus, cx))
                .child(param_column(window, p, values, focus, cx)),
        )
        .into_any_element()
}

/// The whole editor body.
pub(crate) fn rodhareist_panel(
    window: &RodhareistEditorWindow,
    cx: &Context<RodhareistEditorWindow>,
) -> AnyElement {
    let p = window.params_snapshot();
    let values: HashMap<&'static str, f32> = rodharerist::ui_values(&p).into_iter().collect();
    let focus = resolve_focus(&p.stage_order, window.chain_focus());
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(pal::bg())
        .text_color(pal::text())
        .child(preset_bar(window, &p, &values, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .gap(px(12.0))
                .p(px(12.0))
                .child(preset_browser(window, cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .gap(px(12.0))
                        .child(signal_path(window, &p, focus, cx))
                        .child(edit_panel(window, &p, &values, focus, cx)),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_falls_back_when_its_block_leaves_the_path() {
        let mut order = [None; rodharerist::PATH_SLOTS];
        order[2] = Some(StageKind::Amp);
        order[5] = Some(StageKind::Cab);
        assert_eq!(
            resolve_focus(&order, Some(ChainFocus::Block(StageKind::Cab))),
            ChainFocus::Block(StageKind::Cab)
        );
        assert_eq!(
            resolve_focus(&order, Some(ChainFocus::Block(StageKind::Wah))),
            ChainFocus::Block(StageKind::Amp)
        );
        assert_eq!(
            resolve_focus(&order, Some(ChainFocus::Empty(3))),
            ChainFocus::Empty(3)
        );
        assert_eq!(
            resolve_focus(&[None; rodharerist::PATH_SLOTS], None),
            ChainFocus::Empty(0)
        );
    }

    #[test]
    fn picking_a_category_uses_a_free_instance() {
        let mut order = [None; rodharerist::PATH_SLOTS];
        order[0] = Some(StageKind::Drive);
        // Slot 0 already holds Drive A: picking Drive keeps it.
        assert_eq!(
            stage_for(&order, 0, Category::Drive),
            Some(StageKind::Drive)
        );
        // Another slot gets the free second instance.
        assert_eq!(
            stage_for(&order, 3, Category::Drive),
            Some(StageKind::Drive2)
        );
        order[1] = Some(StageKind::Drive2);
        assert_eq!(stage_for(&order, 3, Category::Drive), None);
        assert_eq!(stage_for(&order, 3, Category::Amp), Some(StageKind::Amp));
    }

    #[test]
    fn the_meter_spans_forty_eight_decibels() {
        assert_eq!(meter_fraction(0.0), 0.0);
        assert_eq!(meter_fraction(1.0), 1.0);
        assert!((meter_fraction(0.063) - 0.5).abs() < 0.01);
    }
}
