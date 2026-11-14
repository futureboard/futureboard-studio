//! Quick Sampler's editor, laid out as the Quick Sampler design: the Sample
//! section — the header with the Classic / One shot tabs, the sample stepper,
//! Load file and Bypass; the display with the play region dimmed outside its
//! Start and End handles and the loop bar along the top; the line under it —
//! then the keyboard with the root key marked, and the Pitch, Filter, Amp
//! envelope and Output columns.
//!
//! The state and every gesture live in
//! [`crate::components::quick_sampler_window::QuickSamplerEditorWindow`];
//! this only renders, handing each edit to `set` as one whole params value.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, prelude::FluentBuilder, AnyElement, App, Bounds, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Pixels, Styled, Window,
};
use quicksampler::{FilterMode, LoopMode, QuickSamplerParams};

use crate::components::plate::{self, color, color_a, Dial, DialUnit};
use crate::components::quick_sampler_panel::{
    format_ms, with, QuickSamplerCallbacks, QuickSamplerPanelState, QUICK_SAMPLER_TITLE,
};
use crate::components::sampler_plate::{
    self, amp_column, bend_row, column, computer_keys_hint, display_box, empty_display,
    filter_column, fine_knob, heading, info_line, knob_row, output_column, paint_box, paint_wave,
    sample_facts, segmented, toggle, transpose_knob, voice_knob, voice_stepper, Header, KeyFace,
    SetVoice, DISPLAY_H,
};
use crate::components::soundfont_player_mdi::note_label;

/// The loop bar along the top of the display, and the Start and End handles
/// along its foot, in design pixels: where a press takes those markers.
pub const LOOP_BAR_H: f32 = 18.0;
pub const REGION_HANDLE_H: f32 = 22.0;

/// What the editor shows besides the panel state: the window's own reading
/// of the host and the Samples folder.
pub struct QuickSamplerView<'a> {
    pub panel: &'a QuickSamplerPanelState,
    /// Whether the insert is bypassed, when the host can tell.
    pub bypassed: Option<bool>,
    /// The sample's place among the plug-in's samples.
    pub position: Option<(usize, usize)>,
    /// The filter type a switched-off filter comes back on as.
    pub filter_memory: FilterMode,
    /// The first key the computer keyboard plays.
    pub computer_base: u8,
}

/// The whole editor at plate scale `s`. `waveform_bounds` is filled in by
/// the display's paint so the window can turn a pointer into a position in
/// the sample.
pub fn quick_sampler_plate(
    view: QuickSamplerView<'_>,
    cb: QuickSamplerCallbacks,
    set: SetVoice,
    waveform_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let panel = view.panel;
    let p = panel.params;
    let tab_set = set.clone();
    let browse = cb.on_browse.clone();
    let step = cb.on_step_sample.clone();
    let bypass = cb.on_bypass.clone();
    let header = sampler_plate::header(
        Header {
            title: QUICK_SAMPLER_TITLE,
            tabs: Some((
                &["Classic", "One shot"],
                p.one_shot as usize,
                Rc::new(move |index, _, cx| tab_set(with(p, |p| p.one_shot = index == 1), cx)),
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
    let display = sample_display(panel, &cb, view.bypassed == Some(true), waveform_bounds, s);
    let (right, error) = match (&panel.status, panel.loading) {
        (Some(status), _) => (status.clone(), true),
        (None, true) => ("Loading\u{2026}".to_string(), false),
        (None, false) => (computer_keys_hint(view.computer_base), false),
    };
    let info = info_line(info_left(panel), right, error, s);

    let press = cb.on_note_on.clone();
    let release = cb.on_note_off.clone();
    let keyboard = sampler_plate::keyboard(
        s,
        panel.keyboard_root,
        |pitch| KeyFace {
            held: panel.active_notes.contains(&pitch),
            mapped: true,
            sub: (p.keytrack && pitch == p.root_note).then(|| "Root".to_string()),
        },
        Rc::new(move |pitch, velocity, window, cx| press(&(pitch, velocity), window, cx)),
        Rc::new(move |pitch, window, cx| release(&pitch, window, cx)),
    );

    let controls = sampler_plate::controls_section(
        s,
        vec![
            pitch_column(p, &set, s),
            filter_column(p, view.filter_memory, &set, s, 230.0),
            amp(p, &set, s),
            output_column(p, &set, s, 790.0),
        ],
    );

    sampler_plate::plate_root(
        s,
        "Drop to load the sample",
        vec![
            sampler_plate::sample_section(s, header, display, info),
            keyboard,
            controls,
        ],
    )
}

/// What the sample is and how it plays, for the line under the display.
fn info_left(panel: &QuickSamplerPanelState) -> String {
    let Some(sample) = panel.sample.as_ref() else {
        return "No sample".to_string();
    };
    let p = panel.params;
    let at = |fraction: f32| format!("{:.3} s", fraction as f64 * sample.seconds);
    let mut line = format!(
        "{} \u{00B7} {} to {}",
        sample_facts(sample),
        at(p.start),
        at(p.end)
    );
    if !p.one_shot {
        match p.loop_mode {
            LoopMode::Forward => line.push_str(" \u{00B7} loop on"),
            LoopMode::PingPong => line.push_str(" \u{00B7} ping-pong loop"),
            LoopMode::Off => {}
        }
    }
    if p.reverse {
        line.push_str(" \u{00B7} reversed");
    }
    if p.normalize {
        line.push_str(" \u{00B7} normalized");
    }
    if !p.keytrack {
        line.push_str(" \u{00B7} fixed pitch");
    }
    line
}

// ── The display ────────────────────────────────────────────────────────────

fn sample_display(
    panel: &QuickSamplerPanelState,
    cb: &QuickSamplerCallbacks,
    bypassed: bool,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let p = panel.params;
    let has_sample = panel.sample.is_some();
    let seconds = panel.sample.as_ref().map_or(0.0, |sample| sample.seconds);
    let peaks = panel.sample.as_ref().map(|sample| sample.peaks.clone());
    let press = cb.on_waveform_press.clone();
    let menu = cb.on_waveform_menu.clone();
    display_box("quick-sampler-display", s)
        .when(bypassed, |display| display.opacity(0.5))
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    press(
                        &(f32::from(event.position.x), f32::from(event.position.y)),
                        window,
                        cx,
                    )
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
                    paint_classic(window, cx, bounds, peaks.as_deref(), &p, seconds)
                },
            )
            .size_full(),
        )
        .when(!has_sample, |display| {
            display.child(empty_display(
                panel.loading,
                "or use Load file. The sample plays across the keyboard from its root key.",
                s,
            ))
        })
        .when(bypassed, |display| {
            display.child(sampler_plate::bypass_notice(s))
        })
        .into_any_element()
}

/// The display as the design draws it: the waveform, the play region's
/// outside dimmed, the loop band with its bar and handles, and the Start
/// and End lines with their handles.
fn paint_classic(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    peaks: Option<&Vec<(f32, f32)>>,
    p: &QuickSamplerParams,
    seconds: f64,
) {
    let x0 = f32::from(bounds.origin.x);
    let y0 = f32::from(bounds.origin.y);
    let w = f32::from(bounds.size.width).max(1.0);
    let h = f32::from(bounds.size.height).max(1.0);
    let k = h / DISPLAY_H;
    let Some(peaks) = peaks else {
        return;
    };
    paint_wave(window, bounds, peaks, y0 + 150.0 * k, 135.0 * k);
    let at = |fraction: f32| x0 + fraction.clamp(0.0, 1.0) * w;
    let (sx, ex) = (at(p.start), at(p.end));
    let dim = color_a(sampler_plate::DISPLAY, 0.72);
    paint_box(window, (x0, y0, sx - x0, h), dim);
    paint_box(window, (ex, y0, x0 + w - ex, h), dim);

    let looping = p.loop_mode != LoopMode::Off && !p.one_shot;
    if looping {
        let (lsx, lex) = (at(p.loop_start), at(p.loop_end));
        let bar = LOOP_BAR_H * k;
        paint_box(
            window,
            (lsx, y0 + bar, lex - lsx, h - bar),
            color_a(0xffffff, 0.06),
        );
        paint_box(window, (lsx, y0, lex - lsx, bar), color(0x3a3a3a));
        if lex - lsx > 70.0 * k {
            let length = (p.loop_end - p.loop_start) as f64 * seconds;
            let name = if p.loop_mode == LoopMode::PingPong {
                "Ping-pong"
            } else {
                "Loop"
            };
            plate::paint_text(
                window,
                cx,
                &format!("{name} {length:.2} s"),
                plate::outfit(FontWeight::NORMAL),
                11.0 * k,
                color(plate::WHITE),
                (lsx + 14.0 * k, y0 + 2.0 * k),
                plate::Align::Left,
            );
        }
        let handle = 12.0 * k;
        paint_box(window, (lsx, y0, handle, bar), color(sampler_plate::INK));
        paint_box(
            window,
            (lex - handle, y0, handle, bar),
            color(sampler_plate::INK),
        );
    }

    let line = 1.5 * k;
    paint_box(window, (sx, y0, line, h), color(plate::WHITE));
    paint_box(window, (ex - line, y0, line, h), color(plate::WHITE));
    let foot = REGION_HANDLE_H * k;
    let label = |window: &mut Window, cx: &mut App, text: &str, x: f32| {
        plate::paint_text(
            window,
            cx,
            text,
            plate::outfit(FontWeight::SEMIBOLD),
            11.0 * k,
            color(sampler_plate::ON_INK),
            (x, y0 + h - foot + 4.0 * k),
            plate::Align::Center,
        );
    };
    let start_w = 40.0 * k;
    paint_box(
        window,
        (sx, y0 + h - foot, start_w, foot),
        color(plate::WHITE),
    );
    label(window, cx, "Start", sx + start_w * 0.5);
    paint_box(
        window,
        (ex - start_w, y0 + h - foot, start_w, foot),
        color(plate::WHITE),
    );
    label(window, cx, "End", ex - start_w * 0.5);
}

// ── The columns ────────────────────────────────────────────────────────────

/// Pitch: root key, bend range, poly or mono, then transpose, fine tune and
/// the mono glide.
fn pitch_column(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    let mono = p.polyphony <= 1;
    let voices_set = set.clone();
    let glide = Dial::new(
        "glide",
        "Glide",
        0.0,
        quicksampler::GLIDE_MAX_MS,
        0.0,
        DialUnit::Time,
    );
    column(s, 0.0, 230.0, true)
        .child(heading("Pitch", s))
        .child(voice_stepper(
            "sampler-root",
            "Root key",
            note_label(p.root_note),
            s,
            p,
            set,
            |p, delta| p.root_note = (p.root_note as i32 + delta).clamp(0, 127) as u8,
        ))
        .child(bend_row(p, set, s))
        .child(segmented(
            "sampler-voices",
            &["Poly", "Mono"],
            Some(mono as usize),
            true,
            s,
            Rc::new(move |index, _, cx| {
                voices_set(
                    with(p, |p| {
                        p.polyphony = match (index, p.polyphony) {
                            (1, _) => 1,
                            (_, 1) => QuickSamplerParams::default().polyphony,
                            (_, voices) => voices,
                        }
                    }),
                    cx,
                )
            }),
        ))
        .child(knob_row(
            vec![
                transpose_knob(p, set, s),
                fine_knob(p, set, s),
                voice_knob(
                    "sampler-glide",
                    glide,
                    p.glide_ms,
                    if p.glide_ms <= 0.0 {
                        "Off".to_string()
                    } else {
                        format_ms(p.glide_ms)
                    },
                    mono,
                    s,
                    p,
                    set,
                    |p, value| p.glide_ms = value,
                ),
            ],
            true,
            s,
        ))
        .into_any_element()
}

/// Amp envelope, with the loop switch at its heading.
fn amp(p: QuickSamplerParams, set: &SetVoice, s: f32) -> AnyElement {
    let looping = p.loop_mode != LoopMode::Off;
    let label = match (p.one_shot, p.loop_mode) {
        (true, _) | (false, LoopMode::Off) => "Loop off",
        (false, LoopMode::Forward) => "Loop on",
        (false, LoopMode::PingPong) => "Ping-pong",
    };
    let loop_set = set.clone();
    let corner = toggle(
        "sampler-loop",
        label,
        looping && !p.one_shot,
        !p.one_shot,
        22.0,
        11.5,
        s,
        Rc::new(move |_, cx| {
            loop_set(
                with(p, |p| {
                    p.loop_mode = if looping {
                        LoopMode::Off
                    } else {
                        LoopMode::Forward
                    }
                }),
                cx,
            )
        }),
    )
    .into_any_element();
    let help = if p.one_shot {
        "One shot plays start to end and ignores note off."
    } else if looping {
        "Sustain holds while the loop repeats."
    } else {
        "Release starts when the key lets go."
    };
    amp_column(p, Some(corner), p.one_shot, help, set, s, 490.0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::components::quick_sampler_panel::SampleSummary;

    fn panel(p: QuickSamplerParams) -> QuickSamplerPanelState {
        QuickSamplerPanelState {
            params: p,
            sample: Some(SampleSummary {
                file_name: "Rhodes C3.wav".into(),
                sample_rate: 22_050,
                channels: 1,
                seconds: 2.6,
                peaks: Arc::new(vec![(-0.5, 0.5); 16]),
            }),
            ..QuickSamplerPanelState::default()
        }
    }

    #[test]
    fn the_info_line_says_how_the_region_plays() {
        let p = QuickSamplerParams {
            loop_mode: LoopMode::Forward,
            reverse: true,
            ..QuickSamplerParams::default()
        };
        let line = info_left(&panel(p));
        assert!(line.starts_with("2.60 s \u{00B7} 22.05 kHz \u{00B7} 0.000 s to 2.600 s"));
        assert!(line.contains("loop on") && line.contains("reversed"));
        // A one-shot never loops, whatever the loop setting says.
        let one_shot = info_left(&panel(QuickSamplerParams {
            one_shot: true,
            ..p
        }));
        assert!(!one_shot.contains("loop"));
    }
}
