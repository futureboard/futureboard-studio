//! Quick Sampler's editor, on the plate: the sample in the well — its play
//! region, its loop and a flag on each marker to drag, with how the region
//! plays (loop mode, reverse, normalize) and where its markers sit on the
//! strip under it — then Pitch, Filter, Amp envelope and Output cards on the
//! deck, and the keyboard strip with the root key marked.
//!
//! The state and every gesture live in
//! [`crate::components::quick_sampler_window::QuickSamplerEditorWindow`];
//! this only renders, handing each edit to `set` as one whole params value.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder, px, AnyElement, App, Bounds, InteractiveElement,
    IntoElement, ParentElement, Pixels, Rgba, Styled, Window,
};
use quicksampler::{LoopMode, QuickSamplerParams};

use crate::components::instrument_plate::{self, display_colors, Keyboard, WELL_TOP};
use crate::components::plate::{self, card, card_title, color, SOFT};
use crate::components::plate_parts::{note, switch_row};
use crate::components::quick_sampler_panel::{
    format_seconds, paint_display_frame, paint_waveform, rect, with, DisplayColors,
    QuickSamplerCallbacks, QuickSamplerPanelState, FLAG_H, FLAG_W, QUICK_SAMPLER_TITLE,
};
use crate::components::sampler_plate::{
    self, bend_stepper, display_box, empty_drop_zone, fine_knob, header, paint_flag_text,
    paint_ruler_labels, pair, reading, status_banner, strip, strip_caption, transpose_stepper,
    voice_stepper, well_body, SetVoice, WELL_H,
};
use crate::components::soundfont_player_mdi::note_label;

/// Quick Sampler's accent: amber.
pub const ACCENT: u32 = 0xf2c14e;

/// One complete params value, from any control on the plate.
pub type SetParams = SetVoice;

/// The whole editor at plate scale `s`. `waveform_bounds` is filled in by
/// the display's paint so the window can turn a pointer x into a position
/// in the sample.
pub fn quick_sampler_plate(
    panel: &QuickSamplerPanelState,
    cb: QuickSamplerCallbacks,
    set: SetParams,
    waveform_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let accent = color(ACCENT);
    let p = panel.params;
    let browse = cb.on_browse.clone();
    let well = plate::well(s, WELL_TOP, WELL_H)
        .child(header(
            QUICK_SAMPLER_TITLE,
            panel.sample.as_ref(),
            panel.loading,
            accent,
            Rc::new(move |window: &mut Window, cx: &mut App| browse(window, cx)),
            s,
        ))
        .child(well_body(
            sample_display(panel, &cb, waveform_bounds, accent, s),
            vec![sample_strip(panel, &set, accent, s)],
            s,
        ));
    sampler_plate::plate_root(
        s,
        accent,
        "Drop to load the sample",
        vec![
            well.into_any_element(),
            sampler_plate::deck(pitch_card(p, &set, accent, s), p, &set, accent, s),
            keyboard(panel, &cb, accent, s),
        ],
    )
}

// ── The display ────────────────────────────────────────────────────────────

fn sample_display(
    panel: &QuickSamplerPanelState,
    cb: &QuickSamplerCallbacks,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
    accent: Rgba,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let has_sample = panel.sample.is_some();
    let looping = p.loop_mode != LoopMode::Off;
    let seconds = panel.sample.as_ref().map_or(0.0, |sample| sample.seconds);
    let peaks = panel.sample.as_ref().map(|sample| sample.peaks.clone());
    let colors = display_colors(accent);
    let press = cb.on_waveform_press.clone();
    let menu = cb.on_waveform_menu.clone();
    display_box("quick-sampler-display")
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    press(&f32::from(event.position.x), window, cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                    menu(
                        &(f32::from(event.position.x), f32::from(event.position.y)),
                        window,
                        cx,
                    )
                })
        })
        .child(
            canvas(
                move |bounds, _, _| bounds_out.set(Some(bounds)),
                move |bounds, _, window, cx| {
                    paint_sample(
                        window,
                        cx,
                        bounds,
                        peaks.as_deref(),
                        &p,
                        looping,
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
                "or use Load Sample\u{2026} above \u{2014} it plays across the keys",
                s,
            ))
        })
        .when_some(panel.status.clone(), |display, status| {
            display.child(status_banner(status, s))
        })
        .into_any_element()
}

/// The sample, its loop banded, what the region skips dimmed, and a line
/// and lettered flag on each marker: start and end hang from the top, the
/// loop's stand on the ruler, each flag on the inside of its line.
#[allow(clippy::too_many_arguments)]
fn paint_sample(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    peaks: Option<&Vec<(f32, f32)>>,
    p: &QuickSamplerParams,
    looping: bool,
    seconds: f64,
    c: &DisplayColors,
) {
    if peaks.is_none_or(|peaks| peaks.is_empty()) {
        // Empty, the drop zone stands alone on the ground.
        window.paint_quad(gpui::fill(bounds, c.background));
        return;
    }
    let frame = paint_display_frame(window, bounds, seconds, c);
    let (x0, y0, w, wave_h) = (frame.x0, frame.y0, frame.w, frame.wave_h);
    if looping {
        let (from, to) = (frame.at(p.loop_start), frame.at(p.loop_end));
        window.paint_quad(gpui::fill(rect(from, y0, to - from, wave_h), c.loop_band));
    }
    if !paint_waveform(window, &frame, peaks, c) {
        return;
    }
    let (start, end) = (frame.at(p.start), frame.at(p.end));
    window.paint_quad(gpui::fill(rect(x0, y0, start - x0, wave_h), c.skipped));
    window.paint_quad(gpui::fill(rect(end, y0, x0 + w - end, wave_h), c.skipped));

    let ink_on_flag = color(plate::WELL);
    let mut marker = |window: &mut Window, fraction: f32, letter: &str, ink: Rgba, top: bool| {
        let x = frame.at(fraction).clamp(x0, x0 + w - 1.5);
        window.paint_quad(gpui::fill(rect(x, y0, 1.5, wave_h), ink));
        let flag_x = if fraction > 0.5 { x - FLAG_W + 1.5 } else { x };
        let flag_y = if top { y0 } else { y0 + wave_h - FLAG_H };
        window.paint_quad(
            gpui::fill(rect(flag_x, flag_y, FLAG_W, FLAG_H), ink).corner_radii(px(2.0)),
        );
        paint_flag_text(window, cx, letter, (flag_x, flag_y), FLAG_W, ink_on_flag);
    };
    marker(window, p.start, "S", c.marker, true);
    marker(window, p.end, "E", c.marker, true);
    if looping {
        marker(window, p.loop_start, "L", c.loop_marker, false);
        marker(window, p.loop_end, "L", c.loop_marker, false);
    }
    paint_ruler_labels(window, cx, &frame, seconds);
}

/// Under the display: how the region plays, and where its markers sit.
fn sample_strip(
    panel: &QuickSamplerPanelState,
    set: &SetParams,
    accent: Rgba,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let playable = panel.is_playable();
    let seconds = panel.sample.as_ref().map_or(0.0, |sample| sample.seconds);
    let labels: Vec<&str> = LoopMode::ALL.iter().map(|mode| mode.label()).collect();
    let on_loop = {
        let set = set.clone();
        Rc::new(move |index: i32, _: &mut Window, cx: &mut App| {
            if let Some(mode) = LoopMode::ALL.get(index as usize) {
                set(with(p, |p| p.loop_mode = *mode), cx);
            }
        })
    };
    let toggle =
        |id: &'static str, label: &'static str, on: bool, flip: fn(&mut QuickSamplerParams)| {
            let set = set.clone();
            instrument_plate::toggle(
                id,
                label,
                on,
                accent,
                playable,
                s,
                Rc::new(move |_: &mut Window, cx: &mut App| set(with(p, flip), cx)),
            )
        };
    let at = |fraction: f32| format_seconds(fraction as f64 * seconds);
    let looping = p.loop_mode != LoopMode::Off;
    strip(s)
        .child(strip_caption("Loop", s))
        .child(instrument_plate::choices(
            "quick-sampler-loop",
            &labels,
            LoopMode::ALL.iter().position(|mode| *mode == p.loop_mode),
            accent,
            None,
            true,
            s,
            on_loop,
        ))
        .child(div().w(px(6.0 * s)))
        .child(toggle("quick-sampler-reverse", "Reverse", p.reverse, |p| {
            p.reverse = !p.reverse
        }))
        .child(toggle(
            "quick-sampler-normalize",
            "Normalize",
            p.normalize,
            |p| p.normalize = !p.normalize,
        ))
        .child(div().flex_1())
        .when(playable, |strip| {
            strip
                .child(reading("Start", at(p.start), SOFT, s))
                .child(reading("End", at(p.end), SOFT, s))
                .when(looping, |strip| {
                    strip.child(reading(
                        "Loop",
                        format!("{} \u{2192} {}", at(p.loop_start), at(p.loop_end)),
                        ACCENT,
                        s,
                    ))
                })
        })
        .into_any_element()
}

// ── The deck ───────────────────────────────────────────────────────────────

/// Pitch: the key the sample plays as recorded, transpose, fine tune, the
/// bend range, and whether the keys change its pitch at all.
fn pitch_card(p: QuickSamplerParams, set: &SetParams, accent: Rgba, s: f32) -> gpui::Div {
    let on_keytrack = {
        let set = set.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            set(with(p, |p| p.keytrack = !p.keytrack), cx)
        }
    };
    let keytrack_note = if p.keytrack {
        "Each key plays the sample at its own pitch."
    } else {
        "Off: every key plays the sample as recorded."
    };
    card(s)
        .child(card_title("Pitch", s))
        .child(pair(
            voice_stepper(
                "quick-sampler-root",
                "Root key",
                note_label(p.root_note),
                s,
                p,
                set,
                |p, delta| p.root_note = (p.root_note as i32 + delta).clamp(0, 127) as u8,
            ),
            transpose_stepper(p, set, s),
        ))
        .child(pair(fine_knob(p, set, accent, s), bend_stepper(p, set, s)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0 * s))
                .child(switch_row(
                    "Key tracking",
                    plate::switch(
                        "quick-sampler-keytrack",
                        p.keytrack,
                        accent,
                        true,
                        s,
                        on_keytrack,
                    ),
                    s,
                ))
                .child(note(keytrack_note, s)),
        )
}

// ── The keyboard ───────────────────────────────────────────────────────────

fn keyboard(
    panel: &QuickSamplerPanelState,
    cb: &QuickSamplerCallbacks,
    accent: Rgba,
    s: f32,
) -> AnyElement {
    let root = panel.params.root_note;
    let on = cb.on_note_on.clone();
    let off = cb.on_note_off.clone();
    let shift = cb.on_shift_octave.clone();
    instrument_plate::keyboard(
        Keyboard {
            id: "quick-sampler-key",
            root: panel.keyboard_root,
            active: &panel.active_notes,
            playable: panel.is_playable(),
            mark: &|pitch| (pitch == root).then(|| "ROOT".to_string()),
            mapped: None,
            hint: format!("Root {} plays the sample as recorded", note_label(root)),
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
