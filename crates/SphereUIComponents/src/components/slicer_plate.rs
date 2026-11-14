//! The Slicer's editor, laid out as the Quick Sampler design's Slicer page:
//! the Sample section — the header with the Slices / Voice tabs, the sample
//! stepper, Load file and Bypass; the display with a numbered marker at each
//! slice's start along its top bar, the picked slice lit and a sounding one
//! glowing with its playhead; the line under it — then the keyboard with each
//! slice's key numbered and the keys without one shaded.
//!
//! The Slices page's columns: Slice by (the mode, its setting, sensitivity or
//! tempo, Re-detect), Pads (sixteen at a time), the picked slice (its own
//! reverse, gain and pitch) and Playback (one shot or gate, choke, level).
//! The Voice page shows what every slice plays through: pitch, filter, amp
//! envelope and output, as Quick Sampler draws them.
//!
//! Click a slice to hear it, drag a marker to move it, double-click to cut a
//! new one, right-click for a menu, Delete to join the picked slice to the
//! one before: the gestures live in
//! [`crate::components::slicer_window::SlicerEditorWindow`], which this
//! only renders for.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder, px, AnyElement, App, Bounds, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Pixels, Styled, Window,
};
use quicksampler::{FilterMode, QuickSamplerParams};
use slicer::{
    BeatDivision, SliceMode, SlicerParams, MAX_SAMPLE_BPM, MAX_SLICES, MIN_SAMPLE_BPM,
    SLICE_GAIN_MAX_DB, SLICE_GAIN_MIN_DB, SLICE_PITCH_RANGE,
};

use crate::components::plate::{self, color, color_a, words, Dial, DialUnit};
use crate::components::sampler_plate::{
    self, action, amp_column, bend_row, column, computer_keys_hint, display_box, empty_display,
    filter_column, fine_knob, gain_knob, heading, heading_row, help, info_line, knob, knob_row,
    output_column, paint_box, paint_wave, reading, segmented, toggle, transpose_knob, Header,
    KeyFace, SetVoice, DISPLAY_H, FACE, FACE_EDGE, MUTED, ON_FACE, ON_INK, PLAYING,
};
use crate::components::slicer_panel::{
    marker_width, SlicerCallbacks, SlicerPanelState, SoundingSlice, SLICER_TITLE, SLICE_MARKER_H,
};
use crate::components::soundfont_player_mdi::note_label;

/// One complete params value: sent as it is, or cut again with.
pub type SetSlicer = Rc<dyn Fn(SlicerParams, &mut App)>;

/// Pads shown at once.
pub const PADS_PER_PAGE: usize = 16;

/// Which page the Controls section shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlicerPage {
    #[default]
    Slices,
    Voice,
}

/// What the editor shows besides the panel state.
pub struct SlicerView<'a> {
    pub panel: &'a SlicerPanelState,
    pub page: SlicerPage,
    /// The pads' page: 0 is slices 1–16.
    pub pad_page: usize,
    pub bypassed: Option<bool>,
    pub position: Option<(usize, usize)>,
    pub filter_memory: FilterMode,
    pub computer_base: u8,
    /// A hand edit just switched the slicing to Manual: the line under the
    /// display says so.
    pub switched_to_manual: bool,
}

fn edited(p: SlicerParams, apply: impl FnOnce(&mut SlicerParams)) -> SlicerParams {
    let mut next = p;
    apply(&mut next);
    next.sanitized()
}

/// The whole editor at plate scale `s`. `display_bounds` is filled in by
/// the display's paint so the window can turn a pointer into a position in
/// the sample and find the marker under it. `set` sends params as they are;
/// `reslice` takes changed slicing settings and cuts the sample again.
pub fn slicer_plate(
    view: SlicerView<'_>,
    cb: SlicerCallbacks,
    set: SetSlicer,
    reslice: SetSlicer,
    display_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let panel = view.panel;
    let p = panel.params;
    // The Voice page edits `voice`; this folds its edit back in.
    let set_voice: SetVoice = {
        let set = set.clone();
        Rc::new(move |voice: QuickSamplerParams, cx: &mut App| {
            let mut next = p;
            next.voice = voice;
            set(next, cx)
        })
    };
    let browse = cb.on_browse.clone();
    let step = cb.on_step_sample.clone();
    let bypass = cb.on_bypass.clone();
    let page = cb.on_page.clone();
    let header = sampler_plate::header(
        Header {
            title: SLICER_TITLE,
            tabs: Some((
                &["Slices", "Voice"],
                (view.page == SlicerPage::Voice) as usize,
                Rc::new(move |index, window, cx| page(&index, window, cx)),
            )),
            sample: panel.sample.as_ref(),
            loading: panel.loading,
            position: view.position,
            on_step_sample: Rc::new(move |delta, window, cx| step(&delta, window, cx)),
            on_load: Rc::new(move |window, cx| browse(window, cx)),
            bypass: view.bypassed.map(|on| {
                (
                    on,
                    Rc::new(move |window: &mut Window, cx: &mut App| bypass(window, cx)) as _,
                )
            }),
        },
        s,
    );
    let display = slice_display(panel, &cb, view.bypassed == Some(true), display_bounds, s);
    let (right, error) = match (&panel.status, panel.loading, panel.analysing) {
        (Some(status), _, _) => (status.clone(), true),
        (None, true, _) => ("Loading\u{2026}".to_string(), false),
        (None, false, true) => ("Finding the slices\u{2026}".to_string(), false),
        (None, false, false) if view.switched_to_manual => (
            "Switched to Manual so your marker edits stay. Re-detect undoes them.".to_string(),
            false,
        ),
        (None, false, false) => (computer_keys_hint(view.computer_base), false),
    };
    let left = match panel.sample.as_ref() {
        Some(sample) => format!(
            "{:.2} s \u{00B7} {} {} from {}",
            sample.seconds,
            p.slice_count,
            if p.slice_count == 1 {
                "slice"
            } else {
                "slices"
            },
            note_label(p.first_key)
        ),
        None => "No sample".to_string(),
    };
    let info = info_line(left, right, error, s);

    let sounding_notes: Vec<u8> = panel.sounding.iter().map(|slice| slice.note).collect();
    let press = cb.on_note_on.clone();
    let release = cb.on_note_off.clone();
    let keyboard = sampler_plate::keyboard(
        s,
        panel.keyboard_root,
        |pitch| {
            let slice = p.slice_for_note(pitch);
            KeyFace {
                held: panel.active_notes.contains(&pitch) || sounding_notes.contains(&pitch),
                mapped: slice.is_some(),
                sub: slice.map(|index| (index + 1).to_string()),
            }
        },
        Rc::new(move |pitch, velocity, window, cx| press(&(pitch, velocity), window, cx)),
        Rc::new(move |pitch, window, cx| release(&pitch, window, cx)),
    );

    let columns = match view.page {
        SlicerPage::Slices => vec![
            slice_by_column(panel, &set, &reslice, s),
            pads_column(panel, view.pad_page, &cb, s),
            slice_column(panel, &set, s),
            playback_column(p, &set, &set_voice, s),
        ],
        SlicerPage::Voice => vec![
            voice_pitch_column(p, &set, &set_voice, s),
            filter_column(p.voice, view.filter_memory, &set_voice, s, 230.0),
            amp_column(
                p.voice,
                None,
                false,
                if p.one_shot {
                    "Slices play to their end; the envelope still shapes them."
                } else {
                    "Release starts when the key lets go."
                },
                &set_voice,
                s,
                490.0,
            ),
            output_column(p.voice, &set_voice, s, 790.0),
        ],
    };

    sampler_plate::plate_root(
        s,
        "Drop to load and slice",
        vec![
            sampler_plate::sample_section(s, header, display, info),
            keyboard,
            sampler_plate::controls_section(s, columns),
        ],
    )
}

// ── The display ────────────────────────────────────────────────────────────

fn slice_display(
    panel: &SlicerPanelState,
    cb: &SlicerCallbacks,
    bypassed: bool,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let has_sample = panel.sample.is_some();
    let selected = panel.selected;
    let peaks = panel.sample.as_ref().map(|sample| sample.peaks.clone());
    let sounding = panel.sounding.clone();
    let press = cb.on_display_press.clone();
    let right_press = cb.on_display_right_press.clone();
    display_box("slicer-display", s)
        .when(bypassed, |display| display.opacity(0.5))
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
                    )
                },
            )
            .size_full(),
        )
        .when(!has_sample, |display| {
            display.child(empty_display(
                panel.loading,
                "or use Load file. It is cut into slices at once, one per key.",
                s,
            ))
        })
        .when(bypassed, |display| {
            display.child(sampler_plate::bypass_notice(s))
        })
        .into_any_element()
}

/// The design's slicer display: the picked slice lit, sounding ones
/// glowing, the waveform, a dimmed stretch before the first slice, the
/// marker bar with a numbered marker and a line at each slice's start, and
/// each sounding slice's playhead.
fn paint_slices(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    peaks: Option<&Vec<(f32, f32)>>,
    p: &SlicerParams,
    selected: Option<usize>,
    sounding: &[SoundingSlice],
) {
    let Some(peaks) = peaks else {
        return;
    };
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width).max(1.0);
    let h = f32::from(bounds.size.height).max(1.0);
    let k = h / DISPLAY_H;
    let bar = SLICE_MARKER_H * k;
    let at = |fraction: f32| x0 + fraction.clamp(0.0, 1.0) * w;
    let band = |window: &mut Window, (start, end): (f32, f32), ink| {
        paint_box(
            window,
            (at(start), y0 + bar, at(end) - at(start), h - bar),
            ink,
        )
    };

    if let Some(region) = selected.and_then(|index| p.slice_region(index)) {
        band(window, region, color_a(0xffffff, 0.09));
    }
    for slice in sounding {
        if let Some(region) = p.slice_for_note(slice.note).and_then(|i| p.slice_region(i)) {
            band(window, region, color_a(PLAYING, 0.16 + 0.2 * slice.flash));
        }
    }
    paint_wave(window, bounds, peaks, y0 + 150.0 * k, 135.0 * k);
    if let Some(first) = p.points().first() {
        paint_box(
            window,
            (x0, y0, at(*first) - x0, h),
            color_a(sampler_plate::DISPLAY, 0.72),
        );
    }

    paint_box(window, (x0, y0, w, bar), color(0x1a1a1a));
    paint_box(window, (x0, y0 + bar - 1.0, w, 1.0), color(0x2a2a2a));
    for (index, point) in p.points().iter().enumerate() {
        let x = at(*point).min(x0 + w - 1.0);
        paint_box(window, (x, y0 + bar, 1.0, h - bar), color(0x8a8a8a));
        let on = selected == Some(index);
        let marker = marker_width(index) * k;
        paint_box(
            window,
            (x, y0, marker, bar - 1.0),
            color(if on { ON_FACE } else { 0x1a1a1a }),
        );
        paint_box(window, (x, y0, 1.0, bar - 1.0), color(MUTED));
        plate::paint_text(
            window,
            cx,
            &(index + 1).to_string(),
            plate::mono(FontWeight::NORMAL),
            11.0 * k,
            color(if on { ON_INK } else { MUTED }),
            (x + 4.0 * k, y0 + 3.0 * k),
            plate::Align::Left,
        );
    }
    for slice in sounding {
        let x = at(slice.position);
        paint_box(window, (x - k, y0, 2.0 * k, h), color(PLAYING));
    }
}

// ── The Slices page ────────────────────────────────────────────────────────

const EQUAL_COUNTS: [u8; 3] = [8, 16, 32];

/// Slice by: the mode, its one setting, the knob that tunes it, and
/// Re-detect for a hand-made cut.
fn slice_by_column(
    panel: &SlicerPanelState,
    set: &SetSlicer,
    reslice: &SetSlicer,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let ready = panel.is_playable() && !panel.analysing;
    let modes: Vec<&str> = SliceMode::ALL.iter().map(|mode| mode.label()).collect();
    let mode_set = set.clone();
    let mode_reslice = reslice.clone();
    let mode_row = segmented(
        "slicer-mode",
        &modes,
        SliceMode::ALL.iter().position(|mode| *mode == p.slice_mode),
        true,
        s,
        Rc::new(move |index, _, cx| {
            let Some(mode) = SliceMode::ALL.get(index).copied() else {
                return;
            };
            let next = edited(p, |p| p.slice_mode = mode);
            if mode == SliceMode::Manual {
                // Manual keeps the cut as it is.
                mode_set(next, cx);
            } else {
                mode_reslice(next, cx);
            }
        }),
    );

    let setting_reslice = reslice.clone();
    let setting_row = match p.slice_mode {
        SliceMode::Beat => {
            let labels: Vec<&str> = BeatDivision::ALL.iter().map(|d| d.label()).collect();
            segmented(
                "slicer-division",
                &labels,
                BeatDivision::ALL.iter().position(|d| *d == p.beat_division),
                true,
                s,
                Rc::new(move |index, _, cx| {
                    if let Some(division) = BeatDivision::ALL.get(index).copied() {
                        setting_reslice(edited(p, |p| p.beat_division = division), cx);
                    }
                }),
            )
        }
        mode => segmented(
            "slicer-equal",
            &["8 slices", "16 slices", "32 slices"],
            EQUAL_COUNTS
                .iter()
                .position(|count| *count == p.equal_slices),
            mode == SliceMode::Equal,
            s,
            Rc::new(move |index, _, cx| {
                if let Some(count) = EQUAL_COUNTS.get(index).copied() {
                    setting_reslice(edited(p, |p| p.equal_slices = count), cx);
                }
            }),
        ),
    };

    let capped = p.slice_count as usize >= MAX_SLICES;
    let help_text = match p.slice_mode {
        SliceMode::Transient if capped => {
            "More hits than 64 keys. Lower sensitivity to merge quiet ones.".to_string()
        }
        SliceMode::Transient => "Higher sensitivity catches quieter hits like hats.".to_string(),
        SliceMode::Beat if p.sample_bpm > 0.0 => {
            format!("Cuts on the sample's own beat at {:.1} BPM.", p.sample_bpm)
        }
        SliceMode::Beat => "No steady tempo found. Set it with the Tempo knob.".to_string(),
        SliceMode::Equal => "Cuts the sample into equal parts, ignoring the hits.".to_string(),
        SliceMode::Manual => {
            "Drag numbered markers above the wave. Double-click the wave to add one.".to_string()
        }
    };

    let knob_reslice = reslice.clone();
    let tune = if p.slice_mode == SliceMode::Beat {
        knob(
            "slicer-tempo",
            Dial::new(
                "tempo",
                "Tempo",
                MIN_SAMPLE_BPM,
                MAX_SAMPLE_BPM,
                120.0,
                DialUnit::Plain,
            ),
            if p.sample_bpm > 0.0 {
                p.sample_bpm
            } else {
                120.0
            },
            if p.sample_bpm > 0.0 {
                format!("{:.1} BPM", p.sample_bpm)
            } else {
                "\u{2014}".to_string()
            },
            ready,
            s,
            Rc::new(move |bpm, cx| knob_reslice(edited(p, |p| p.sample_bpm = bpm), cx)),
        )
    } else {
        knob(
            "slicer-sensitivity",
            Dial::new("sensitivity", "Sensitivity", 0.0, 1.0, 0.5, DialUnit::Q),
            p.sensitivity,
            format!("{:.0}%", p.sensitivity * 100.0),
            ready && p.slice_mode == SliceMode::Transient,
            s,
            Rc::new(move |value, cx| knob_reslice(edited(p, |p| p.sensitivity = value), cx)),
        )
    };
    let redetect_reslice = reslice.clone();
    let redetect = action(
        "slicer-redetect",
        "Re-detect",
        ready && p.slice_mode == SliceMode::Manual,
        s,
        Rc::new(move |_, cx| {
            redetect_reslice(edited(p, |p| p.slice_mode = SliceMode::Transient), cx)
        }),
    );

    let count = format!(
        "{} {}{}",
        p.slice_count,
        if p.slice_count == 1 {
            "slice"
        } else {
            "slices"
        },
        if capped { " (max)" } else { "" }
    );
    column(s, 0.0, 250.0, true)
        .child(heading_row(
            "Slice by",
            reading(count, plate::WHITE, s).into_any_element(),
            s,
        ))
        .child(mode_row)
        .child(setting_row)
        .child(help(help_text, s))
        .child(
            knob_row(
                vec![
                    tune,
                    div().mb(px(4.0 * s)).child(redetect).into_any_element(),
                ],
                false,
                s,
            )
            .gap(px(12.0 * s)),
        )
        .into_any_element()
}

/// Pads: sixteen slices a page, each sounding its slice while held.
fn pads_column(
    panel: &SlicerPanelState,
    pad_page: usize,
    cb: &SlicerCallbacks,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let count = p.slice_count as usize;
    let pages = count.div_ceil(PADS_PER_PAGE).max(1);
    let page = pad_page.min(pages - 1);
    let page_buttons =
        div()
            .flex()
            .flex_row()
            .gap(px(2.0 * s))
            .children((0..MAX_SLICES / PADS_PER_PAGE).map(|index| {
                let on_page = cb.on_pad_page.clone();
                let label = format!(
                    "{}\u{2013}{}",
                    index * PADS_PER_PAGE + 1,
                    (index + 1) * PADS_PER_PAGE
                );
                toggle(
                    ("slicer-pad-page", index),
                    label,
                    index == page,
                    index < pages,
                    22.0,
                    11.0,
                    s,
                    Rc::new(move |window, cx| on_page(&index, window, cx)),
                )
                .px(px(6.0 * s))
                .font(plate::mono(FontWeight::NORMAL))
                .into_any_element()
            }));
    let sounding: Vec<u8> = panel.sounding.iter().map(|slice| slice.note).collect();
    let pads = (0..PADS_PER_PAGE).map(|slot| {
        let index = page * PADS_PER_PAGE + slot;
        let exists = index < count;
        let note = p.note_for_slice(index);
        let playing =
            note.is_some_and(|note| panel.active_notes.contains(&note) || sounding.contains(&note));
        let picked = exists && panel.selected.unwrap_or(0) == index;
        let (fill, ink, ink2) = if playing {
            (PLAYING, ON_INK, ON_INK)
        } else if exists {
            (FACE, plate::WHITE, MUTED)
        } else {
            (0x2e2e2e, plate::WHITE, MUTED)
        };
        let press = cb.on_note_on.clone();
        let release = cb.on_note_off.clone();
        let release_out = cb.on_note_off.clone();
        div()
            .id(("slicer-pad", slot))
            .w(px(74.0 * s))
            .h(px(46.0 * s))
            .px(px(7.0 * s))
            .py(px(5.0 * s))
            .flex()
            .flex_col()
            .justify_between()
            .rounded(px(4.0 * s))
            .bg(color(fill))
            .border(px(if picked { 2.0 } else { 1.0 }))
            .border_color(color(if picked {
                plate::WHITE
            } else if exists {
                FACE_EDGE
            } else {
                0x3a3a3a
            }))
            .when(exists, |pad| {
                pad.child(
                    words((index + 1).to_string(), 13.0, ink, s).font_weight(FontWeight::SEMIBOLD),
                )
                .children(note.map(|note| plate::numbers(note_label(note), 10.5, ink2, s)))
            })
            .when_some(note.filter(|_| exists), |pad, note| {
                pad.cursor(gpui::CursorStyle::PointingHand)
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                        press(&(note, 110), window, cx)
                    })
                    .on_mouse_up(gpui::MouseButton::Left, move |_, window, cx| {
                        release(&note, window, cx)
                    })
                    .on_mouse_up_out(gpui::MouseButton::Left, move |_, window, cx| {
                        release_out(&note, window, cx)
                    })
            })
            .into_any_element()
    });
    column(s, 250.0, 330.0, false)
        .px(px(8.0 * s))
        .child(
            div()
                .pl(px(6.0 * s))
                .child(heading_row("Pads", page_buttons.into_any_element(), s)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.0 * s))
                .w(px(314.0 * s))
                .children(pads),
        )
        .into_any_element()
}

/// The picked slice: where it is, and its own direction, level and pitch.
fn slice_column(panel: &SlicerPanelState, set: &SetSlicer, s: f32) -> AnyElement {
    let p = panel.params;
    let index = panel
        .selected
        .filter(|index| *index < p.slice_count as usize)
        .unwrap_or(0);
    let seconds = panel.sample.as_ref().map_or(0.0, |sample| sample.seconds);
    let (start, end) = p.slice_region(index).unwrap_or((0.0, 0.0));
    let title = match p.note_for_slice(index) {
        Some(note) => format!("Slice {} \u{00B7} {}", index + 1, note_label(note)),
        None => format!("Slice {}", index + 1),
    };
    let info = format!(
        "{:.3} s \u{00B7} {:.3} s long",
        start as f64 * seconds,
        (end - start) as f64 * seconds
    );
    let reversed = p.slice_reversed(index);
    let reverse_set = set.clone();
    let gain_set = set.clone();
    let pitch_set = set.clone();
    let gain = p.slice_gain_db[index];
    let pitch = p.slice_pitch[index];
    column(s, 580.0, 250.0, false)
        .child(heading(title, s))
        .child(reading(info, MUTED, s))
        .child(div().flex().child(toggle(
            "slicer-slice-reverse",
            "Reverse",
            reversed,
            true,
            26.0,
            12.0,
            s,
            Rc::new(move |_, cx| {
                reverse_set(edited(p, |p| p.set_slice_reversed(index, !reversed)), cx)
            }),
        )))
        .child(knob_row(
            vec![
                knob(
                    "slicer-slice-gain",
                    Dial::new(
                        "slice-gain",
                        "Gain",
                        SLICE_GAIN_MIN_DB,
                        SLICE_GAIN_MAX_DB,
                        0.0,
                        DialUnit::Db,
                    )
                    .bipolar(),
                    gain,
                    format!("{gain:.1} dB").replace('-', "\u{2212}"),
                    true,
                    s,
                    Rc::new(move |value, cx| {
                        gain_set(edited(p, |p| p.slice_gain_db[index] = value), cx)
                    }),
                ),
                knob(
                    "slicer-slice-pitch",
                    Dial::new(
                        "slice-pitch",
                        "Pitch",
                        -SLICE_PITCH_RANGE,
                        SLICE_PITCH_RANGE,
                        0.0,
                        DialUnit::Plain,
                    )
                    .bipolar(),
                    pitch,
                    format!("{pitch:.0} st").replace('-', "\u{2212}"),
                    true,
                    s,
                    Rc::new(move |value, cx| {
                        pitch_set(edited(p, |p| p.slice_pitch[index] = value.round()), cx)
                    }),
                ),
            ],
            false,
            s,
        ))
        .into_any_element()
}

/// Playback: one shot or gate, choke, and the slices' level.
fn playback_column(p: SlicerParams, set: &SetSlicer, set_voice: &SetVoice, s: f32) -> AnyElement {
    let trigger_set = set.clone();
    let choke_set = set.clone();
    let help_text = format!(
        "{}{}",
        if p.one_shot {
            "Slices play to the end. "
        } else {
            "Slices stop when you let go. "
        },
        if p.choke {
            "A new slice cuts the last one."
        } else {
            "Slices overlap."
        }
    );
    column(s, 830.0, 182.0, false)
        .child(heading("Playback", s))
        .child(segmented(
            "slicer-trigger",
            &["One shot", "Gate"],
            Some(!p.one_shot as usize),
            true,
            s,
            Rc::new(move |index, _, cx| trigger_set(edited(p, |p| p.one_shot = index == 0), cx)),
        ))
        .child(div().flex().child(toggle(
            "slicer-choke",
            "Choke",
            p.choke,
            true,
            26.0,
            12.0,
            s,
            Rc::new(move |_, cx| choke_set(edited(p, |p| p.choke = !p.choke), cx)),
        )))
        .child(help(help_text, s))
        .child(knob_row(vec![gain_knob(p.voice, set_voice, s)], false, s).justify_center())
        .into_any_element()
}

// ── The Voice page ─────────────────────────────────────────────────────────

/// Pitch: the first slice's key, bend range, transpose and fine tune.
fn voice_pitch_column(
    p: SlicerParams,
    set: &SetSlicer,
    set_voice: &SetVoice,
    s: f32,
) -> AnyElement {
    let key_set = set.clone();
    column(s, 0.0, 230.0, true)
        .child(heading("Pitch", s))
        .child(sampler_plate::stepper_row(
            "slicer-first-key",
            "First key",
            note_label(p.first_key),
            s,
            Rc::new(move |delta, _, cx| {
                key_set(
                    edited(p, |p| {
                        p.first_key = (p.first_key as i32 + delta).clamp(0, 127) as u8
                    }),
                    cx,
                )
            }),
        ))
        .child(bend_row(p.voice, set_voice, s))
        .child(help(
            "Slices play at the sample's own pitch, one per key.",
            s,
        ))
        .child(knob_row(
            vec![
                transpose_knob(p.voice, set_voice, s),
                fine_knob(p.voice, set_voice, s),
            ],
            false,
            s,
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pads_page_holds_sixteen_and_the_buttons_cover_every_slice() {
        assert_eq!(MAX_SLICES / PADS_PER_PAGE * PADS_PER_PAGE, MAX_SLICES);
        assert_eq!(EQUAL_COUNTS.iter().max().copied(), Some(32));
    }
}
