//! The chord palette: a key navigator over a grid of the key's chords, a
//! column per scale degree and a row per kind of chord.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled,
};
use sphere_midi_service::chords::{palette, pitch_name, Chord};

use super::{
    family_color, fb_tooltip, key_dot, segment_position, ChordGeneratorWindow, PressKind, LABEL_W,
};
use crate::assets;
use crate::components::controls::{
    fb_button, fb_icon_button, fb_section_label, fb_segment, fb_segmented_track, FbButtonKind,
};
use crate::theme::{radius, size, space, typography, Colors};

/// Height of one palette chord.
const TILE_H: f32 = 30.0;

impl ChordGeneratorWindow {
    pub(super) fn palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (tonic, scale) = self.palette_key();
        let chords = palette(scale, tonic);
        let home_scale = self.settings.scale;
        let parallel = home_scale.parallel();

        let distance = match self.palette_steps {
            0 => "Song key".to_string(),
            n if n > 0 => format!("{n} {} sharper", if n == 1 { "fifth" } else { "fifths" }),
            n => format!(
                "{} {} flatter",
                -n,
                if n == -1 { "fifth" } else { "fifths" }
            ),
        };
        let key_label = div()
            .flex_none()
            .w(px(124.0))
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .text_size(px(typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .whitespace_nowrap()
                    .child(format!(
                        "{} {}",
                        pitch_name(tonic, scale.prefers_flats(tonic)),
                        scale.label()
                    )),
            )
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(if self.palette_is_home() {
                        Colors::text_faint()
                    } else {
                        Colors::text_secondary()
                    })
                    .child(distance),
            );
        let mode = (parallel != home_scale).then(|| {
            let options = [(false, home_scale.label()), (true, parallel.label())];
            div().flex_none().w(px(184.0)).child(
                fb_segmented_track().children(options.iter().enumerate().map(
                    |(index, (is_parallel, label))| {
                        let is_parallel = *is_parallel;
                        fb_segment(
                            ("chordgen-palette-mode", index),
                            *label,
                            self.palette_parallel == is_parallel,
                            segment_position(index, options.len()),
                            cx.listener(move |this, _, _, cx| {
                                if this.palette_parallel != is_parallel {
                                    this.toggle_parallel(cx);
                                }
                            }),
                        )
                    },
                )),
            )
        });
        let legend = |in_key: bool, label: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(key_dot(in_key, Colors::text_muted()))
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(label),
                )
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .child(fb_section_label("PALETTE"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::TIGHT))
                    .child(
                        div()
                            .id("chordgen-palette-prev-tip")
                            .tooltip(fb_tooltip("A fifth flatter (←)"))
                            .child(fb_icon_button(
                                "chordgen-palette-prev",
                                assets::ICON_CHEVRON_LEFT_PATH,
                                "A fifth flatter",
                                size::DEFAULT,
                                None,
                                cx.listener(|this, _, _, cx| this.step_palette(-1, cx)),
                            )),
                    )
                    .child(key_label)
                    .child(
                        div()
                            .id("chordgen-palette-next-tip")
                            .tooltip(fb_tooltip("A fifth sharper (→)"))
                            .child(fb_icon_button(
                                "chordgen-palette-next",
                                assets::ICON_CHEVRON_RIGHT_PATH,
                                "A fifth sharper",
                                size::DEFAULT,
                                None,
                                cx.listener(|this, _, _, cx| this.step_palette(1, cx)),
                            )),
                    )
                    .child(fb_button(
                        "chordgen-palette-home",
                        "Home",
                        FbButtonKind::Ghost,
                        !self.palette_is_home(),
                        cx.listener(|this, _, _, cx| this.palette_home(cx)),
                    )),
            )
            .children(mode)
            .child(div().flex_1())
            .child(legend(true, "In song key"))
            .child(legend(false, "Outside"));

        // Each column is headed by its degree in the palette's key; once the
        // palette has left the song key, the degree in the song key follows
        // underneath, so a borrowed column reads at a glance.
        let away = !self.palette_is_home();
        let song_tonic = self.settings.tonic;
        let column_head = div()
            .flex()
            .flex_row()
            .gap(px(space::TIGHT))
            .child(div().flex_none().w(px(LABEL_W)).when(away, |label| {
                label
                    .flex()
                    .flex_col()
                    .justify_end()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_faint())
                    .child(format!("in {}", pitch_name(song_tonic, self.flats())))
            }))
            .children(chords.columns.iter().map(|column| {
                let heading = if scale.is_heptatonic() {
                    column.roman(tonic)
                } else {
                    pitch_name(column.root, scale.prefers_flats(tonic)).to_string()
                };
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_secondary())
                            .child(heading),
                    )
                    .when(away, |cell| {
                        cell.child(
                            div()
                                .text_size(px(typography::DENSE_CAPTION))
                                .text_color(Colors::text_faint())
                                .child(column.roman(song_tonic)),
                        )
                    })
            }));

        let rows = chords.rows.iter().enumerate().map(|(row_index, row)| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .flex_none()
                        .w(px(LABEL_W))
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .truncate()
                        .child(row.label),
                )
                .children(row.cells.iter().enumerate().map(|(column, cell)| {
                    // The column heading names a chord's degree; a tile adds a
                    // numeral (in the song key) only for the rows whose
                    // chords stand on another degree than their column's.
                    let numeral = match row.label {
                        "V7 of" => Some(format!("V7/{}", chords.columns[column].roman(song_tonic))),
                        "Parallel" => cell.map(|c| c.roman(song_tonic)),
                        _ => None,
                    };
                    self.palette_tile(*cell, numeral, row_index * 16 + column, cx)
                }))
        });

        div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .p(px(space::BASE))
                    .rounded(px(radius::SURFACE))
                    .bg(Colors::surface_canvas())
                    .border(px(1.0))
                    .border_color(Colors::border_subtle())
                    .child(column_head)
                    .child(
                        div()
                            .id("chordgen-palette-rows")
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(space::TIGHT))
                            .children(rows),
                    ),
            )
    }

    fn palette_tile(
        &self,
        cell: Option<Chord>,
        numeral: Option<String>,
        key: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(chord) = cell else {
            return div()
                .flex_1()
                .min_w(px(0.0))
                .h(px(TILE_H))
                .into_any_element();
        };
        let color = family_color(&chord);
        let in_key = self.in_song_key(&chord);
        let picked = self.palette_pick == Some(chord);
        let pressed = self
            .press
            .is_some_and(|p| p.kind == PressKind::Palette(chord));
        let rest = Colors::composite(Colors::surface_panel(), Colors::with_alpha(color, 0.12));
        let fill = if picked || pressed {
            Colors::composite(rest, Colors::state_selected())
        } else {
            rest
        };
        let hover = Colors::composite(fill, Colors::state_hover());
        div()
            .id(("chordgen-palette-chord", key))
            .flex_1()
            .min_w(px(0.0))
            .h(px(TILE_H))
            .px(px(space::SNUG))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .rounded(px(radius::CONTROL))
            .bg(fill)
            .border(px(1.0))
            .border_color(if picked {
                Colors::with_alpha(Colors::accent_primary(), 0.9)
            } else {
                Colors::with_alpha(color, 0.35)
            })
            .hover(move |s| s.bg(hover))
            .cursor(gpui::CursorStyle::OpenHand)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if event.click_count >= 2 {
                        this.press = None;
                        let at = this.add_destination();
                        this.insert_palette_chord(chord, at, cx);
                        return;
                    }
                    this.begin_press(
                        PressKind::Palette(chord),
                        event.position,
                        event.modifiers,
                        cx,
                    );
                }),
            )
            .child(key_dot(in_key, color))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.chord_name(&chord)),
            )
            .children(numeral.map(|numeral| {
                div()
                    .flex_none()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_muted())
                    .whitespace_nowrap()
                    .child(numeral)
            }))
            .into_any_element()
    }
}
