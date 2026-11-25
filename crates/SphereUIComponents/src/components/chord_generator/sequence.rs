//! The sequence panel: a ruler, the sections of chords, and the Comp and
//! Bass pattern lanes under them, all on one beat-to-pixel transform.

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, AnyElement, Bounds, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, Window,
};

use super::model::{Insert, Lane, Region, Section, BEATS_PER_BAR};
use super::{
    beats_label, family_color, fb_tooltip, key_dot, section_label, segment_position,
    sequence_content_width, tabular_features, ChordGeneratorWindow, PressKind, RegionMode,
    Selection, CHORD_LANE_H, EDGE_W, LABEL_W, LENGTHS, PATTERN_LANE_H, REGION_INSET, RULER_H,
    SECTION_TAB_H,
};
use crate::assets;
use crate::components::controls::{
    fb_button, fb_section_label, fb_segment, fb_segmented_track, FbButtonKind,
};
use crate::theme::{radius, space, typography, Colors};

impl ChordGeneratorWindow {
    pub(super) fn sequence(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let arrangement = self.shown();
        let bars = arrangement.bars();
        let total = arrangement.total_beats();
        let ppb = sequence_content_width(window) / total.max(1.0) as f32;

        let length =
            fb_segmented_track().children(LENGTHS.iter().enumerate().map(|(index, &n)| {
                fb_segment(
                    ("chordgen-bars", index),
                    format!("{n}"),
                    bars == n,
                    segment_position(index, LENGTHS.len()),
                    cx.listener(move |this, _, _, cx| this.set_bars(n, cx)),
                )
            }));
        let voicings = [(false, "Close"), (true, "Open")];
        let voicing = fb_segmented_track().children(voicings.iter().enumerate().map(
            |(index, (open, label))| {
                let open = *open;
                fb_segment(
                    ("chordgen-voicing", index),
                    *label,
                    self.voicing.open == open,
                    segment_position(index, voicings.len()),
                    cx.listener(move |this, _, _, cx| {
                        this.voicing.open = open;
                        cx.notify();
                    }),
                )
            },
        ));
        let sections = arrangement.sections().len();
        let summary = if sections == 0 {
            "Empty".to_string()
        } else {
            format!(
                "{} {} · {} {} · {}",
                sections,
                if sections == 1 { "section" } else { "sections" },
                arrangement.chord_count(),
                if arrangement.chord_count() == 1 {
                    "chord"
                } else {
                    "chords"
                },
                beats_label(arrangement.chords_end())
            )
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(space::BASE))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(fb_section_label("SEQUENCE"))
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .font_features(tabular_features())
                            .text_color(Colors::text_muted())
                            .child(summary),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child(
                        "Drag chords in from the palette · Alt-drag copies · \
                         drag a section by its tab",
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("Voicing"),
                    )
                    .child(div().w(px(124.0)).child(voicing))
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("Bars"),
                    )
                    .child(div().w(px(180.0)).child(length))
                    .child(fb_button(
                        "chordgen-clear",
                        "Clear",
                        FbButtonKind::Ghost,
                        !self.arrangement.is_empty(),
                        cx.listener(|this, _, _, cx| this.clear(cx)),
                    )),
            );

        // Ruler: bar numbers where they fit.
        let label_every = (1..=8u32)
            .find(|n| *n as f32 * BEATS_PER_BAR as f32 * ppb >= 26.0)
            .unwrap_or(8);
        let ruler = div()
            .relative()
            .h(px(RULER_H))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .children((0..bars).filter(|b| b % label_every == 0).map(|bar| {
                div()
                    .absolute()
                    .left(px(bar as f32 * BEATS_PER_BAR as f32 * ppb + space::TIGHT))
                    .top(px(space::HAIR))
                    .text_size(px(typography::UI_XS))
                    .font_features(tabular_features())
                    .text_color(Colors::text_muted())
                    .child(format!("{}", bar + 1))
            }));

        let chord_lane = self.chord_lane(ppb, cx);
        let pattern_lanes = Lane::ALL.map(|lane| self.pattern_lane(lane, ppb, cx));

        let labels = div()
            .flex_none()
            .w(px(LABEL_W))
            .flex()
            .flex_col()
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .h(px(RULER_H))
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle()),
            )
            .child(lane_label("Chords", CHORD_LANE_H))
            .children(Lane::ALL.map(|lane| lane_label(lane.label(), PATTERN_LANE_H)));

        // Grid (bars lead, beats recede) under everything; the painted
        // bounds are kept for mapping the pointer to beats.
        let bounds_cell = self.lanes_bounds.clone();
        let bar_line = Colors::border_normal();
        let beat_line = Colors::with_alpha(Colors::border_subtle(), 0.6);
        let grid = canvas(
            move |bounds, _, _| bounds_cell.set(Some(bounds)),
            move |bounds, _, window, _| {
                let ox: f32 = bounds.origin.x.into();
                let oy: f32 = bounds.origin.y.into();
                let h: f32 = bounds.size.height.into();
                let beats = total.round() as usize;
                for beat in 0..=beats {
                    let x = ox + beat as f32 * ppb;
                    let bar = beat % BEATS_PER_BAR as usize == 0;
                    if !bar && ppb < 6.0 {
                        continue;
                    }
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            gpui::point(px(x), px(oy + RULER_H)),
                            gpui::size(px(1.0), px(h - RULER_H)),
                        ),
                        if bar { bar_line } else { beat_line },
                    ));
                }
            },
        )
        .absolute()
        .inset_0();

        let marker = self.marker.map(|marker| {
            let x = marker.beat as f32 * ppb;
            let new_section = matches!(marker.at, Insert::NewSection(_));
            div()
                .absolute()
                .left(px(x - 1.0))
                .top(px(RULER_H))
                .h(px(CHORD_LANE_H))
                .w(px(2.0))
                .bg(Colors::accent_primary())
                .when(new_section, |line| {
                    line.child(
                        div()
                            .absolute()
                            .top(px(-RULER_H + space::HAIR))
                            .left(px(space::TIGHT))
                            .h(px(RULER_H - 2.0 * space::HAIR))
                            .px(px(space::SNUG))
                            .flex()
                            .items_center()
                            .rounded(px(radius::CONTROL_SM))
                            .bg(Colors::accent_primary())
                            .text_size(px(typography::DENSE_CAPTION))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::on_accent())
                            .whitespace_nowrap()
                            .child("New section"),
                    )
                })
        });

        let playhead = self.playback.as_ref().filter(|p| p.timeline).map(|p| {
            div()
                .absolute()
                .left(px(p.beat as f32 * ppb))
                .top_0()
                .bottom_0()
                .w(px(1.0))
                .bg(Colors::accent_primary())
        });

        let content = div()
            .flex_1()
            .min_w(px(0.0))
            .relative()
            .flex()
            .flex_col()
            .child(grid)
            .child(ruler)
            .child(chord_lane)
            .children(pattern_lanes)
            .children(marker)
            .children(playhead);

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .rounded(px(radius::SURFACE))
                    .overflow_hidden()
                    .bg(Colors::surface_canvas())
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .child(labels)
                    .child(content),
            )
    }

    fn chord_lane(&self, ppb: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let arrangement = self.shown();
        let mut start = 0.0;
        let mut sections: Vec<AnyElement> = Vec::new();
        for (index, section) in arrangement.sections().iter().enumerate() {
            sections.push(self.section(index, section, start, ppb, cx));
            start += section.beats();
        }
        let empty = sections.is_empty() && self.marker.is_none();
        div()
            .id("chordgen-chord-lane")
            .relative()
            .h(px(CHORD_LANE_H))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.selection != Selection::None {
                        this.selection = Selection::None;
                        cx.notify();
                    }
                }),
            )
            .when(empty, |lane| {
                lane.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_faint())
                        .child("Drag chords here from the palette, or press Fill"),
                )
            })
            .children(sections)
    }

    fn section(
        &self,
        index: usize,
        section: &Section,
        start: f64,
        ppb: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = section.id;
        let beats = section.beats();
        let x = start as f32 * ppb;
        let w = (beats as f32 * ppb - 2.0).max(4.0);
        let h = CHORD_LANE_H - 2.0 * REGION_INSET;
        let selected = self.selection == Selection::Section(id);
        let moving = self
            .press
            .is_some_and(|p| p.dragging && p.kind == PressKind::Section(id));
        let label = section_label(index);
        let tab_rest = Colors::with_alpha(Colors::surface_raised(), 0.0);
        let tab_hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());
        let tonic = self.settings.tonic;
        let moving_chords: Vec<u64> = match self.press {
            Some(p) if p.dragging => match p.kind {
                PressKind::Chord {
                    id, stretch: false, ..
                } => {
                    let selected = self.selected_chords();
                    if selected.contains(&id) {
                        selected
                    } else {
                        vec![id]
                    }
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };

        let mut at = 0.0;
        let mut slots: Vec<AnyElement> = Vec::with_capacity(section.slots.len());
        for slot in &section.slots {
            let slot_x = at as f32 * ppb + space::HAIR;
            let slot_w = (slot.beats as f32 * ppb - 2.0 * space::HAIR).max(4.0);
            let slot_h = h - SECTION_TAB_H - space::HAIR;
            at += slot.beats;
            let slot_id = slot.id;
            let chord = slot.chord;
            let color = family_color(&chord);
            let in_key = self.in_song_key(&chord);
            let picked =
                matches!(&self.selection, Selection::Chords(ids) if ids.contains(&slot_id));
            let rest = Colors::composite(Colors::surface_raised(), Colors::with_alpha(color, 0.16));
            let fill = if picked {
                Colors::composite(rest, Colors::state_selected())
            } else {
                rest
            };
            let original_beats = slot.beats;
            let big = slot_h > 40.0;
            slots.push(
                div()
                    .id(("chordgen-slot", slot_id))
                    .absolute()
                    .left(px(slot_x))
                    .top(px(SECTION_TAB_H))
                    .w(px(slot_w))
                    .h(px(slot_h))
                    .rounded(px(radius::clamped(radius::CONTROL_SM, slot_w, slot_h)))
                    .overflow_hidden()
                    .bg(fill)
                    .border(px(1.0))
                    .border_color(if picked {
                        Colors::with_alpha(Colors::accent_primary(), 0.9)
                    } else {
                        Colors::with_alpha(color, 0.5)
                    })
                    .when(moving_chords.contains(&slot_id), |el| el.opacity(0.45))
                    .cursor(gpui::CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.begin_press(
                                PressKind::Chord {
                                    id: slot_id,
                                    stretch: false,
                                    original_beats,
                                },
                                event.position,
                                event.modifiers,
                                cx,
                            );
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(space::SNUG))
                            .top(px(space::TIGHT))
                            .right(px(EDGE_W + space::TIGHT))
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(space::TIGHT))
                                    .child(key_dot(in_key, color))
                                    .child(
                                        div()
                                            .text_size(px(if big {
                                                typography::UI_MD
                                            } else {
                                                typography::UI_XS
                                            }))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(Colors::text_primary())
                                            .whitespace_nowrap()
                                            .child(self.chord_name(&chord)),
                                    )
                                    .when(slot.locked, |row| {
                                        row.child(
                                            gpui::svg()
                                                .path(assets::ICON_LOCK_PATH)
                                                .w(px(10.0))
                                                .h(px(10.0))
                                                .text_color(Colors::text_muted()),
                                        )
                                    }),
                            )
                            .when(big, |col| {
                                col.child(
                                    div()
                                        .pl(px(7.0 + space::TIGHT))
                                        .text_size(px(typography::UI_XS))
                                        .text_color(Colors::text_muted())
                                        .whitespace_nowrap()
                                        .child(chord.roman(tonic)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id(("chordgen-slot-edge", slot_id))
                            .absolute()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .w(px(EDGE_W))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.begin_press(
                                        PressKind::Chord {
                                            id: slot_id,
                                            stretch: true,
                                            original_beats,
                                        },
                                        event.position,
                                        event.modifiers,
                                        cx,
                                    );
                                }),
                            ),
                    )
                    .into_any_element(),
            );
        }

        div()
            .id(("chordgen-section", id))
            .absolute()
            .left(px(x + 1.0))
            .top(px(REGION_INSET))
            .w(px(w))
            .h(px(h))
            .rounded(px(radius::clamped(radius::CONTROL, w, h)))
            .bg(Colors::surface_panel())
            .border(px(1.0))
            .border_color(if selected {
                Colors::with_alpha(Colors::accent_primary(), 0.9)
            } else {
                Colors::border_subtle()
            })
            .when(moving, |el| el.opacity(0.45))
            .child(
                div()
                    .id(("chordgen-section-tab", id))
                    .absolute()
                    .left(px(space::HAIR))
                    .top(px(space::HAIR))
                    .h(px(SECTION_TAB_H - space::HAIR))
                    .max_w(px((w - 2.0 * space::HAIR).max(0.0)))
                    .px(px(space::SNUG))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::TIGHT))
                    .overflow_hidden()
                    .rounded(px(radius::CONTROL_SM))
                    .bg(tab_rest)
                    .hover(move |s| s.bg(tab_hover))
                    .cursor(gpui::CursorStyle::OpenHand)
                    .tooltip(fb_tooltip(
                        "Section — click to select, drag to move, Alt-drag to copy",
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.begin_press(
                                PressKind::Section(id),
                                event.position,
                                event.modifiers,
                                cx,
                            );
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(typography::DENSE_LABEL))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(if selected {
                                Colors::accent_primary()
                            } else {
                                Colors::text_secondary()
                            })
                            .child(label),
                    )
                    .when(w > 90.0, |tab| {
                        tab.child(
                            div()
                                .text_size(px(typography::DENSE_CAPTION))
                                .font_features(tabular_features())
                                .text_color(Colors::text_faint())
                                .whitespace_nowrap()
                                .child(beats_label(beats)),
                        )
                    }),
            )
            .children(slots)
            .into_any_element()
    }

    fn pattern_lane(&self, lane: Lane, ppb: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let arrangement = self.shown();
        let regions: Vec<AnyElement> = arrangement
            .lane(lane)
            .map(|region| self.pattern_region(region, ppb, cx))
            .collect();
        let empty = regions.is_empty();
        let bounds = self.lanes_bounds.clone();
        div()
            .id(("chordgen-lane", lane as usize))
            .relative()
            .h(px(PATTERN_LANE_H))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            // A click on empty lane space clears the selection; a
            // double-click fills the empty stretch with a pattern.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        if let Some(b) = bounds.get() {
                            let x: f32 = (event.position.x - b.origin.x).into();
                            let beat = (x / ppb.max(1.0)).max(0.0) as f64;
                            this.add_pattern_at(lane, beat, cx);
                        }
                        return;
                    }
                    if this.selection != Selection::None {
                        this.selection = Selection::None;
                        cx.notify();
                    }
                }),
            )
            .when(empty, |lane_el| {
                lane_el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .px(px(space::BASE))
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_faint())
                        .child("Silent — double-click to add a pattern"),
                )
            })
            .children(regions)
    }

    fn pattern_region(&self, region: &Region, ppb: f32, cx: &mut Context<Self>) -> AnyElement {
        let x = region.start as f32 * ppb;
        let w = (region.length as f32 * ppb).max(2.0);
        let h = PATTERN_LANE_H - 2.0 * REGION_INSET;
        let selected = self.selection == Selection::Region(region.id);
        let rest = Colors::surface_raised();
        let fill = if selected {
            Colors::composite(rest, Colors::state_selected())
        } else {
            rest
        };
        let id = region.id;
        let original = *region;
        let onsets = region.pattern.onsets();
        let ticks = (ppb * 0.5 >= 2.5).then(|| {
            let bars = (region.length / BEATS_PER_BAR).ceil() as usize;
            let mut xs = Vec::new();
            for bar in 0..bars {
                for onset in &onsets {
                    let beat = bar as f64 * BEATS_PER_BAR + onset;
                    if beat < region.length - 1e-9 {
                        xs.push(beat as f32 * ppb);
                    }
                }
            }
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(space::HAIR))
                .h(px(4.0))
                .children(xs.into_iter().map(|x| {
                    div()
                        .absolute()
                        .left(px(x))
                        .top_0()
                        .w(px(2.0))
                        .h(px(4.0))
                        .bg(Colors::with_alpha(Colors::text_secondary(), 0.7))
                }))
        });
        div()
            .id(("chordgen-region", id))
            .absolute()
            .left(px(x))
            .top(px(REGION_INSET))
            .w(px(w))
            .h(px(h))
            .rounded(px(radius::clamped(radius::CONTROL_SM, w, h)))
            .overflow_hidden()
            .bg(fill)
            .border(px(1.0))
            .border_color(if selected {
                Colors::with_alpha(Colors::accent_primary(), 0.9)
            } else {
                Colors::border_normal()
            })
            .cursor(gpui::CursorStyle::OpenHand)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.begin_press(
                        PressKind::Region {
                            id,
                            mode: RegionMode::Move,
                            original,
                        },
                        event.position,
                        event.modifiers,
                        cx,
                    );
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(px(space::SNUG))
                    .top(px(space::HAIR))
                    .right(px(EDGE_W))
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(region.pattern.label()),
            )
            .children(ticks)
            .child(
                div()
                    .id(("chordgen-region-edge", id))
                    .absolute()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .w(px(EDGE_W))
                    .cursor(gpui::CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.begin_press(
                                PressKind::Region {
                                    id,
                                    mode: RegionMode::Stretch,
                                    original,
                                },
                                event.position,
                                event.modifiers,
                                cx,
                            );
                        }),
                    ),
            )
            .into_any_element()
    }
}

fn lane_label(label: &'static str, height: f32) -> impl IntoElement {
    div()
        .h(px(height))
        .px(px(space::BASE))
        .flex()
        .items_center()
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .child(label)
}
