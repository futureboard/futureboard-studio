//! The MIDI editor's chord readout: the chord the notes spell, live.
//!
//! While the transport plays it names the notes sounding under the playhead;
//! stopped, it names the selected notes, or with none selected the notes under
//! the parked playhead. The naming is `Chord::from_pitches`.
//!
//! Like the playhead, it is its own entity: the label changes on playback ticks,
//! and rebuilding the editor for a word in its footer is the cost the playhead
//! overlay exists to avoid. The roll writes the label into a shared cell when
//! it renders, and the playback poll writes it and notifies this view only when
//! the chord actually changed.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{div, px, Context, IntoElement, ParentElement, Render, SharedString, Styled, Window};

use crate::theme::Colors;

/// The chord name shown, shared between the roll that works it out and the
/// readout that draws it. `None` when the notes spell no chord.
pub type ChordLabelCell = Rc<RefCell<Option<SharedString>>>;

pub struct PianoRollChordReadout {
    label: ChordLabelCell,
}

impl PianoRollChordReadout {
    pub fn new(label: ChordLabelCell) -> Self {
        Self { label }
    }
}

impl Render for PianoRollChordReadout {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.label.borrow().clone();
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .gap(px(crate::theme::space::TIGHT))
            .child("Chord")
            .child(
                div()
                    .min_w(px(44.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if label.is_some() {
                        Colors::text_primary()
                    } else {
                        Colors::text_faint()
                    })
                    .child(label.unwrap_or_else(|| SharedString::new_static("—"))),
            )
    }
}

/// The chord `pitches` spell, as the readout writes it. Sharps, like the
/// editor's own note names (`D#4`).
pub fn chord_label(pitches: &[u8]) -> Option<SharedString> {
    sphere_midi_service::chords::Chord::from_pitches(pitches)
        .map(|chord| SharedString::from(chord.name(false)))
}
