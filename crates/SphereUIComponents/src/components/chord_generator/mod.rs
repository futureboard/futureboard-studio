//! Chord Generator: an independent utility window that writes a progression
//! and hands it to the arrangement.
//!
//! The flow is built for speed: pick a key, scale and style, press
//! **Generate** until something sounds right, lock the chords you like and
//! re-roll the rest, then either drag the progression (or a single chord) onto
//! the Chord Track or a MIDI track, or use the footer buttons to drop it at the
//! playhead. Every chord card auditions on click through the track the virtual
//! keyboard plays.
//!
//! The window owns only its own state. Anything that touches the project goes
//! back to the Studio as a [`ChordGeneratorCommand`]; the Studio resolves drops
//! against the main window's live pointer, so a drag that starts here and ends
//! over the arrangement lands where its ghost was shown.

use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, size, AnyElement, App, AppContext, Bounds, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};
use sphere_midi_service::chords::{
    alternatives, generate, pitch_name, voice_progression, Chord, Colors as ChordColors,
    GeneratorSettings, Richness, Rng, Scale, Style, VoicingOptions,
};

use crate::assets;
use crate::components::controls::{
    fb_button, fb_checkbox, fb_section_label, fb_segment, fb_segmented_track, fb_tooltip,
    FbButtonKind, FbSegment,
};
use crate::components::pitch_wheel::{self, fifths_pitch_class, ring, WheelFrame};
use crate::components::timeline::timeline_state::ChordPlacement;
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const CHORD_GENERATOR_WIDTH: f32 = 920.0;
pub const CHORD_GENERATOR_HEIGHT: f32 = 660.0;
const MIN_WIDTH: f32 = 820.0;
const MIN_HEIGHT: f32 = 620.0;
const WHEEL_SIZE: f32 = 200.0;
const CARD_W: f32 = 136.0;
const CARD_H: f32 = 156.0;
/// Pointer travel before a press becomes a drag.
const DRAG_THRESHOLD: f32 = 4.0;
const FRAME: Duration = Duration::from_millis(16);
const SETTLE_SECONDS: f32 = 0.12;
/// How long a clicked chord rings when the transport is not driving it.
const AUDITION_SECONDS: f32 = 1.4;
/// Mini keyboard span (upper voices fold into it).
const KEYS_LOW: u8 = 48;
const KEYS_HIGH: u8 = 71;

/// What the window asks the Studio to do.
pub enum ChordGeneratorCommand {
    /// Lay the chords on the Chord Track from the playhead.
    PlaceOnChordTrack { chords: Vec<ChordPlacement> },
    /// One voiced MIDI clip at the playhead on the selected MIDI track (or a
    /// new one).
    CreateMidiClip {
        chords: Vec<ChordPlacement>,
        voicing: VoicingOptions,
    },
    /// The pointer moved while dragging: update the drop ghost.
    DragMove { chords: Vec<ChordPlacement> },
    /// Released: drop where the ghost was.
    DragEnd {
        chords: Vec<ChordPlacement>,
        voicing: VoicingOptions,
    },
    /// Escape, or the window lost the gesture: clear the ghost.
    DragCancel,
}

#[derive(Clone)]
pub struct ChordGeneratorCallbacks {
    /// Runs a command; the returned text (if any) is shown in the footer.
    pub on_command: Arc<dyn Fn(ChordGeneratorCommand, &mut App) -> Option<String> + Send + Sync>,
    /// Note on/off for auditioning. `false` when nothing can play them.
    pub audition: Arc<dyn Fn(&[u8], bool, &mut App) -> bool + Send + Sync>,
    /// Current project tempo, for playing the progression back.
    pub project_bpm: Arc<dyn Fn(&App) -> f32 + Send + Sync>,
    pub on_close: Arc<dyn Fn(Bounds<Pixels>, &mut App) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    chord: Chord,
    locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Payload {
    Progression,
    Slot(usize),
}

#[derive(Debug, Clone, Copy)]
struct Press {
    payload: Payload,
    origin: Point<Pixels>,
    dragging: bool,
}

/// Popover anchored at a window point.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Popover {
    Scale(Point<Pixels>),
    Swap(usize, Point<Pixels>),
}

pub struct ChordGeneratorWindow {
    callbacks: ChordGeneratorCallbacks,
    focus: FocusHandle,
    settings: GeneratorSettings,
    seed: u64,
    slots: Vec<Slot>,
    beats_per_chord: f64,
    voicing: VoicingOptions,
    selected: Option<usize>,
    popover: Option<Popover>,
    press: Option<Press>,
    /// Notes currently sounding from an audition, for the note-offs.
    sounding: Vec<u8>,
    /// Bumped on every audition start, so a stale timer does not cut a newer
    /// chord short.
    audition_generation: u64,
    playing: bool,
    status: Option<String>,
    // Wheel animation.
    shown_energy: [f32; 12],
    ticking: bool,
    last_tick: Instant,
    window_active: bool,
    _activation: Option<gpui::Subscription>,
}

impl ChordGeneratorWindow {
    fn new(callbacks: ChordGeneratorCallbacks, cx: &mut Context<Self>) -> Self {
        let seed = time_seed();
        let settings = GeneratorSettings::default();
        let slots = generate(&settings, seed)
            .into_iter()
            .map(|chord| Slot {
                chord,
                locked: false,
            })
            .collect();
        let mut window = Self {
            callbacks,
            focus: cx.focus_handle(),
            settings,
            seed,
            slots,
            beats_per_chord: 4.0,
            voicing: VoicingOptions::default(),
            selected: Some(0),
            popover: None,
            press: None,
            sounding: Vec::new(),
            audition_generation: 0,
            playing: false,
            status: None,
            shown_energy: [0.0; 12],
            ticking: false,
            last_tick: Instant::now(),
            window_active: true,
            _activation: None,
        };
        window.ensure_ticking(cx);
        window
    }

    // ── Progression ─────────────────────────────────────────────────────────

    fn flats(&self) -> bool {
        self.settings.scale.prefers_flats(self.settings.tonic)
    }

    /// Re-run the generator with the current seed, keeping locked chords.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let fresh = generate(&self.settings, self.seed);
        let mut slots = Vec::with_capacity(fresh.len());
        for (index, chord) in fresh.into_iter().enumerate() {
            match self.slots.get(index) {
                Some(slot) if slot.locked => slots.push(*slot),
                _ => slots.push(Slot {
                    chord,
                    locked: false,
                }),
            }
        }
        self.slots = slots;
        if self.selected.is_some_and(|i| i >= self.slots.len()) {
            self.selected = Some(0);
        }
        self.popover = None;
        self.status = None;
        self.ensure_ticking(cx);
        cx.notify();
    }

    /// A new roll of the dice.
    fn regenerate(&mut self, cx: &mut Context<Self>) {
        let mut rng = Rng::new(self.seed ^ time_seed());
        self.seed = rng.next_u64();
        self.rebuild(cx);
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut GeneratorSettings),
    ) {
        let before = self.settings;
        edit(&mut self.settings);
        if self.settings != before {
            self.rebuild(cx);
        }
    }

    fn reroll_slot(&mut self, index: usize, cx: &mut Context<Self>) {
        let next = self.slots.get(index + 1).map(|s| s.chord);
        let options = alternatives(&self.settings, next);
        let current = self.slots[index].chord;
        let choices: Vec<Chord> = options.into_iter().filter(|c| *c != current).collect();
        if choices.is_empty() {
            return;
        }
        let mut rng = Rng::new(time_seed() ^ index as u64);
        self.slots[index].chord = choices[rng.below(choices.len())];
        self.selected = Some(index);
        self.audition_slot(index, cx);
    }

    fn placements(&self, payload: Payload) -> Vec<ChordPlacement> {
        let flats = self.flats();
        let slots: Vec<&Slot> = match payload {
            Payload::Progression => self.slots.iter().collect(),
            Payload::Slot(i) => self.slots.get(i).into_iter().collect(),
        };
        slots
            .into_iter()
            .map(|slot| ChordPlacement {
                chord: slot.chord,
                flats,
                length_beats: self.beats_per_chord,
            })
            .collect()
    }

    fn voiced(&self) -> Vec<Vec<u8>> {
        voice_progression(
            &self.slots.iter().map(|s| s.chord).collect::<Vec<_>>(),
            self.voicing,
        )
    }

    // ── Audition ────────────────────────────────────────────────────────────

    fn stop_sounding(&mut self, cx: &mut Context<Self>) {
        if !self.sounding.is_empty() {
            let notes = std::mem::take(&mut self.sounding);
            (self.callbacks.audition)(&notes, false, cx);
        }
    }

    /// Sound one chord, voiced as it sits in the progression, for `seconds`.
    fn sound_slot(&mut self, index: usize, seconds: f32, cx: &mut Context<Self>) -> bool {
        self.stop_sounding(cx);
        let Some(notes) = self.voiced().get(index).cloned() else {
            return false;
        };
        if !(self.callbacks.audition)(&notes, true, cx) {
            self.status = Some("Select an instrument track to hear the chords".to_string());
            return false;
        }
        self.sounding = notes;
        self.audition_generation = self.audition_generation.wrapping_add(1);
        let generation = self.audition_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs_f32(seconds))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.audition_generation == generation {
                    this.stop_sounding(cx);
                    if this.playing {
                        this.advance_playback(index, cx);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        true
    }

    fn audition_slot(&mut self, index: usize, cx: &mut Context<Self>) {
        self.playing = false;
        self.selected = Some(index);
        self.sound_slot(index, AUDITION_SECONDS, cx);
        self.ensure_ticking(cx);
        cx.notify();
    }

    fn chord_seconds(&self, cx: &App) -> f32 {
        let bpm = (self.callbacks.project_bpm)(cx).clamp(20.0, 400.0);
        (self.beats_per_chord as f32 * 60.0 / bpm).clamp(0.25, 8.0)
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.playing {
            self.playing = false;
            self.audition_generation = self.audition_generation.wrapping_add(1);
            self.stop_sounding(cx);
        } else if !self.slots.is_empty() {
            self.playing = true;
            self.selected = Some(0);
            let seconds = self.chord_seconds(cx);
            if !self.sound_slot(0, seconds, cx) {
                self.playing = false;
            }
            self.ensure_ticking(cx);
        }
        cx.notify();
    }

    fn advance_playback(&mut self, finished: usize, cx: &mut Context<Self>) {
        let next = finished + 1;
        if next >= self.slots.len() {
            self.playing = false;
            return;
        }
        self.selected = Some(next);
        let seconds = self.chord_seconds(cx);
        if !self.sound_slot(next, seconds, cx) {
            self.playing = false;
        }
        self.ensure_ticking(cx);
    }

    // ── Commands ────────────────────────────────────────────────────────────

    fn run(&mut self, command: ChordGeneratorCommand, cx: &mut Context<Self>) {
        if let Some(message) = (self.callbacks.on_command)(command, cx) {
            self.status = Some(message);
        }
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.press.is_some_and(|p| p.dragging) {
            (self.callbacks.on_command)(ChordGeneratorCommand::DragCancel, cx);
        }
        self.playing = false;
        self.stop_sounding(cx);
        (self.callbacks.on_close)(window.bounds(), cx);
        window.remove_window();
    }

    // ── Drag (cross-window) ─────────────────────────────────────────────────

    fn begin_press(&mut self, payload: Payload, origin: Point<Pixels>, cx: &mut Context<Self>) {
        self.popover = None;
        self.press = Some(Press {
            payload,
            origin,
            dragging: false,
        });
        cx.notify();
    }

    /// Whether a window-local point is over this window. A drag released
    /// here must not land in the arrangement hidden behind it.
    fn over_self(position: Point<Pixels>, window: &Window) -> bool {
        let size = window.viewport_size();
        position.x >= px(0.0)
            && position.y >= px(0.0)
            && position.x <= size.width
            && position.y <= size.height
    }

    fn pointer_moved(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(mut press) = self.press else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            // The button came up somewhere we did not see: end cleanly.
            self.finish_press(None, cx);
            return;
        }
        if !press.dragging {
            let dx: f32 = (event.position.x - press.origin.x).into();
            let dy: f32 = (event.position.y - press.origin.y).into();
            if dx.hypot(dy) < DRAG_THRESHOLD {
                return;
            }
            press.dragging = true;
            self.press = Some(press);
        }
        if Self::over_self(event.position, window) {
            self.run(ChordGeneratorCommand::DragCancel, cx);
            self.status = Some("Drag out over the Chord Track or a MIDI track".to_string());
            cx.notify();
            return;
        }
        let chords = self.placements(press.payload);
        self.run(ChordGeneratorCommand::DragMove { chords }, cx);
    }

    /// End a press. `released_at` is the release point (window-local) for a
    /// mouse-up; `None` when the gesture was abandoned.
    fn finish_press(&mut self, released_at: Option<(Point<Pixels>, bool)>, cx: &mut Context<Self>) {
        let Some(press) = self.press.take() else {
            return;
        };
        // `(point, over_this_window)`.
        let released = released_at.is_some();
        let dropped_on_self = released_at.is_some_and(|(_, over)| over);
        if press.dragging {
            if dropped_on_self {
                self.run(ChordGeneratorCommand::DragCancel, cx);
                self.status = None;
            } else if released {
                let chords = self.placements(press.payload);
                let voicing = self.voicing;
                self.status = None;
                self.run(ChordGeneratorCommand::DragEnd { chords, voicing }, cx);
            } else {
                self.run(ChordGeneratorCommand::DragCancel, cx);
            }
        } else if released {
            if let Payload::Slot(index) = press.payload {
                self.audition_slot(index, cx);
            }
        }
        cx.notify();
    }

    // ── Wheel animation ─────────────────────────────────────────────────────

    fn wheel_target(&self) -> [f32; 12] {
        let mut energy = [0.0; 12];
        if let Some(slot) = self.selected.and_then(|i| self.slots.get(i)) {
            for pc in slot.chord.pitch_classes() {
                energy[pc as usize] = if pc == slot.chord.root { 1.0 } else { 0.72 };
            }
        }
        energy
    }

    fn ensure_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        self.last_tick = Instant::now();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(FRAME).await;
            match this.update(cx, |this, cx| this.tick(cx)) {
                Ok(true) => {}
                _ => break,
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;
        let target = self.wheel_target();
        let blend = if self.window_active {
            1.0 - (-dt / SETTLE_SECONDS).exp()
        } else {
            1.0
        };
        let mut changed = false;
        for (shown, goal) in self.shown_energy.iter_mut().zip(target) {
            let next = if (goal - *shown).abs() < 0.002 {
                goal
            } else {
                *shown + (goal - *shown) * blend
            };
            if next != *shown {
                *shown = next;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        } else {
            self.ticking = false;
        }
        changed
    }

    fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.window_active != active {
            self.window_active = active;
            self.ensure_ticking(cx);
            cx.notify();
        }
    }

    // ── Render pieces ───────────────────────────────────────────────────────

    fn wheel(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale_pcs = self.settings.scale.pitch_classes(self.settings.tonic);
        let mut in_key = [false; 12];
        for pc in &scale_pcs {
            in_key[*pc as usize] = true;
        }
        let chord = self
            .selected
            .and_then(|i| self.slots.get(i))
            .map(|s| s.chord);
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
        let flats = self.flats();
        let radius = WHEEL_SIZE * 0.5;
        let tonic = self.settings.tonic as usize;
        let labels = (0..12).map(move |position| {
            let pc = fifths_pitch_class(position);
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
                    .text_size(px(20.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(c.name(flats))
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

    fn controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let flats = self.flats();
        let tonic = self.settings.tonic;
        let keys = fb_segmented_track().children((0..12u8).map(|pc| {
            fb_segment(
                ("chordgen-key", pc as usize),
                pitch_name(pc, flats),
                pc == tonic,
                segment_position(pc as usize, 12),
                cx.listener(move |this, _, _, cx| {
                    this.update_settings(cx, |s| s.tonic = pc);
                }),
            )
        }));
        let scale_label = self.settings.scale.label();
        let scale_trigger = div().id("chordgen-scale").w(px(190.0)).child(
            crate::components::combo_box::combo_box_trigger(
                "chordgen-scale-trigger",
                scale_label,
                matches!(self.popover, Some(Popover::Scale(_))),
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.popover = match this.popover {
                        Some(Popover::Scale(_)) => None,
                        _ => Some(Popover::Scale(event.position)),
                    };
                    cx.notify();
                }),
            ),
        );
        let styles =
            fb_segmented_track().children(Style::ALL.iter().enumerate().map(|(index, style)| {
                let style = *style;
                fb_segment(
                    ("chordgen-style", index),
                    style.label(),
                    self.settings.style == style,
                    segment_position(index, Style::ALL.len()),
                    cx.listener(move |this, _, _, cx| {
                        this.update_settings(cx, |s| s.style = style);
                    }),
                )
            }));
        let richness_options = [
            (Richness::Triads, "Triads"),
            (Richness::Sevenths, "7ths"),
            (Richness::Ninths, "9ths"),
        ];
        let richness = fb_segmented_track().children(richness_options.iter().enumerate().map(
            |(index, (value, label))| {
                let value = *value;
                fb_segment(
                    ("chordgen-richness", index),
                    *label,
                    self.settings.richness == value,
                    segment_position(index, richness_options.len()),
                    cx.listener(move |this, _, _, cx| {
                        this.update_settings(cx, |s| s.richness = value);
                    }),
                )
            },
        ));
        let lengths = [4usize, 8];
        let length =
            fb_segmented_track().children(lengths.iter().enumerate().map(|(index, value)| {
                let value = *value;
                fb_segment(
                    ("chordgen-length", index),
                    format!("{value} chords"),
                    self.settings.length == value,
                    segment_position(index, lengths.len()),
                    cx.listener(move |this, _, _, cx| {
                        this.update_settings(cx, |s| s.length = value);
                    }),
                )
            }));
        let durations = [(2.0, "½ bar"), (4.0, "1 bar"), (8.0, "2 bars")];
        let each = fb_segmented_track().children(durations.iter().enumerate().map(
            |(index, (beats, label))| {
                let beats = *beats;
                fb_segment(
                    ("chordgen-each", index),
                    *label,
                    (self.beats_per_chord - beats).abs() < 1e-9,
                    segment_position(index, durations.len()),
                    cx.listener(move |this, _, _, cx| {
                        this.beats_per_chord = beats;
                        cx.notify();
                    }),
                )
            },
        ));
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
        let colors = self.settings.colors;
        let color_toggle = |id: &'static str,
                            label: &'static str,
                            on: bool,
                            cx: &mut Context<Self>,
                            set: fn(&mut ChordColors, bool)| {
            div().flex_none().child(fb_checkbox(
                id,
                label,
                on,
                true,
                cx.listener(move |this, _, _, cx| {
                    this.update_settings(cx, |s| set(&mut s.colors, !on));
                }),
            ))
        };
        let bass_on = self.voicing.bass;

        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(labeled_row(
                "KEY",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::LOOSE))
                    .child(div().flex_1().min_w(px(0.0)).child(keys))
                    .child(scale_trigger),
            ))
            .child(labeled_row(
                "STYLE",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::LOOSE))
                    .child(div().flex_1().min_w(px(0.0)).child(styles))
                    .child(div().w(px(210.0)).child(richness)),
            ))
            .child(labeled_row(
                "SHAPE",
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(space::LOOSE))
                    .child(div().w(px(190.0)).child(length))
                    .child(div().w(px(220.0)).child(each))
                    .child(div().w(px(150.0)).child(voicing))
                    .child(div().flex_none().child(fb_checkbox(
                        "chordgen-bass",
                        "Bass note",
                        bass_on,
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.voicing.bass = !this.voicing.bass;
                            cx.notify();
                        }),
                    ))),
            ))
            .child(labeled_row(
                "COLOR",
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(color_toggle(
                        "chordgen-sus",
                        "Sus",
                        colors.sus,
                        cx,
                        |c, v| c.sus = v,
                    ))
                    .child(color_toggle(
                        "chordgen-borrowed",
                        "Borrowed",
                        colors.borrowed,
                        cx,
                        |c, v| c.borrowed = v,
                    ))
                    .child(color_toggle(
                        "chordgen-secondary",
                        "Secondary dominants",
                        colors.secondary_dominants,
                        cx,
                        |c, v| c.secondary_dominants = v,
                    ))
                    .child(color_toggle(
                        "chordgen-passing",
                        "Passing diminished",
                        colors.passing_diminished,
                        cx,
                        |c, v| c.passing_diminished = v,
                    ))
                    .child(color_toggle(
                        "chordgen-blackadder",
                        "Black Adder",
                        colors.black_adder,
                        cx,
                        |c, v| c.black_adder = v,
                    )),
            ))
    }

    fn card(&self, index: usize, notes: &[u8], cx: &mut Context<Self>) -> AnyElement {
        let slot = self.slots[index];
        let flats = self.flats();
        let selected = self.selected == Some(index);
        let dragging_this = self
            .press
            .is_some_and(|p| p.dragging && p.payload == Payload::Slot(index));
        let rest = Colors::surface_panel();
        let fill = if selected {
            Colors::composite(rest, Colors::state_selected())
        } else {
            rest
        };
        let hover = Colors::composite(fill, Colors::state_hover());
        let lock_icon = if slot.locked {
            assets::ICON_LOCK_PATH
        } else {
            assets::ICON_LOCK_OPEN_PATH
        };
        div()
            .id(("chordgen-card", index))
            .relative()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .flex()
            .flex_col()
            .p(px(space::BASE))
            .gap(px(space::TIGHT))
            .rounded(px(radius::SURFACE))
            .bg(fill)
            .border(px(1.0))
            .border_color(if selected {
                Colors::with_alpha(Colors::accent_primary(), 0.85)
            } else {
                Colors::border_subtle()
            })
            .when(dragging_this, |card| card.opacity(0.55))
            .cursor(gpui::CursorStyle::OpenHand)
            .hover(move |s| s.bg(hover))
            .tooltip(fb_tooltip("Click to hear · drag onto the timeline"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.begin_press(Payload::Slot(index), event.position, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_muted())
                            .child(slot.chord.roman(self.settings.tonic)),
                    )
                    .child(card_icon_button(
                        ("chordgen-lock", index),
                        lock_icon,
                        if slot.locked {
                            "Unlock — Generate may change this chord"
                        } else {
                            "Lock — keep this chord when generating"
                        },
                        slot.locked,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(slot) = this.slots.get_mut(index) {
                                slot.locked = !slot.locked;
                            }
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .text_size(px(if slot.chord.name(flats).len() > 7 {
                        17.0
                    } else {
                        22.0
                    }))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(slot.chord.name(flats)),
            )
            .child(mini_keyboard(notes, self.voicing.bass))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(space::HAIR))
                    .child(card_icon_button(
                        ("chordgen-swap", index),
                        assets::ICON_ARROW_LEFT_RIGHT_PATH,
                        "Swap for another chord",
                        false,
                        cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                            this.popover = Some(Popover::Swap(index, event.position()));
                            this.selected = Some(index);
                            cx.notify();
                        }),
                    ))
                    .child(card_icon_button(
                        ("chordgen-reroll", index),
                        assets::ICON_REFRESH_CW_PATH,
                        "Re-roll this chord",
                        false,
                        cx.listener(move |this, _, _, cx| this.reroll_slot(index, cx)),
                    )),
            )
            .into_any_element()
    }

    fn progression(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let voiced = self.voiced();
        let bars = self.beats_per_chord * self.slots.len() as f64 / 4.0;
        let handle_active = self
            .press
            .is_some_and(|p| p.dragging && p.payload == Payload::Progression);
        let handle_rest = Colors::surface_raised();
        let handle_hover = Colors::composite(handle_rest, Colors::state_hover());
        div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(space::BASE))
                            .child(fb_section_label("PROGRESSION"))
                            .child(
                                div()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_muted())
                                    .child(format!(
                                        "{} chords · {} {}",
                                        self.slots.len(),
                                        format_bars(bars),
                                        if (bars - 1.0).abs() < 1e-9 {
                                            "bar"
                                        } else {
                                            "bars"
                                        }
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .id("chordgen-drag-all")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(space::TIGHT))
                            .h(px(crate::theme::size::DEFAULT))
                            .px(px(space::BASE))
                            .rounded(px(radius::CONTROL))
                            .bg(if handle_active {
                                Colors::composite(handle_rest, Colors::state_selected())
                            } else {
                                handle_rest
                            })
                            .border(px(1.0))
                            .border_color(if handle_active {
                                Colors::with_alpha(Colors::accent_primary(), 0.85)
                            } else {
                                Colors::border_normal()
                            })
                            .cursor(gpui::CursorStyle::OpenHand)
                            .hover(move |s| s.bg(handle_hover))
                            .tooltip(fb_tooltip(
                                "Drag onto the Chord Track, or onto a MIDI track to write a clip",
                            ))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                                    this.begin_press(Payload::Progression, event.position, cx);
                                }),
                            )
                            .child(
                                gpui::svg()
                                    .path(assets::ICON_GRIP_VERTICAL_PATH)
                                    .w(px(12.0))
                                    .h(px(12.0))
                                    .text_color(Colors::text_secondary()),
                            )
                            .child(
                                div()
                                    .text_size(px(typography::UI_SM))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(Colors::text_primary())
                                    .child("Drag to timeline"),
                            ),
                    ),
            )
            .child(
                div()
                    .id("chordgen-cards")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .content_start()
                    .gap(px(space::LOOSE))
                    .children((0..self.slots.len()).map(|index| {
                        let notes = voiced.get(index).cloned().unwrap_or_default();
                        self.card(index, &notes, cx)
                    })),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone().unwrap_or_else(|| {
            "Click a chord to hear it on the selected instrument track".to_string()
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .px(px(space::SECTION))
            .py(px(space::LOOSE))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(status),
            )
            .child(fb_button(
                "chordgen-play",
                if self.playing { "Stop" } else { "Play" },
                FbButtonKind::Ghost,
                !self.slots.is_empty(),
                cx.listener(|this, _, _, cx| this.toggle_playback(cx)),
            ))
            .child(fb_button(
                "chordgen-to-lane",
                "Add to Chord Track",
                FbButtonKind::Default,
                !self.slots.is_empty(),
                cx.listener(|this, _, _, cx| {
                    let chords = this.placements(Payload::Progression);
                    this.run(ChordGeneratorCommand::PlaceOnChordTrack { chords }, cx);
                }),
            ))
            .child(fb_button(
                "chordgen-to-midi",
                "Create MIDI Clip",
                FbButtonKind::Default,
                !self.slots.is_empty(),
                cx.listener(|this, _, _, cx| {
                    let chords = this.placements(Payload::Progression);
                    let voicing = this.voicing;
                    this.run(
                        ChordGeneratorCommand::CreateMidiClip { chords, voicing },
                        cx,
                    );
                }),
            ))
            .child(fb_button(
                "chordgen-generate",
                "Generate",
                FbButtonKind::Primary,
                true,
                cx.listener(|this, _, _, cx| this.regenerate(cx)),
            ))
    }

    fn popover(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let viewport = window.viewport_size();
        let clamp = |point: Point<Pixels>, w: f32, h: f32| {
            let vw: f32 = viewport.width.into();
            let vh: f32 = viewport.height.into();
            let x: f32 = point.x.into();
            let y: f32 = point.y.into();
            (
                x.min(vw - w - space::BASE).max(space::BASE),
                (y + space::TIGHT)
                    .min(vh - h - space::BASE)
                    .max(space::BASE),
            )
        };
        let menu = |left: f32, top: f32, w: f32, h: f32| {
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(w))
                .max_h(px(h))
                .p(px(space::TIGHT))
                .rounded(px(radius::SURFACE))
                .bg(Colors::surface_card())
                .border(px(1.0))
                .border_color(Colors::border_normal())
                .shadow_lg()
                .flex()
                .flex_col()
                // Clicks inside the menu must not reach the root's
                // click-away handler before the item's own click fires.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        let row = |id: gpui::ElementId, label: String, detail: String, active: bool| {
            let rest = if active {
                Colors::composite(Colors::surface_card(), Colors::state_selected())
            } else {
                Colors::with_alpha(Colors::surface_card(), 0.0)
            };
            div()
                .id(id)
                .h(px(crate::theme::size::DEFAULT))
                .px(px(space::BASE))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .rounded(px(radius::CONTROL))
                .bg(rest)
                .hover(|s| s.bg(Colors::surface_control_hover()))
                .cursor(gpui::CursorStyle::PointingHand)
                .child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .font_weight(if active {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(Colors::text_primary())
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(detail),
                )
        };
        match self.popover? {
            Popover::Scale(point) => {
                let (left, top) = clamp(point, 220.0, 360.0);
                let current = self.settings.scale;
                Some(
                    menu(left, top, 220.0, 360.0)
                        .id("chordgen-scale-menu")
                        .overflow_y_scroll()
                        .children(Scale::ALL.iter().enumerate().map(|(index, scale)| {
                            let scale = *scale;
                            let detail = if scale.is_heptatonic() {
                                String::new()
                            } else {
                                "symmetric".to_string()
                            };
                            row(
                                ("chordgen-scale-item", index).into(),
                                scale.label().to_string(),
                                detail,
                                scale == current,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.popover = None;
                                    this.update_settings(cx, |s| s.scale = scale);
                                    cx.notify();
                                },
                            ))
                        }))
                        .into_any_element(),
                )
            }
            Popover::Swap(index, point) => {
                let next = self.slots.get(index + 1).map(|s| s.chord);
                let options = alternatives(&self.settings, next);
                let current = self.slots.get(index)?.chord;
                let flats = self.flats();
                let tonic = self.settings.tonic;
                let height = (options.len() as f32 * 26.0 + 10.0).min(340.0);
                let (left, top) = clamp(point, 200.0, height);
                Some(
                    menu(left, top, 200.0, height)
                        .id("chordgen-swap-menu")
                        .overflow_y_scroll()
                        .children(options.into_iter().enumerate().map(|(i, chord)| {
                            row(
                                ("chordgen-swap-item", i).into(),
                                chord.name(flats),
                                chord.roman(tonic),
                                chord == current,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.popover = None;
                                    if let Some(slot) = this.slots.get_mut(index) {
                                        slot.chord = chord;
                                        slot.locked = true;
                                    }
                                    this.audition_slot(index, cx);
                                },
                            ))
                        }))
                        .into_any_element(),
                )
            }
        }
    }
}

impl Render for ChordGeneratorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dragging = self.press.is_some_and(|p| p.dragging);
        // While a press is live, listen window-wide: the pointer leaves this
        // window on its way to the arrangement, and hover-scoped handlers
        // would stop hearing it.
        let tracker = self.press.is_some().then(|| {
            let this = cx.weak_entity();
            let up = this.clone();
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _w, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        let _ = this.update(cx, |this, cx| this.pointer_moved(event, _w, cx));
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _w, cx| {
                        if phase != gpui::DispatchPhase::Bubble || event.button != MouseButton::Left
                        {
                            return;
                        }
                        let over = ChordGeneratorWindow::over_self(event.position, _w);
                        let _ = up.update(cx, |this, cx| {
                            this.finish_press(Some((event.position, over)), cx)
                        });
                    });
                },
            )
            .absolute()
            .inset_0()
        });
        let popover = self.popover(window, cx);
        let wheel = self.wheel(window, cx);
        let selected_detail = self
            .selected
            .and_then(|i| self.slots.get(i))
            .map(|slot| {
                let flats = self.flats();
                slot.chord
                    .pitch_classes()
                    .into_iter()
                    .map(|pc| pitch_name(pc, flats))
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default();
        let scale_summary = format!(
            "{} {}",
            pitch_name(self.settings.tonic, self.flats()),
            self.settings.scale.label()
        );

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(crate::theme::ui_font())
            .when(dragging, |root| root.cursor(gpui::CursorStyle::ClosedHand))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" if this.press.is_some() => this.finish_press(None, cx),
                    "escape" if this.popover.is_some() => {
                        this.popover = None;
                        cx.notify();
                    }
                    "escape" => this.close(window, cx),
                    "space" => this.toggle_playback(cx),
                    "g" if !event.keystroke.modifiers.modified() => this.regenerate(cx),
                    _ => {}
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.popover.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            // A quick click can release before the window-wide listener above
            // is painted; the root hears that release (always over this
            // window) and finishes the press — once, whichever arrives first.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.finish_press(Some((event.position, true)), cx);
                }),
            )
            .child(external_window_titlebar(
                "Chord Generator",
                "chordgen-close",
                {
                    let entity = cx.entity().downgrade();
                    move |window, cx| {
                        let _ = entity.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(self.controls(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .gap(px(space::SECTION))
                    .p(px(space::SECTION))
                    .child(
                        div()
                            .flex_none()
                            .w(px(WHEEL_SIZE))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(space::BASE))
                            .child(wheel)
                            .child(
                                div()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_muted())
                                    .child(selected_detail),
                            )
                            .child(
                                div()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_faint())
                                    .child(scale_summary),
                            ),
                    )
                    .child(self.progression(cx)),
            )
            .child(self.footer(cx))
            .children(popover)
            .children(tracker)
    }
}

/// Open the window, centred on the Studio.
pub fn open_chord_generator_window(
    owner_bounds: Option<Bounds<Pixels>>,
    remembered: Option<Bounds<Pixels>>,
    callbacks: ChordGeneratorCallbacks,
    cx: &mut App,
) -> Result<WindowHandle<ChordGeneratorWindow>, String> {
    let window_size = size(px(CHORD_GENERATOR_WIDTH), px(CHORD_GENERATOR_HEIGHT));
    let bounds =
        remembered.unwrap_or_else(|| centered_window_bounds(owner_bounds, window_size, cx));
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(bounds));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(px(MIN_WIDTH), px(MIN_HEIGHT)));
    apply_owner_display(&mut options, owner_bounds, cx);
    cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| {
            let mut generator = ChordGeneratorWindow::new(callbacks, cx);
            generator._activation =
                Some(cx.observe_window_activation(window, |this, window, cx| {
                    this.set_window_active(window.is_window_active(), cx);
                }));
            generator
        });
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        view
    })
    .map_err(|e| e.to_string())
}

// ── Small pieces ────────────────────────────────────────────────────────────

fn time_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED)
}

fn format_bars(bars: f64) -> String {
    if (bars - bars.round()).abs() < 1e-9 {
        format!("{}", bars.round() as i64)
    } else {
        format!("{bars:.1}")
    }
}

fn segment_position(index: usize, count: usize) -> FbSegment {
    match (index, count) {
        (_, 0 | 1) => FbSegment::Only,
        (0, _) => FbSegment::First,
        (i, n) if i + 1 == n => FbSegment::Last,
        _ => FbSegment::Middle,
    }
}

fn labeled_row(label: &'static str, content: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .child(div().w(px(52.0)).flex_none().child(fb_section_label(label)))
        .child(div().flex_1().min_w(px(0.0)).child(content))
}

fn card_icon_button(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    tooltip: &'static str,
    latched: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let color = if latched {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    div()
        .id(id)
        .size(px(crate::theme::size::DENSE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL_SM))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| s.bg(Colors::surface_control_hover()))
        .tooltip(fb_tooltip(tooltip))
        // The card starts a drag on mouse-down; its buttons must not.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .child(
            gpui::svg()
                .path(icon)
                .w(px(12.0))
                .h(px(12.0))
                .text_color(color),
        )
}

/// Two-octave keyboard (C3–B4) with the chord's upper voices folded into it
/// and the bass, when voiced, as a marked key in the lowest octave.
fn mini_keyboard(notes: &[u8], has_bass: bool) -> impl IntoElement {
    const WHITE: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    const BLACK: [(u8, f32); 5] = [(1, 1.0), (3, 2.0), (6, 4.0), (8, 5.0), (10, 6.0)];
    let key_w = (CARD_W - 2.0 * space::BASE) / 14.0;
    let height = 28.0;
    let bass = if has_bass {
        notes.first().copied()
    } else {
        None
    };
    let upper: Vec<u8> = notes
        .iter()
        .skip(usize::from(has_bass))
        .map(|&n| fold(n))
        .collect();
    let bass_key = bass.map(|n| KEYS_LOW + n % 12);
    let accent = Colors::accent_primary();
    let lit = move |note: u8| upper.contains(&note);
    let whites = (0..14).map(move |i| {
        let note = KEYS_LOW + 12 * (i / 7) as u8 + WHITE[i % 7];
        let on = lit(note);
        let is_bass = bass_key == Some(note);
        div()
            .absolute()
            .left(px(i as f32 * key_w))
            .top_0()
            .w(px(key_w - 1.0))
            .h(px(height))
            .rounded_b(px(radius::MICRO))
            .bg(if on {
                accent
            } else if is_bass {
                Colors::with_alpha(accent, 0.35)
            } else {
                Colors::with_alpha(Colors::text_primary(), 0.82)
            })
    });
    let upper_black: Vec<u8> = notes
        .iter()
        .skip(usize::from(has_bass))
        .map(|&n| fold(n))
        .collect();
    let blacks = (0..2).flat_map(move |octave| {
        let upper_black = upper_black.clone();
        BLACK.iter().map(move |(pc, slot)| {
            let note = KEYS_LOW + 12 * octave + pc;
            let on = upper_black.contains(&note);
            let is_bass = bass_key == Some(note);
            let x = (octave as f32 * 7.0 + slot) * key_w - key_w * 0.32;
            div()
                .absolute()
                .left(px(x))
                .top_0()
                .w(px(key_w * 0.62))
                .h(px(height * 0.6))
                .rounded_b(px(radius::MICRO))
                .bg(if on {
                    accent
                } else if is_bass {
                    Colors::with_alpha(accent, 0.35)
                } else {
                    Colors::surface_canvas()
                })
        })
    });
    div()
        .relative()
        .w(px(key_w * 14.0))
        .h(px(height))
        .children(whites)
        .children(blacks)
}

/// Fold a note into the mini keyboard's two octaves.
fn fold(note: u8) -> u8 {
    let mut n = note;
    while n > KEYS_HIGH {
        n -= 12;
    }
    while n < KEYS_LOW {
        n += 12;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_keeps_pitch_class_inside_the_two_octaves() {
        for note in 0..=127u8 {
            let folded = fold(note);
            assert!((KEYS_LOW..=KEYS_HIGH).contains(&folded));
            assert_eq!(folded % 12, note % 12);
        }
    }

    #[test]
    fn bars_read_naturally() {
        assert_eq!(format_bars(4.0), "4");
        assert_eq!(format_bars(2.5), "2.5");
    }
}
