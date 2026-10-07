//! The built-in Slicer's panel.
//!
//! Read top to bottom like the Quick Sampler it is built from:
//!
//! * the **slice display** — the waveform cut into slices, each tinted in
//!   turn and numbered on a flag at its start, the one last played picked
//!   out. Click a slice to hear it, drag a flag to move its slice, double-
//!   click to cut a new one, right-click for a menu (play, split, join), or
//!   press Delete to join the picked slice to the one before. A sounding
//!   slice, however it was played, lights up with its playhead where the
//!   plug-in is reading;
//! * the **slice strip** — how the sample is cut (at its hits, on its beat
//!   grid, or into equal lengths) and with what setting, and how slices play:
//!   the key the first one sits on, choke, one-shot, direction, normalize;
//! * the voice **modules** every slice plays through — pitch, filter, amp
//!   and output, shared with Quick Sampler;
//! * a keyboard with each slice's key marked with its number.
//!
//! This file only renders; the state lives in
//! [`crate::components::slicer_window::SlicerEditorWindow`].

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Bounds, InteractiveElement, IntoElement, ParentElement, Pixels,
    StatefulInteractiveElement, Styled, Window, canvas, div, fill, px, relative,
};

use crate::components::controls::{
    FbButtonKind, FbSegment, fb_button, fb_checkbox, fb_segment, fb_segmented_track,
    fb_stepper_button,
};
use crate::components::quick_sampler_panel::{
    DISPLAY_H, DisplayColors, FLAG_H, I32Cb, KEYBOARD_HIGHEST_ROOT, NoteCb, ParamsCb,
    SAMPLE_DROP_GROUP, SampleSummary, VoidCb, amp_module, bipolar_knob, controls_row,
    empty_sample_drop_zone, filter_module, module, output_module, paint_display_frame,
    paint_waveform, piano_keyboard, rect, ruler_labels, sample_drop_overlay, sampler_header,
    sampler_keyboard_footer, status_banner, stepper, with,
};
use crate::components::soundfont_player_mdi::note_label;
use crate::theme::{Colors, radius, space, typography};
use quicksampler::QuickSamplerParams;
use slicer::{BeatDivision, MAX_SLICES, SliceMode, SlicerParams};

pub const SLICER_TITLE: &str = "Slicer";

/// A slice flag: wide enough for two digits.
const SLICE_FLAG_W: f32 = 18.0;
/// How near a press must land to a flag's line to grab it, in pixels.
pub const GRAB_PX: f32 = 8.0;
/// A slice sounding in the plug-in, from whatever played it — the editor,
/// the track, the virtual keyboard — as its telemetry reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoundingSlice {
    pub note: u8,
    /// Where it is reading, as a fraction of the sample.
    pub position: f32,
    /// 1 as it starts, falling to 0 a moment later: the flash over its
    /// region.
    pub flash: f32,
}

/// The slice point under a pointer at `(x, y)`, measured from the display's
/// top-left corner across a display `width` wide: on its numbered flag, or
/// within [`GRAB_PX`] of its line. A later flag is drawn over an earlier
/// one, so it wins where they overlap.
pub fn point_under(p: &SlicerParams, x: f32, y: f32, width: f32) -> Option<usize> {
    let points = p.points();
    let line_x = |point: f32| point.clamp(0.0, 1.0) * width;
    if (0.0..=FLAG_H).contains(&y) {
        let on_flag = points.iter().rposition(|point| {
            let left = line_x(*point);
            x >= left - GRAB_PX * 0.5 && x <= left + SLICE_FLAG_W
        });
        if on_flag.is_some() {
            return on_flag;
        }
    }
    points
        .iter()
        .enumerate()
        .map(|(index, point)| (index, (line_x(*point) - x).abs()))
        .filter(|(_, distance)| *distance <= GRAB_PX)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

/// What the panel draws: the insert's slicer, the loaded sample's summary,
/// and the window's transient state.
#[derive(Clone)]
pub struct SlicerPanelState {
    /// The insert's params — what its DSP in the plug-in host plays with.
    pub params: SlicerParams,
    pub sample: Option<SampleSummary>,
    pub loading: bool,
    /// Finding the sample's hits or its tempo.
    pub analysing: bool,
    pub status: Option<String>,
    pub keyboard_root: u8,
    pub active_notes: Vec<u8>,
    /// The slice point being dragged.
    pub dragging: Option<usize>,
    /// The slice last played or picked, highlighted on the display.
    pub selected: Option<usize>,
    /// Slices sounding in the plug-in, for the display and the keyboard.
    pub sounding: Vec<SoundingSlice>,
}

impl Default for SlicerPanelState {
    fn default() -> Self {
        Self {
            params: SlicerParams::default(),
            sample: None,
            loading: false,
            analysing: false,
            status: None,
            keyboard_root: slicer::DEFAULT_FIRST_KEY,
            active_notes: Vec::new(),
            dragging: None,
            selected: None,
            sounding: Vec::new(),
        }
    }
}

impl SlicerPanelState {
    pub fn is_playable(&self) -> bool {
        self.sample.is_some() && !self.loading
    }

    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root = root.clamp(0, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

pub type SlicerParamsCb = Arc<dyn Fn(&SlicerParams, &mut Window, &mut App) + 'static>;
/// A press on the display: window-space x and y, and the click count.
pub type DisplayPressCb = Arc<dyn Fn(&(f32, f32, usize), &mut Window, &mut App) + 'static>;
/// A right-press on the display: window-space x and y.
pub type DisplayRightPressCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct SlicerCallbacks {
    pub on_browse: VoidCb,
    /// One complete params value, sent as it is.
    pub on_set_params: SlicerParamsCb,
    /// Params whose slicing settings changed: the window cuts the sample
    /// again with them.
    pub on_reslice: SlicerParamsCb,
    pub on_display_press: DisplayPressCb,
    pub on_display_right_press: DisplayRightPressCb,
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    pub on_shift_octave: I32Cb,
}

/// The whole panel. `display_bounds` is filled in by the display's paint so
/// the window can turn a pointer x into a position in the sample.
pub fn slicer_panel(
    panel: &SlicerPanelState,
    cb: SlicerCallbacks,
    display_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let p = panel.params;
    // The voice modules edit `voice`; this folds their edit back in.
    let on_voice: ParamsCb = {
        let set = cb.on_set_params.clone();
        Arc::new(move |voice: &QuickSamplerParams, w, cx| {
            let mut next = p;
            next.voice = *voice;
            set(&next, w, cx)
        })
    };
    div()
        .relative()
        .group(SAMPLE_DROP_GROUP)
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(sampler_header(
            "slicer-browse",
            panel.sample.as_ref(),
            panel.loading,
            "Load or drop a drum loop or a phrase to cut it into slices, one per key",
            cb.on_browse.clone(),
        ))
        .when_some(panel.status.clone(), |root, status| {
            root.child(status_banner(status))
        })
        .child(
            div()
                .id("slicer-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .px(px(space::SECTION))
                .pt(px(space::LOOSE))
                .pb(px(space::SECTION))
                .gap(px(space::BASE))
                .child(slice_display(panel, &cb, display_bounds))
                .child(slicing_strip(panel, &cb))
                .child(playing_strip(panel, &cb))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::BASE))
                        .mt(px(space::TIGHT))
                        .child(pitch_module(p.voice, &on_voice))
                        .child(filter_module(p.voice, &on_voice))
                        .child(amp_module(p.voice, &on_voice))
                        .child(output_module(p.voice, &on_voice)),
                ),
        )
        .child(keyboard_footer(panel, &cb))
        .child(sample_drop_overlay("Drop to load and slice"))
        .into_any_element()
}

// ── Slice display ──────────────────────────────────────────────────────────

/// The waveform cut into slices.
fn slice_display(
    panel: &SlicerPanelState,
    cb: &SlicerCallbacks,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let p = panel.params;
    let sample = panel.sample.clone();
    let has_sample = sample.is_some();
    let colors = DisplayColors::resolve();
    let selected = panel.selected;
    let seconds = sample.as_ref().map(|s| s.seconds).unwrap_or(0.0);
    let peaks = sample.as_ref().map(|s| s.peaks.clone());
    let sounding = panel.sounding.clone();
    let press = cb.on_display_press.clone();
    let right_press = cb.on_display_right_press.clone();

    let mut display = div()
        .id("slicer-display")
        .relative()
        .flex_shrink_0()
        .h(px(DISPLAY_H))
        .w_full()
        .rounded(px(radius::SURFACE))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::PointingHand)
                .on_mouse_down(gpui::MouseButton::Left, move |event, w, cx| {
                    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                    press(&(x, y, event.click_count), w, cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, w, cx| {
                    let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                    right_press(&(x, y), w, cx)
                })
        })
        .child(
            canvas(
                move |bounds, _, _| bounds_out.set(Some(bounds)),
                move |bounds, _, window, _| {
                    paint_slices(
                        window,
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
        );

    if has_sample {
        for (index, point) in p.points().iter().enumerate() {
            display = display.child(slice_number(*point, index, selected == Some(index)));
        }
        for (fraction, label) in ruler_labels(seconds) {
            display = display.child(
                div()
                    .absolute()
                    .bottom(px(2.0))
                    .left(relative(fraction))
                    .pl(px(3.0))
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_faint())
                    .child(label),
            );
        }
    } else {
        display = display.child(empty_sample_drop_zone(
            panel.loading,
            "or use Load Sample… above — it is cut into slices at once",
        ));
    }
    display.into_any_element()
}

/// The number on a slice's flag, at the slice's start.
fn slice_number(fraction: f32, index: usize, selected: bool) -> AnyElement {
    div()
        .absolute()
        .top(px(0.0))
        .left(relative(fraction.clamp(0.0, 1.0)))
        .w(px(SLICE_FLAG_W))
        .h(px(FLAG_H))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(if selected {
            Colors::on_accent()
        } else {
            Colors::text_inverse()
        })
        .child((index + 1).to_string())
        .into_any_element()
}

fn paint_slices(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    peaks: Option<&Vec<(f32, f32)>>,
    p: &SlicerParams,
    selected: Option<usize>,
    sounding: &[SoundingSlice],
    seconds: f64,
    c: &DisplayColors,
) {
    let frame = paint_display_frame(window, bounds, seconds, c);
    let (y0, wave_h) = (frame.y0, frame.wave_h);
    let points = p.points();

    // Every other slice tinted, so neighbours read apart; the picked one
    // stronger.
    for index in 0..points.len() {
        let Some((start, end)) = p.slice_region(index) else {
            continue;
        };
        let tint = if selected == Some(index) {
            Some(Colors::with_alpha(c.loop_marker, 0.2))
        } else if index % 2 == 1 {
            Some(c.loop_band)
        } else {
            None
        };
        if let Some(tint) = tint {
            let (from, to) = (frame.at(start), frame.at(end));
            window.paint_quad(fill(rect(from, y0, to - from, wave_h), tint));
        }
    }
    // A sounding slice lights up, brightest as it starts.
    for slice in sounding {
        let Some((start, end)) = p.slice_for_note(slice.note).and_then(|i| p.slice_region(i))
        else {
            continue;
        };
        let (from, to) = (frame.at(start), frame.at(end));
        let tint = Colors::with_alpha(c.loop_marker, 0.08 + 0.24 * slice.flash);
        window.paint_quad(fill(rect(from, y0, to - from, wave_h), tint));
    }

    if !paint_waveform(window, &frame, peaks, c) {
        return;
    }

    // Before the first slice nothing plays: dimmed, like a region's outside.
    if let Some(first) = points.first() {
        let x = frame.at(*first);
        window.paint_quad(fill(rect(frame.x0, y0, x - frame.x0, wave_h), c.skipped));
    }

    // A line and a numbered flag at each slice's start.
    for (index, point) in points.iter().enumerate() {
        let color = if selected == Some(index) {
            c.loop_marker
        } else {
            c.marker
        };
        let x = frame.at(*point).clamp(frame.x0, frame.x0 + frame.w - 1.5);
        window.paint_quad(fill(rect(x, y0, 1.5, wave_h), color));
        window.paint_quad(fill(rect(x, y0, SLICE_FLAG_W, FLAG_H), color).corner_radii(px(2.0)));
    }

    // Where each sounding slice is reading.
    for slice in sounding {
        let x = frame
            .at(slice.position)
            .clamp(frame.x0, frame.x0 + frame.w - 2.0);
        window.paint_quad(fill(rect(x - 0.5, y0, 2.0, wave_h), c.loop_marker));
    }
}

// ── Strips ─────────────────────────────────────────────────────────────────

/// A caption in the strips' small capitals.
fn caption(text: &'static str) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text)
        .into_any_element()
}

/// A `− value +` stepper over the slicer's own settings, in one row.
fn inline_stepper(
    id: &'static str,
    readout: String,
    dec: SlicerParams,
    inc: SlicerParams,
    on_change: &SlicerParamsCb,
) -> AnyElement {
    let on_dec = on_change.clone();
    let on_inc = on_change.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .child(fb_stepper_button((id, 0usize), "−", move |_, w, cx| {
            on_dec(&dec, w, cx)
        }))
        .child(
            div()
                .min_w(px(44.0))
                .flex()
                .justify_center()
                .text_size(px(typography::UI_SM))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(readout),
        )
        .child(fb_stepper_button((id, 1usize), "+", move |_, w, cx| {
            on_inc(&inc, w, cx)
        }))
        .into_any_element()
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match index {
        0 => FbSegment::First,
        i if i + 1 == count => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

fn edited(p: SlicerParams, apply: impl FnOnce(&mut SlicerParams)) -> SlicerParams {
    let mut next = p;
    apply(&mut next);
    next.sanitized()
}

/// How the sample is cut: the mode, its one setting, and a button that cuts
/// it again (after hand edits, say).
fn slicing_strip(panel: &SlicerPanelState, cb: &SlicerCallbacks) -> AnyElement {
    let p = panel.params;
    let enabled = panel.is_playable() && !panel.analysing;
    let mut modes = fb_segmented_track();
    for (index, mode) in SliceMode::ALL.into_iter().enumerate() {
        let reslice = cb.on_reslice.clone();
        modes = modes.child(fb_segment(
            ("slicer-mode", index),
            mode.label(),
            p.slice_mode == mode,
            segment_position(index, SliceMode::ALL.len()),
            move |_, w, cx| reslice(&edited(p, |p| p.slice_mode = mode), w, cx),
        ));
    }

    let setting: AnyElement = match p.slice_mode {
        SliceMode::Transient => div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .child(caption("SENSITIVITY"))
            .child(inline_stepper(
                "slicer-sensitivity",
                format!("{:.0}%", p.sensitivity * 100.0),
                edited(p, |p| p.sensitivity -= 0.05),
                edited(p, |p| p.sensitivity += 0.05),
                &cb.on_reslice,
            ))
            .into_any_element(),
        SliceMode::Beat => {
            let mut divisions = fb_segmented_track();
            for (index, division) in BeatDivision::ALL.into_iter().enumerate() {
                let reslice = cb.on_reslice.clone();
                divisions = divisions.child(fb_segment(
                    ("slicer-division", index),
                    division.label(),
                    p.beat_division == division,
                    segment_position(index, BeatDivision::ALL.len()),
                    move |_, w, cx| reslice(&edited(p, |p| p.beat_division = division), w, cx),
                ));
            }
            let bpm = if p.sample_bpm > 0.0 {
                format!("{:.1}", p.sample_bpm)
            } else {
                "—".to_string()
            };
            let halve = cb.on_reslice.clone();
            let double = cb.on_reslice.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(caption("TEMPO"))
                .child(inline_stepper(
                    "slicer-bpm",
                    bpm,
                    edited(p, |p| {
                        p.sample_bpm = (p.sample_bpm - 1.0).max(slicer::MIN_SAMPLE_BPM)
                    }),
                    edited(p, |p| {
                        p.sample_bpm = (p.sample_bpm + 1.0).max(slicer::MIN_SAMPLE_BPM)
                    }),
                    &cb.on_reslice,
                ))
                .child(fb_stepper_button(
                    "slicer-bpm-half",
                    "÷2",
                    move |_, w, cx| halve(&edited(p, |p| p.sample_bpm *= 0.5), w, cx),
                ))
                .child(fb_stepper_button(
                    "slicer-bpm-double",
                    "×2",
                    move |_, w, cx| double(&edited(p, |p| p.sample_bpm *= 2.0), w, cx),
                ))
                .child(divisions.w(px(150.0)))
                .into_any_element()
        }
        SliceMode::Equal => div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .child(caption("SLICES"))
            .child(inline_stepper(
                "slicer-equal",
                p.equal_slices.to_string(),
                edited(p, |p| p.equal_slices = p.equal_slices.saturating_sub(1)),
                edited(p, |p| p.equal_slices = p.equal_slices.saturating_add(1)),
                &cb.on_reslice,
            ))
            .into_any_element(),
    };

    let again = cb.on_reslice.clone();
    let count = p.slice_count as usize;
    let summary = if panel.analysing {
        "Slicing…".to_string()
    } else if count == 0 {
        "No slices".to_string()
    } else {
        let last = p
            .note_for_slice(count - 1)
            .map(note_label)
            .unwrap_or_else(|| "—".to_string());
        format!(
            "{count} slice{} · {} – {last}",
            if count == 1 { "" } else { "s" },
            note_label(p.first_key)
        )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(space::LOOSE))
                .opacity(if enabled { 1.0 } else { 0.48 })
                .child(caption("SLICE BY"))
                .child(modes.w(px(244.0)))
                .child(setting)
                .child(fb_button(
                    "slicer-again",
                    "Slice Again",
                    FbButtonKind::Default,
                    enabled,
                    move |_, w, cx| again(&p, w, cx),
                ))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_secondary())
                        .child(summary),
                ),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(format!(
                    "Click a slice to hear it · drag a flag to move it · double-click to cut · \
                     right-click for more (play, split, join) · Delete joins the picked slice \
                     to the one before · up to {MAX_SLICES} slices"
                )),
        )
        .into_any_element()
}

/// How the slices play: which key starts them, and how they meet.
fn playing_strip(panel: &SlicerPanelState, cb: &SlicerCallbacks) -> AnyElement {
    let p = panel.params;
    let toggle = |id: &'static str, label: &'static str, on: bool, apply: fn(&mut SlicerParams)| {
        let on_change = cb.on_set_params.clone();
        fb_checkbox(id, label, on, true, move |_, w, cx| {
            on_change(&edited(p, apply), w, cx)
        })
    };
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(space::LOOSE))
        .child(caption("FIRST KEY"))
        .child(inline_stepper(
            "slicer-first-key",
            note_label(p.first_key),
            edited(p, |p| p.first_key = p.first_key.saturating_sub(1)),
            edited(p, |p| p.first_key = p.first_key.saturating_add(1).min(127)),
            &cb.on_set_params,
        ))
        .child(toggle("slicer-choke", "Choke", p.choke, |p| {
            p.choke = !p.choke
        }))
        .child(toggle("slicer-one-shot", "One-shot", p.one_shot, |p| {
            p.one_shot = !p.one_shot
        }))
        .child(toggle("slicer-reverse", "Reverse", p.voice.reverse, |p| {
            p.voice.reverse = !p.voice.reverse
        }))
        .child(toggle(
            "slicer-normalize",
            "Normalize",
            p.voice.normalize,
            |p| p.voice.normalize = !p.voice.normalize,
        ))
        .into_any_element()
}

// ── Modules ────────────────────────────────────────────────────────────────

/// Pitch for slices: no root key or key tracking — every slice plays at the
/// sample's own pitch on its key.
fn pitch_module(p: QuickSamplerParams, on_set: &ParamsCb) -> AnyElement {
    let on_fine = on_set.clone();
    module(
        "PITCH",
        None,
        div().flex().flex_col().gap(px(space::BASE)).child(
            controls_row()
                .items_center()
                .child(stepper(
                    "slicer-transpose",
                    "Transpose",
                    format!("{:+}", p.transpose),
                    with(p, |p| p.transpose = p.transpose.saturating_sub(1)),
                    with(p, |p| p.transpose = p.transpose.saturating_add(1)),
                    on_set,
                ))
                .child(bipolar_knob(
                    "slicer-fine",
                    "Fine",
                    format!("{:+.0} ct", p.fine_cents),
                    (p.fine_cents, -100.0, 100.0),
                    move |value, w, cx| on_fine(&with(p, |p| p.fine_cents = value.round()), w, cx),
                ))
                .child(stepper(
                    "slicer-bend",
                    "Bend range",
                    format!("±{}", p.pitch_bend_range),
                    with(p, |p| {
                        p.pitch_bend_range = p.pitch_bend_range.saturating_sub(1)
                    }),
                    with(p, |p| {
                        p.pitch_bend_range = p.pitch_bend_range.saturating_add(1)
                    }),
                    on_set,
                )),
        ),
    )
}

// ── Keyboard ───────────────────────────────────────────────────────────────

fn keyboard_footer(panel: &SlicerPanelState, cb: &SlicerCallbacks) -> AnyElement {
    let p = panel.params;
    let mut lit = panel.active_notes.clone();
    lit.extend(panel.sounding.iter().map(|slice| slice.note));
    let board = piano_keyboard(
        "slicer-key",
        panel.keyboard_root,
        &lit,
        panel.is_playable(),
        &|pitch| p.slice_for_note(pitch).map(|index| (index + 1).to_string()),
        cb.on_note_on.clone(),
        cb.on_note_off.clone(),
    );
    sampler_keyboard_footer(
        "slicer-octave",
        panel.keyboard_root,
        &panel.active_notes,
        format!("Slices play from {}, one per key", note_label(p.first_key)),
        cb.on_shift_octave.clone(),
        board,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_opens_on_the_first_slice() {
        let panel = SlicerPanelState::default();
        assert_eq!(panel.keyboard_root, panel.params.first_key);
        assert_eq!(note_label(panel.params.first_key), "C3");
    }

    #[test]
    fn a_press_on_a_flag_or_near_its_line_finds_that_slice_point() {
        let mut p = SlicerParams {
            slice_count: 3,
            ..SlicerParams::default()
        };
        p.slices[..3].copy_from_slice(&[0.0, 0.5, 0.505]);
        // On the number, well right of the line.
        assert_eq!(point_under(&p, 14.0, 6.0, 1_000.0), Some(0));
        // Two overlapping flags: the one drawn on top.
        assert_eq!(point_under(&p, 512.0, 6.0, 1_000.0), Some(2));
        // Below the flags only the line counts.
        assert_eq!(point_under(&p, 14.0, 80.0, 1_000.0), None);
        assert_eq!(point_under(&p, 497.0, 80.0, 1_000.0), Some(1));
        assert_eq!(point_under(&p, 300.0, 80.0, 1_000.0), None);
    }

    #[test]
    fn edits_stay_in_range() {
        let p = SlicerParams::default();
        assert_eq!(edited(p, |p| p.sensitivity += 2.0).sensitivity, 1.0);
        assert_eq!(
            edited(p, |p| p.equal_slices = 0).equal_slices,
            slicer::MIN_EQUAL_SLICES
        );
    }
}
