//! The Slicer's editor, on the plate: the sample in the well cut into
//! slices — each tinted in turn and numbered on a flag at its start, the one
//! last played picked out, a sounding slice lit with its playhead — with how
//! it is cut (at its hits, on its beat grid, or into equal lengths) on the
//! strip under it; then a Play card (the first slice's key, transpose, fine
//! tune, bend range, choke, one-shot, reverse, normalize) and the voice's
//! Filter, Amp envelope and Output cards on the deck; and the keyboard strip
//! with each slice's key numbered and the keys without one shaded.
//!
//! Click a slice to hear it, drag a flag to move its slice, double-click to
//! cut a new one, right-click for a menu, Delete to join the picked slice to
//! the one before: the gestures live in
//! [`crate::components::slicer_window::SlicerEditorWindow`], which this
//! only renders for.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder, px, AnyElement, App, Bounds, InteractiveElement,
    IntoElement, ParentElement, Pixels, Rgba, Styled, Window,
};
use quicksampler::QuickSamplerParams;
use slicer::{BeatDivision, SliceMode, SlicerParams, MAX_SLICES};

use crate::components::instrument_plate::{self, display_colors, Keyboard, Step, WELL_TOP};
use crate::components::plate::{self, card, card_title, color, words, FAINT, WHITE};
use crate::components::quick_sampler_panel::{
    paint_display_frame, paint_waveform, rect, DisplayColors, FLAG_H,
};
use crate::components::sampler_plate::{
    self, action, bend_stepper, display_box, empty_drop_zone, fine_knob, header, inline_stepper,
    paint_flag_text, paint_ruler_labels, pair, status_banner, strip, strip_caption,
    transpose_stepper, well_body, SetVoice, WELL_H,
};
use crate::components::slicer_panel::{
    SlicerCallbacks, SlicerPanelState, SoundingSlice, SLICER_TITLE, SLICE_FLAG_W,
};
use crate::components::soundfont_player_mdi::note_label;

/// The Slicer's accent: coral.
pub const ACCENT: u32 = 0xff7a59;

/// One complete params value: sent as it is, or cut again with.
pub type SetSlicer = Rc<dyn Fn(SlicerParams, &mut App)>;

fn edited(p: SlicerParams, apply: impl FnOnce(&mut SlicerParams)) -> SlicerParams {
    let mut next = p;
    apply(&mut next);
    next.sanitized()
}

/// The whole editor at plate scale `s`. `display_bounds` is filled in by
/// the display's paint so the window can turn a pointer into a position in
/// the sample and find the flag under it. `set` sends params as they are;
/// `reslice` takes changed slicing settings and cuts the sample again.
pub fn slicer_plate(
    panel: &SlicerPanelState,
    cb: SlicerCallbacks,
    set: SetSlicer,
    reslice: SetSlicer,
    display_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let accent = color(ACCENT);
    let p = panel.params;
    // The voice cards edit `voice`; this folds their edit back in.
    let set_voice: SetVoice = {
        let set = set.clone();
        Rc::new(move |voice: QuickSamplerParams, cx: &mut App| {
            let mut next = p;
            next.voice = voice;
            set(next, cx)
        })
    };
    let browse = cb.on_browse.clone();
    let well = plate::well(s, WELL_TOP, WELL_H)
        .child(header(
            SLICER_TITLE,
            panel.sample.as_ref(),
            panel.loading,
            accent,
            Rc::new(move |window: &mut Window, cx: &mut App| browse(window, cx)),
            s,
        ))
        .child(well_body(
            slice_display(panel, &cb, display_bounds, accent, s),
            vec![slicing_strip(panel, &reslice, accent, s), hint(s)],
            s,
        ));
    sampler_plate::plate_root(
        s,
        accent,
        "Drop to load and slice",
        vec![
            well.into_any_element(),
            sampler_plate::deck(
                play_card(p, &set, &set_voice, accent, s),
                p.voice,
                &set_voice,
                accent,
                s,
            ),
            keyboard(panel, &cb, accent, s),
        ],
    )
}

// ── The display ────────────────────────────────────────────────────────────

fn slice_display(
    panel: &SlicerPanelState,
    cb: &SlicerCallbacks,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
    accent: Rgba,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let has_sample = panel.sample.is_some();
    let selected = panel.selected;
    let seconds = panel.sample.as_ref().map_or(0.0, |sample| sample.seconds);
    let peaks = panel.sample.as_ref().map(|sample| sample.peaks.clone());
    let sounding = panel.sounding.clone();
    let colors = display_colors(accent);
    let press = cb.on_display_press.clone();
    let right_press = cb.on_display_right_press.clone();
    display_box("slicer-display")
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::PointingHand)
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                    press(&(x, y, event.click_count), window, cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                    right_press(&(x, y), window, cx)
                })
        })
        .child(
            canvas(
                move |bounds, _, _| bounds_out.set(Some(bounds)),
                move |bounds, _, window, cx| {
                    paint_slices(
                        window,
                        cx,
                        bounds,
                        peaks.as_deref(),
                        &p,
                        selected,
                        &sounding,
                        seconds,
                        &colors,
                    )
                },
            )
            .absolute()
            .inset_0(),
        )
        .when(!has_sample, |display| {
            display.child(empty_drop_zone(
                panel.loading,
                "or use Load Sample\u{2026} above \u{2014} it is cut into slices at once",
                s,
            ))
        })
        .when_some(panel.status.clone(), |display, status| {
            display.child(status_banner(status, s))
        })
        .into_any_element()
}

/// Every other slice tinted so neighbours read apart, the picked one
/// stronger, a sounding one lit; before the first slice dimmed; a line and
/// a numbered flag at each slice's start (the flag sits right of its line,
/// as [`crate::components::slicer_panel::point_under`] finds it), and each
/// sounding slice's playhead.
#[allow(clippy::too_many_arguments)]
fn paint_slices(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    peaks: Option<&Vec<(f32, f32)>>,
    p: &SlicerParams,
    selected: Option<usize>,
    sounding: &[SoundingSlice],
    seconds: f64,
    c: &DisplayColors,
) {
    if peaks.is_none_or(|peaks| peaks.is_empty()) {
        // Empty, the drop zone stands alone on the ground.
        window.paint_quad(gpui::fill(bounds, c.background));
        return;
    }
    let frame = paint_display_frame(window, bounds, seconds, c);
    let (y0, wave_h) = (frame.y0, frame.wave_h);
    let points = p.points();
    let tint = |alpha: f32| Rgba {
        a: alpha,
        ..c.loop_marker
    };

    for index in 0..points.len() {
        let Some((start, end)) = p.slice_region(index) else {
            continue;
        };
        let fill = if selected == Some(index) {
            Some(tint(0.2))
        } else if index % 2 == 1 {
            Some(c.loop_band)
        } else {
            None
        };
        if let Some(fill) = fill {
            let (from, to) = (frame.at(start), frame.at(end));
            window.paint_quad(gpui::fill(rect(from, y0, to - from, wave_h), fill));
        }
    }
    for slice in sounding {
        let Some((start, end)) = p.slice_for_note(slice.note).and_then(|i| p.slice_region(i))
        else {
            continue;
        };
        let (from, to) = (frame.at(start), frame.at(end));
        window.paint_quad(gpui::fill(
            rect(from, y0, to - from, wave_h),
            tint(0.08 + 0.24 * slice.flash),
        ));
    }

    if !paint_waveform(window, &frame, peaks, c) {
        return;
    }

    if let Some(first) = points.first() {
        let x = frame.at(*first);
        window.paint_quad(gpui::fill(
            rect(frame.x0, y0, x - frame.x0, wave_h),
            c.skipped,
        ));
    }

    let on_flag = color(plate::WELL);
    for (index, point) in points.iter().enumerate() {
        let ink = if selected == Some(index) {
            c.loop_marker
        } else {
            c.marker
        };
        let x = frame.at(*point).clamp(frame.x0, frame.x0 + frame.w - 1.5);
        window.paint_quad(gpui::fill(rect(x, y0, 1.5, wave_h), ink));
        window.paint_quad(gpui::fill(rect(x, y0, SLICE_FLAG_W, FLAG_H), ink).corner_radii(px(2.0)));
        paint_flag_text(
            window,
            cx,
            &(index + 1).to_string(),
            (x, y0),
            SLICE_FLAG_W,
            on_flag,
        );
    }

    for slice in sounding {
        let x = frame
            .at(slice.position)
            .clamp(frame.x0, frame.x0 + frame.w - 2.0);
        window.paint_quad(gpui::fill(rect(x - 0.5, y0, 2.0, wave_h), c.loop_marker));
    }
    paint_ruler_labels(window, cx, &frame, seconds);
}

// ── The strip ──────────────────────────────────────────────────────────────

/// A step handler that cuts again with `p` changed by `apply`.
fn reslice_step(
    p: SlicerParams,
    reslice: &SetSlicer,
    apply: impl Fn(&mut SlicerParams, i32) + 'static,
) -> Step {
    let reslice = reslice.clone();
    Rc::new(move |delta: i32, _: &mut Window, cx: &mut App| {
        reslice(edited(p, |p| apply(p, delta)), cx)
    })
}

/// How the sample is cut: the mode, its one setting, a button that cuts it
/// again (after hand edits, say), and what the cut made.
fn slicing_strip(
    panel: &SlicerPanelState,
    reslice: &SetSlicer,
    accent: Rgba,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let enabled = panel.is_playable() && !panel.analysing;
    let modes: Vec<&str> = SliceMode::ALL.iter().map(|mode| mode.label()).collect();
    let on_mode = reslice_step(p, reslice, |p, index| {
        if let Some(mode) = SliceMode::ALL.get(index as usize) {
            p.slice_mode = *mode;
        }
    });
    let mut row = strip(s)
        .child(strip_caption("Slice by", s))
        .child(instrument_plate::choices(
            "slicer-mode",
            &modes,
            SliceMode::ALL.iter().position(|mode| *mode == p.slice_mode),
            accent,
            None,
            true,
            s,
            on_mode,
        ))
        .child(div().w(px(4.0 * s)));
    row = match p.slice_mode {
        SliceMode::Transient => row
            .child(strip_caption("Sensitivity", s))
            .child(inline_stepper(
                "slicer-sensitivity",
                format!("{:.0}%", p.sensitivity * 100.0),
                44.0,
                s,
                reslice_step(p, reslice, |p, delta| p.sensitivity += 0.05 * delta as f32),
            )),
        SliceMode::Beat => {
            let divisions: Vec<&str> = BeatDivision::ALL.iter().map(|d| d.label()).collect();
            let bpm = if p.sample_bpm > 0.0 {
                format!("{:.1}", p.sample_bpm)
            } else {
                "\u{2014}".to_string()
            };
            let scale = |id: &'static str, label: &'static str, factor: f32| {
                let reslice = reslice.clone();
                action(
                    id,
                    label,
                    true,
                    accent,
                    s,
                    Rc::new(move |_: &mut Window, cx: &mut App| {
                        reslice(edited(p, |p| p.sample_bpm *= factor), cx)
                    }),
                )
            };
            row.child(strip_caption("Tempo", s))
                .child(inline_stepper(
                    "slicer-bpm",
                    bpm,
                    48.0,
                    s,
                    reslice_step(p, reslice, |p, delta| {
                        p.sample_bpm = (p.sample_bpm + delta as f32).max(slicer::MIN_SAMPLE_BPM)
                    }),
                ))
                .child(scale("slicer-bpm-half", "\u{00F7}2", 0.5))
                .child(scale("slicer-bpm-double", "\u{00D7}2", 2.0))
                .child(instrument_plate::choices(
                    "slicer-division",
                    &divisions,
                    BeatDivision::ALL
                        .iter()
                        .position(|division| *division == p.beat_division),
                    accent,
                    None,
                    true,
                    s,
                    reslice_step(p, reslice, |p, index| {
                        if let Some(division) = BeatDivision::ALL.get(index as usize) {
                            p.beat_division = *division;
                        }
                    }),
                ))
        }
        SliceMode::Equal => row.child(strip_caption("Slices", s)).child(inline_stepper(
            "slicer-equal",
            p.equal_slices.to_string(),
            32.0,
            s,
            reslice_step(p, reslice, |p, delta| {
                p.equal_slices = (p.equal_slices as i32 + delta).clamp(0, 255) as u8
            }),
        )),
    };
    let again = {
        let reslice = reslice.clone();
        Rc::new(move |_: &mut Window, cx: &mut App| reslice(p, cx))
    };
    row.child(action(
        "slicer-again",
        "Slice Again",
        enabled,
        accent,
        s,
        again,
    ))
    .child(div().flex_1())
    .child(plate::numbers(summary(panel), 11.5, WHITE, s))
    // Before a sample is in, the settings can still be chosen for it.
    .opacity(if enabled { 1.0 } else { 0.48 })
    .into_any_element()
}

/// What the cut made: `8 slices · C3 – G3`.
fn summary(panel: &SlicerPanelState) -> String {
    let p = panel.params;
    let count = p.slice_count as usize;
    if panel.analysing {
        "Slicing\u{2026}".to_string()
    } else if count == 0 {
        "No slices".to_string()
    } else {
        let last = p
            .note_for_slice(count - 1)
            .map(note_label)
            .unwrap_or_else(|| "\u{2014}".to_string());
        format!(
            "{count} slice{} \u{00B7} {} \u{2013} {last}",
            if count == 1 { "" } else { "s" },
            note_label(p.first_key)
        )
    }
}

/// The display's gestures, under the strip.
fn hint(s: f32) -> AnyElement {
    div()
        .flex_shrink_0()
        .h(px(14.0 * s))
        .overflow_hidden()
        .child(words(
            format!(
                "Click a slice to hear it \u{00B7} drag a flag to move it \u{00B7} double-click \
                 to cut \u{00B7} right-click for more (play, split, join) \u{00B7} Delete joins \
                 the picked slice to the one before \u{00B7} up to {MAX_SLICES} slices"
            ),
            10.5,
            FAINT,
            s,
        ))
        .into_any_element()
}

// ── The deck ───────────────────────────────────────────────────────────────

/// Play: which key starts the slices and how they meet, and the voice's
/// pitch — no root key or key tracking, every slice plays at the sample's
/// own pitch on its key.
fn play_card(
    p: SlicerParams,
    set: &SetSlicer,
    set_voice: &SetVoice,
    accent: Rgba,
    s: f32,
) -> gpui::Div {
    let first_key = {
        let set = set.clone();
        instrument_plate::stepper(
            "slicer-first-key",
            "First key",
            note_label(p.first_key),
            true,
            s,
            Rc::new(move |delta: i32, _: &mut Window, cx: &mut App| {
                set(
                    edited(p, |p| {
                        p.first_key = (p.first_key as i32 + delta).clamp(0, 127) as u8
                    }),
                    cx,
                )
            }),
        )
    };
    let toggle = |id: &'static str, label: &'static str, on: bool, flip: fn(&mut SlicerParams)| {
        let set = set.clone();
        instrument_plate::toggle(
            id,
            label,
            on,
            accent,
            true,
            s,
            Rc::new(move |_: &mut Window, cx: &mut App| set(edited(p, flip), cx)),
        )
        .flex_1()
    };
    let voice = p.voice;
    card(s)
        .child(card_title("Play", s))
        .child(pair(first_key, transpose_stepper(voice, set_voice, s)))
        .child(pair(
            fine_knob(voice, set_voice, accent, s),
            bend_stepper(voice, set_voice, s),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0 * s))
                        .child(toggle("slicer-choke", "Choke", p.choke, |p| {
                            p.choke = !p.choke
                        }))
                        .child(toggle("slicer-one-shot", "One-shot", p.one_shot, |p| {
                            p.one_shot = !p.one_shot
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0 * s))
                        .child(toggle("slicer-reverse", "Reverse", voice.reverse, |p| {
                            p.voice.reverse = !p.voice.reverse
                        }))
                        .child(toggle(
                            "slicer-normalize",
                            "Normalize",
                            voice.normalize,
                            |p| p.voice.normalize = !p.voice.normalize,
                        )),
                ),
        )
}

// ── The keyboard ───────────────────────────────────────────────────────────

fn keyboard(panel: &SlicerPanelState, cb: &SlicerCallbacks, accent: Rgba, s: f32) -> AnyElement {
    let p = panel.params;
    let mut lit = panel.active_notes.clone();
    lit.extend(panel.sounding.iter().map(|slice| slice.note));
    let on = cb.on_note_on.clone();
    let off = cb.on_note_off.clone();
    let shift = cb.on_shift_octave.clone();
    instrument_plate::keyboard(
        Keyboard {
            id: "slicer-key",
            root: panel.keyboard_root,
            active: &lit,
            playable: panel.is_playable(),
            mark: &|pitch| p.slice_for_note(pitch).map(|index| (index + 1).to_string()),
            mapped: Some(&|pitch| p.slice_for_note(pitch).is_some()),
            hint: format!("Slices play from {}, one per key", note_label(p.first_key)),
            accent,
            note_on: Rc::new(move |pitch: u8, window: &mut Window, cx: &mut App| {
                on(&pitch, window, cx)
            }),
            note_off: Rc::new(move |pitch: u8, window: &mut Window, cx: &mut App| {
                off(&pitch, window, cx)
            }),
            on_shift: Rc::new(move |delta: i32, window: &mut Window, cx: &mut App| {
                shift(&delta, window, cx)
            }),
        },
        s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::slicer_panel::point_under;

    #[test]
    fn a_press_anywhere_on_a_painted_flag_finds_its_slice() {
        let p = slicer::slicing::with_points(SlicerParams::default(), &[0.0, 0.5], 0.0);
        // The flag is painted from its line to SLICE_FLAG_W right of it,
        // FLAG_H down: the hit-test covers all of it.
        let line = 500.0;
        for x in [line, line + SLICE_FLAG_W * 0.5, line + SLICE_FLAG_W - 0.5] {
            for y in [0.5, FLAG_H - 0.5] {
                assert_eq!(point_under(&p, x, y, 1_000.0), Some(1), "{x}, {y}");
            }
        }
    }

    #[test]
    fn the_summary_names_the_keys_the_slices_take() {
        let mut panel = SlicerPanelState::default();
        panel.params =
            slicer::slicing::with_points(panel.params, &slicer::slicing::equal_points(8), 0.0);
        assert_eq!(summary(&panel), "8 slices \u{00B7} C3 \u{2013} G3");
        panel.analysing = true;
        assert_eq!(summary(&panel), "Slicing\u{2026}");
    }
}
