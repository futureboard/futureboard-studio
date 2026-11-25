//! The right column: the pitch wheel showing the chord in focus, and the
//! properties of whatever is selected.

use std::f32::consts::TAU;

use gpui::{div, px, AnyElement, Context, IntoElement, ParentElement, Styled, Window};
use gpui::{InteractiveElement, StatefulInteractiveElement};
use sphere_midi_service::chords::{alternatives, pitch_name, Chord};
use sphere_midi_service::performance::{BassPattern, CompPattern};

use super::model::{Lane, Pattern};
use super::{
    bar_beat, beats_label, choice_row, family_color, key_dot, list_heading, section_label,
    tabular_features, ChordGeneratorWindow, Selection, SIDE_W, WHEEL_SIZE,
};
use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::pitch_wheel::{self, fifths_pitch_class, ring, WheelFrame};
use crate::theme::{space, typography, Colors};

impl ChordGeneratorWindow {
    pub(super) fn side_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let wheel = self.wheel(window, cx);
        let body = match self.selection.clone() {
            Selection::Chords(ids) if ids.len() == 1 => self.chord_properties(ids[0], cx),
            Selection::Chords(ids) => self.many_chords(ids.len(), cx),
            Selection::Section(id) => self.section_properties(id, cx),
            Selection::Region(id) => self.region_properties(id, cx),
            Selection::None => self.nothing_selected(cx),
        };
        div()
            .flex_none()
            .w(px(SIDE_W))
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(div().flex().justify_center().child(wheel))
            .children(body)
    }

    fn wheel(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale_pcs = self.settings.scale.pitch_classes(self.settings.tonic);
        let mut in_key = [false; 12];
        for pc in &scale_pcs {
            in_key[*pc as usize] = true;
        }
        let chord = self.focus_chord();
        let frame = WheelFrame {
            size: WHEEL_SIZE,
            scale: window.scale_factor(),
            energy: self.shown_energy,
            in_key,
            tonic: chord.map(|c| c.root as usize),
            sweep: None,
            beat_phase: None,
            bar_phase: 0.0,
        };
        let painted = pitch_wheel::render_wgpu(&frame, cx)
            .unwrap_or_else(|| pitch_wheel::render_gpui(&frame));
        let spelling: [bool; 12] = std::array::from_fn(|pc| self.pc_flats(pc as u8));
        let radius = WHEEL_SIZE * 0.5;
        let tonic = self.settings.tonic as usize;
        let labels = (0..12).map(move |position| {
            let pc = fifths_pitch_class(position);
            let flats = spelling[pc];
            let angle = (position as f32 + 0.5) * TAU / 12.0;
            let r = radius * ring::LABEL;
            let x = radius + r * angle.sin();
            let y = radius - r * angle.cos();
            div()
                .absolute()
                .left(px(x - 12.0))
                .top(px(y - 7.0))
                .w(px(24.0))
                .h(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(if pc == tonic {
                    gpui::FontWeight::BOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .text_color(if in_key[pc] {
                    Colors::text_secondary()
                } else {
                    Colors::text_faint()
                })
                .child(pitch_name(pc as u8, flats))
        });
        let disc = WHEEL_SIZE * ring::DISC;
        let centre = div()
            .absolute()
            .left(px(radius - disc))
            .top(px(radius - disc))
            .size(px(disc * 2.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .children(chord.map(|c| {
                div()
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.chord_name(&c))
            }))
            .children(chord.map(|c| {
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::accent_primary())
                    .child(c.roman(self.settings.tonic))
            }));
        div()
            .relative()
            .flex_none()
            .size(px(WHEEL_SIZE))
            .child(painted)
            .children(labels)
            .child(centre)
    }

    /// Notes, family and key of `chord`.
    fn chord_facts(&self, chord: &Chord) -> Vec<AnyElement> {
        let notes = chord
            .pitch_classes()
            .into_iter()
            .map(|pc| pitch_name(pc, self.pc_flats(pc)))
            .collect::<Vec<_>>()
            .join(" · ");
        let in_key = self.in_song_key(chord);
        vec![
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(format!("{} — {}", chord.quality.label(), notes))
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(key_dot(in_key, family_color(chord)))
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(if in_key {
                            "Every note in the song key"
                        } else {
                            "Some notes outside the song key"
                        }),
                )
                .into_any_element(),
        ]
    }

    fn chord_properties(&self, id: u64, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(placed) = self.arrangement.find_slot(id) else {
            return Vec::new();
        };
        let slot = placed.slot;
        let chord = slot.chord;
        let tonic = self.settings.tonic;
        let mut body = vec![div()
            .text_size(px(typography::UI_XS))
            .font_features(tabular_features())
            .text_color(Colors::text_muted())
            .child(format!(
                "Section {} · {} · {}",
                section_label(placed.section_index),
                bar_beat(placed.start),
                beats_label(slot.beats)
            ))
            .into_any_element()];
        body.extend(self.chord_facts(&chord));
        body.push(
            div()
                .flex()
                .flex_row()
                .gap(px(space::TIGHT))
                .child(fb_button(
                    "chordgen-lock",
                    if slot.locked { "Unlock" } else { "Lock" },
                    FbButtonKind::Default,
                    true,
                    cx.listener(move |this, _, _, cx| this.toggle_lock(id, cx)),
                ))
                .child(fb_button(
                    "chordgen-reroll",
                    "Re-roll",
                    FbButtonKind::Default,
                    true,
                    cx.listener(move |this, _, _, cx| this.reroll(id, cx)),
                ))
                .child(fb_button(
                    "chordgen-delete",
                    "Delete",
                    FbButtonKind::Ghost,
                    true,
                    cx.listener(|this, _, _, cx| this.delete_selection(cx)),
                ))
                .into_any_element(),
        );
        body.push(list_heading("SWAP FOR"));
        let options = alternatives(&self.settings, self.arrangement.next_chord(id));
        body.push(
            div()
                .id("chordgen-alternatives")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(options.into_iter().enumerate().map(|(i, option)| {
                    choice_row(
                        ("chordgen-alt", i),
                        self.chord_name(&option),
                        option.roman(tonic),
                        option == chord,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.swap_chord(id, option, cx)))
                }))
                .into_any_element(),
        );
        body
    }

    fn many_chords(&self, count: usize, cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            div()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(format!("{count} chords selected"))
                .into_any_element(),
            hint("Drag one of them to move them all; Alt-drag copies. Shift-click adds or removes a chord."),
            div()
                .flex()
                .flex_row()
                .child(fb_button(
                    "chordgen-delete-many",
                    "Delete",
                    FbButtonKind::Ghost,
                    true,
                    cx.listener(|this, _, _, cx| this.delete_selection(cx)),
                ))
                .into_any_element(),
        ]
    }

    fn section_properties(&self, id: u64, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let arrangement = &self.arrangement;
        let (Some(index), Some((start, length))) =
            (arrangement.section_index(id), arrangement.section_span(id))
        else {
            return Vec::new();
        };
        let section = &arrangement.sections()[index];
        let chords = section
            .slots
            .iter()
            .map(|s| self.chord_name(&s.chord))
            .collect::<Vec<_>>()
            .join("  ");
        let locked = section.slots.iter().filter(|s| s.locked).count();
        vec![
            div()
                .text_size(px(typography::UI_MD))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(format!("Section {}", section_label(index)))
                .into_any_element(),
            div()
                .text_size(px(typography::UI_XS))
                .font_features(tabular_features())
                .text_color(Colors::text_muted())
                .child(format!(
                    "{} · {} chords · {}",
                    bar_beat(start),
                    section.slots.len(),
                    beats_label(length)
                ))
                .into_any_element(),
            div()
                .text_size(px(typography::UI_SM))
                .text_color(Colors::text_secondary())
                .child(chords)
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .gap(px(space::TIGHT))
                .child(fb_button(
                    "chordgen-section-fill",
                    "Fill",
                    FbButtonKind::Default,
                    true,
                    cx.listener(|this, _, _, cx| this.fill(cx)),
                ))
                .child(fb_button(
                    "chordgen-section-duplicate",
                    "Duplicate",
                    FbButtonKind::Default,
                    true,
                    cx.listener(move |this, _, _, cx| this.duplicate_section(id, cx)),
                ))
                .child(fb_button(
                    "chordgen-section-delete",
                    "Delete",
                    FbButtonKind::Ghost,
                    true,
                    cx.listener(|this, _, _, cx| this.delete_selection(cx)),
                ))
                .into_any_element(),
            hint(&if locked > 0 {
                format!(
                    "Fill rewrites this section in the chosen style and keeps its {locked} locked {}.",
                    if locked == 1 { "chord" } else { "chords" }
                )
            } else {
                "Fill rewrites this section in the chosen style; lock a chord to keep it."
                    .to_string()
            }),
        ]
    }

    fn region_properties(&self, id: u64, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(region) = self.arrangement.find_region(id).copied() else {
            return Vec::new();
        };
        let patterns: Vec<Pattern> = match region.lane() {
            Lane::Comp => CompPattern::ALL.iter().map(|p| Pattern::Comp(*p)).collect(),
            Lane::Bass => BassPattern::ALL.iter().map(|p| Pattern::Bass(*p)).collect(),
        };
        let heading = match region.lane() {
            Lane::Comp => "COMP PATTERN",
            Lane::Bass => "BASS PATTERN",
        };
        vec![
            div()
                .text_size(px(typography::UI_XS))
                .font_features(tabular_features())
                .text_color(Colors::text_muted())
                .child(format!(
                    "{} · {}",
                    bar_beat(region.start),
                    beats_label(region.length)
                ))
                .into_any_element(),
            list_heading(heading),
            div()
                .id("chordgen-pattern-choices")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(patterns.into_iter().enumerate().map(|(i, pattern)| {
                    choice_row(
                        ("chordgen-pattern-pick", i),
                        pattern.label().to_string(),
                        String::new(),
                        pattern == region.pattern,
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_region_pattern(id, pattern, cx)),
                    )
                }))
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .child(fb_button(
                    "chordgen-delete-pattern",
                    "Delete",
                    FbButtonKind::Ghost,
                    true,
                    cx.listener(|this, _, _, cx| this.delete_selection(cx)),
                ))
                .into_any_element(),
        ]
    }

    fn nothing_selected(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(chord) = self.palette_pick else {
            return vec![hint(
                "Click a chord in the palette to hear it, double-click to add it, or drag it \
                 into the sequence. Select a chord, section or pattern to edit it.",
            )];
        };
        let mut body = vec![div()
            .text_size(px(typography::UI_MD))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_primary())
            .child(self.chord_name(&chord))
            .into_any_element()];
        body.extend(self.chord_facts(&chord));
        body.push(
            div()
                .flex()
                .flex_row()
                .child(fb_button(
                    "chordgen-add-pick",
                    "Add to sequence",
                    FbButtonKind::Default,
                    true,
                    cx.listener(move |this, _, _, cx| {
                        let at = this.add_destination();
                        this.insert_palette_chord(chord, at, cx);
                    }),
                ))
                .into_any_element(),
        );
        body
    }
}

fn hint(text: &str) -> AnyElement {
    div()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_faint())
        .child(text.to_string())
        .into_any_element()
}
