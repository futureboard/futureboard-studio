//! Chord Generator: an independent utility window for writing a progression
//! on a small timeline, then handing it to the arrangement.
//!
//! The window is a three-lane timeline:
//!
//! - **Chords** — chord regions. A progression pattern from the browser is
//!   dropped onto it; each chord can then be moved, stretched, swapped for an
//!   alternative, re-rolled, locked or deleted.
//! - **Comp** and **Bass** — performance-pattern regions (sustain, pulse,
//!   arpeggio, strum…; root, walking…) saying how the chords above them are
//!   played. Where no pattern lies, that lane is silent.
//!
//! Regions move by their body and stretch by their right edge, snapped to the
//! beat; a region overwrites what it lands on in its lane. Every gesture is a
//! live preview committed once on release, and every edit is one undo step
//! (window-local: nothing here touches the project until it is exported).
//!
//! Export goes to the Studio as a [`ChordGeneratorCommand`]: the chords to the
//! Chord Track, or the rendered Comp + Bass performance as one MIDI clip —
//! from the footer buttons at the playhead, or by dragging the footer handle
//! onto the arrangement, where the Studio resolves the drop against the main
//! window's live pointer.

mod model;

use std::cell::Cell;
use std::f32::consts::TAU;
use std::rc::Rc;
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
    alternatives, generate, pitch_name, progression_patterns, voice_progression, Chord,
    Colors as ChordColors, GeneratorSettings, Richness, Rng, Scale, Style, VoicingOptions,
};
use sphere_midi_service::performance::{
    render_performance, BassPattern, CompPattern, GeneratedNote,
};

use self::model::{snap, Arrangement, Content, History, Lane, Region, BEATS_PER_BAR, MIN_LENGTH};
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

pub const CHORD_GENERATOR_WIDTH: f32 = 1120.0;
pub const CHORD_GENERATOR_HEIGHT: f32 = 720.0;
const MIN_WIDTH: f32 = 980.0;
const MIN_HEIGHT: f32 = 640.0;
const BROWSER_W: f32 = 236.0;
const INSPECTOR_W: f32 = 232.0;
const WHEEL_SIZE: f32 = 168.0;
/// Timeline geometry: lane label column, ruler, lane heights, region inset,
/// stretch handle width.
const LABEL_W: f32 = 64.0;
const RULER_H: f32 = 22.0;
const REGION_INSET: f32 = 3.0;
const EDGE_W: f32 = 6.0;
/// Beats a region snaps to.
const GRID: f64 = 1.0;
/// Length of each chord of a dropped progression pattern.
const PATTERN_CHORD_BEATS: f64 = BEATS_PER_BAR;
/// Progressions generated this session, kept in the browser.
const RECENT: usize = 12;
/// Pointer travel before a press becomes a drag.
const DRAG_THRESHOLD: f32 = 4.0;
const FRAME: Duration = Duration::from_millis(16);
const SETTLE_SECONDS: f32 = 0.12;
/// How long a clicked chord rings.
const AUDITION_SECONDS: f64 = 1.4;
const LENGTHS: [u32; 4] = [4, 8, 16, 32];

fn lane_height(lane: Lane) -> f32 {
    match lane {
        Lane::Chords => 60.0,
        Lane::Comp | Lane::Bass => 44.0,
    }
}

/// What the window asks the Studio to do.
pub enum ChordGeneratorCommand {
    /// Lay the chords on the Chord Track, the first one `offset_beats` after
    /// the playhead.
    PlaceOnChordTrack {
        chords: Vec<ChordPlacement>,
        offset_beats: f64,
    },
    /// One MIDI clip of `length_beats` at the playhead on the selected MIDI
    /// track (or a new one), holding `notes` (beats from the clip start).
    CreateMidiClip {
        notes: Vec<GeneratedNote>,
        length_beats: f64,
    },
    /// The pointer moved while dragging: update the drop ghost.
    DragMove {
        chords: Vec<ChordPlacement>,
        offset_beats: f64,
    },
    /// Released: drop where the ghost was — chords on the Chord Track, or the
    /// notes as a clip on a track.
    DragEnd {
        chords: Vec<ChordPlacement>,
        offset_beats: f64,
        notes: Vec<GeneratedNote>,
        length_beats: f64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionMode {
    Move,
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BrowserItem {
    /// Index into [`ChordGeneratorWindow::library`].
    Progression(usize),
    Comp(CompPattern),
    Bass(BassPattern),
}

impl BrowserItem {
    fn lane(self) -> Lane {
        match self {
            BrowserItem::Progression(_) => Lane::Chords,
            BrowserItem::Comp(_) => Lane::Comp,
            BrowserItem::Bass(_) => Lane::Bass,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PressKind {
    Region {
        id: u64,
        mode: RegionMode,
        original: Region,
    },
    Browser(BrowserItem),
    /// The footer handle: the whole timeline to the arrangement.
    Export,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    kind: PressKind,
    origin: Point<Pixels>,
    dragging: bool,
}

/// Where a browser item would land.
#[derive(Debug, Clone, PartialEq)]
struct Ghost {
    lane: Lane,
    start: f64,
    length: f64,
    /// The region the drop would restyle rather than overwrite.
    onto: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserTab {
    Progressions,
    Performance,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Popover {
    Scale(Point<Pixels>),
}

/// A progression in the browser.
#[derive(Debug, Clone)]
struct LibraryEntry {
    chords: Vec<Chord>,
    generated: bool,
}

/// Notes being played through the audition callback.
struct Playback {
    notes: Vec<GeneratedNote>,
    next: usize,
    /// `(pitch, end beat)` of notes sounding now.
    sounding: Vec<(u8, f64)>,
    beat: f64,
    end: f64,
    bpm: f64,
    /// Draw the playhead (timeline playback, not an audition).
    timeline: bool,
}

pub struct ChordGeneratorWindow {
    callbacks: ChordGeneratorCallbacks,
    focus: FocusHandle,
    settings: GeneratorSettings,
    seed: u64,
    voicing: VoicingOptions,
    arrangement: Arrangement,
    /// The arrangement as a live gesture would leave it.
    preview: Option<Arrangement>,
    history: History,
    selected: Option<u64>,
    tab: BrowserTab,
    /// Progressions generated this session, newest first.
    recent: Vec<Vec<Chord>>,
    press: Option<Press>,
    ghost: Option<Ghost>,
    popover: Option<Popover>,
    playback: Option<Playback>,
    status: Option<String>,
    /// Painted bounds of the timeline's content column (ruler + lanes), for
    /// mapping a browser drag onto a lane.
    lanes_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    // Wheel animation.
    shown_energy: [f32; 12],
    ticking: bool,
    last_tick: Instant,
    window_active: bool,
    _activation: Option<gpui::Subscription>,
}

impl ChordGeneratorWindow {
    fn new(callbacks: ChordGeneratorCallbacks, cx: &mut Context<Self>) -> Self {
        let mut window = Self {
            callbacks,
            focus: cx.focus_handle(),
            settings: GeneratorSettings::default(),
            seed: time_seed(),
            voicing: VoicingOptions {
                bass: false,
                ..VoicingOptions::default()
            },
            arrangement: Arrangement::default(),
            preview: None,
            history: History::default(),
            selected: None,
            tab: BrowserTab::Progressions,
            recent: Vec::new(),
            press: None,
            ghost: None,
            popover: None,
            playback: None,
            status: None,
            lanes_bounds: Rc::new(Cell::new(None)),
            shown_energy: [0.0; 12],
            ticking: false,
            last_tick: Instant::now(),
            window_active: true,
            _activation: None,
        };
        // Open on a fresh progression with a sustained comp and a root bass,
        // so Play sounds straight away.
        let chords = generate(&window.settings, window.seed);
        window.remember(chords.clone());
        window.place_progression(&chords, 0.0);
        window.selected = window.arrangement.lane(Lane::Chords).next().map(|r| r.id);
        window.ensure_ticking(cx);
        window
    }

    // ── State helpers ───────────────────────────────────────────────────────

    fn flats(&self) -> bool {
        self.settings.scale.prefers_flats(self.settings.tonic)
    }

    /// What the timeline shows: the live gesture's result, or the committed
    /// arrangement.
    fn shown(&self) -> &Arrangement {
        self.preview.as_ref().unwrap_or(&self.arrangement)
    }

    /// One undoable edit.
    fn edit(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Self)) {
        let before = self.arrangement.clone();
        change(self);
        if self.arrangement != before {
            self.history.record(before);
        }
        if self
            .selected
            .is_some_and(|id| self.arrangement.find(id).is_none())
        {
            self.selected = None;
        }
        self.ensure_ticking(cx);
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.cancel_gesture();
        if self.history.undo(&mut self.arrangement) {
            self.status = Some("Undone".to_string());
        }
        self.drop_stale_selection();
        cx.notify();
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.cancel_gesture();
        if self.history.redo(&mut self.arrangement) {
            self.status = Some("Redone".to_string());
        }
        self.drop_stale_selection();
        cx.notify();
    }

    fn drop_stale_selection(&mut self) {
        if self
            .selected
            .is_some_and(|id| self.arrangement.find(id).is_none())
        {
            self.selected = None;
        }
    }

    fn remember(&mut self, chords: Vec<Chord>) {
        self.recent.retain(|c| *c != chords);
        self.recent.insert(0, chords);
        self.recent.truncate(RECENT);
    }

    /// Browser progressions: this session's generated ones, then the style's
    /// templates in the current key.
    fn library(&self) -> Vec<LibraryEntry> {
        self.recent
            .iter()
            .map(|chords| LibraryEntry {
                chords: chords.clone(),
                generated: true,
            })
            .chain(
                progression_patterns(&self.settings)
                    .into_iter()
                    .map(|chords| LibraryEntry {
                        chords,
                        generated: false,
                    }),
            )
            .collect()
    }

    /// Place a progression at `start` (one bar per chord). A Comp or Bass
    /// lane with nothing on it at all gets a plain pattern under the new
    /// chords, so they sound; a lane already in use is left as written.
    fn place_progression(&mut self, chords: &[Chord], start: f64) -> Vec<u64> {
        let items: Vec<(Content, f64)> = chords
            .iter()
            .map(|&chord| {
                (
                    Content::Chord {
                        chord,
                        locked: false,
                    },
                    PATTERN_CHORD_BEATS,
                )
            })
            .collect();
        let ids = self.arrangement.place_sequence(start, &items);
        let length = chords.len() as f64 * PATTERN_CHORD_BEATS;
        if self.arrangement.lane(Lane::Comp).next().is_none() {
            self.arrangement
                .place(start, length, Content::Comp(CompPattern::Sustain));
        }
        if self.arrangement.lane(Lane::Bass).next().is_none() {
            self.arrangement
                .place(start, length, Content::Bass(BassPattern::Root));
        }
        ids
    }

    // ── Progression edits ───────────────────────────────────────────────────

    /// A new progression over the Chords lane from the start, keeping locked
    /// chords where they are.
    fn regenerate(&mut self, cx: &mut Context<Self>) {
        let mut rng = Rng::new(self.seed ^ time_seed());
        self.seed = rng.next_u64();
        let chords = generate(&self.settings, self.seed);
        self.remember(chords.clone());
        self.edit(cx, |this| {
            let locked: Vec<Region> = this
                .arrangement
                .lane(Lane::Chords)
                .filter(|r| matches!(r.content, Content::Chord { locked: true, .. }))
                .copied()
                .collect();
            this.arrangement.clear_lane(Lane::Chords, |_| true);
            this.place_progression(&chords, 0.0);
            for region in locked {
                this.arrangement
                    .place(region.start, region.length, region.content);
            }
            this.selected = this.arrangement.lane(Lane::Chords).next().map(|r| r.id);
        });
        self.status = Some(format!(
            "New {} progression{}",
            self.settings.style.label(),
            if self
                .arrangement
                .lane(Lane::Chords)
                .any(|r| matches!(r.content, Content::Chord { locked: true, .. }))
            {
                " — locked chords kept"
            } else {
                ""
            }
        ));
    }

    fn set_tonic(&mut self, tonic: u8, cx: &mut Context<Self>) {
        let delta = tonic as i32 - self.settings.tonic as i32;
        if delta == 0 {
            return;
        }
        // Take the shorter way round, so a key change moves the song by at
        // most a tritone.
        let delta = if delta > 6 {
            delta - 12
        } else if delta < -6 {
            delta + 12
        } else {
            delta
        };
        self.settings.tonic = tonic;
        self.edit(cx, |this| this.arrangement.transpose(delta));
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut GeneratorSettings),
    ) {
        change(&mut self.settings);
        cx.notify();
    }

    fn selected_region(&self) -> Option<Region> {
        self.selected
            .and_then(|id| self.arrangement.find(id))
            .copied()
    }

    /// The chord after `region` on the Chords lane.
    fn next_chord(&self, region: &Region) -> Option<Chord> {
        self.arrangement
            .lane(Lane::Chords)
            .find(|r| r.start >= region.end() - 1e-9)
            .and_then(|r| r.content.chord())
    }

    fn set_chord(&mut self, id: u64, chord: Chord, cx: &mut Context<Self>) {
        self.edit(cx, |this| {
            this.arrangement.set_content(
                id,
                Content::Chord {
                    chord,
                    locked: true,
                },
            );
        });
        self.audition_region(id, cx);
    }

    fn reroll(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(region) = self.arrangement.find(id).copied() else {
            return;
        };
        let Some(current) = region.content.chord() else {
            return;
        };
        let choices: Vec<Chord> = alternatives(&self.settings, self.next_chord(&region))
            .into_iter()
            .filter(|c| *c != current)
            .collect();
        if choices.is_empty() {
            return;
        }
        let mut rng = Rng::new(time_seed() ^ id);
        let chord = choices[rng.below(choices.len())];
        self.edit(cx, |this| {
            this.arrangement.set_content(
                id,
                Content::Chord {
                    chord,
                    locked: false,
                },
            );
        });
        self.audition_region(id, cx);
    }

    fn toggle_lock(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit(cx, |this| {
            if let Some(Content::Chord { chord, locked }) =
                this.arrangement.find(id).map(|r| r.content)
            {
                this.arrangement.set_content(
                    id,
                    Content::Chord {
                        chord,
                        locked: !locked,
                    },
                );
            }
        });
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            self.edit(cx, |this| {
                this.arrangement.remove(id);
                this.selected = None;
            });
        }
    }

    fn set_bars(&mut self, bars: u32, cx: &mut Context<Self>) {
        self.edit(cx, |this| this.arrangement.set_bars(bars));
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.stop_playback(cx);
        self.edit(cx, |this| {
            for lane in Lane::ALL {
                this.arrangement.clear_lane(lane, |_| true);
            }
            this.selected = None;
        });
        self.status = Some("Timeline cleared — Undo brings it back".to_string());
    }

    /// Add a browser item without dragging: a progression after the last
    /// chord; a pattern over the whole progression.
    fn add_item(&mut self, item: BrowserItem, cx: &mut Context<Self>) {
        let start = self.arrangement.chords_end();
        match item {
            BrowserItem::Progression(index) => {
                let Some(entry) = self.library().get(index).cloned() else {
                    return;
                };
                self.edit(cx, |this| {
                    let ids = this.place_progression(&entry.chords, start);
                    this.selected = ids.first().copied();
                });
                self.status = Some(format!("Added {} chords", entry.chords.len()));
            }
            BrowserItem::Comp(_) | BrowserItem::Bass(_) => {
                let end = self.arrangement.chords_end().max(BEATS_PER_BAR);
                self.drop_pattern(item, 0.0, end, None, cx);
            }
        }
    }

    fn content_for(item: BrowserItem) -> Option<Content> {
        match item {
            BrowserItem::Comp(p) => Some(Content::Comp(p)),
            BrowserItem::Bass(p) => Some(Content::Bass(p)),
            BrowserItem::Progression(_) => None,
        }
    }

    fn drop_pattern(
        &mut self,
        item: BrowserItem,
        start: f64,
        length: f64,
        onto: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(content) = Self::content_for(item) else {
            return;
        };
        self.edit(cx, |this| match onto {
            Some(id) => {
                this.arrangement.set_content(id, content);
                this.selected = Some(id);
            }
            None => this.selected = this.arrangement.place(start, length, content),
        });
    }

    // ── Rendering to notes ──────────────────────────────────────────────────

    fn performance_notes(&self, arrangement: &Arrangement) -> Vec<GeneratedNote> {
        render_performance(
            &arrangement.chord_spans(),
            &arrangement.comp_spans(),
            &arrangement.bass_spans(),
            self.voicing,
        )
    }

    /// Chords for the Chord Track: each lasts until the next one starts (a
    /// gap holds the chord before it); `offset` is where the first starts.
    fn placements(&self) -> (f64, Vec<ChordPlacement>) {
        let chords: Vec<&Region> = self.arrangement.lane(Lane::Chords).collect();
        let flats = self.flats();
        let offset = chords.first().map_or(0.0, |r| r.start);
        let placements = chords
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let length = chords
                    .get(i + 1)
                    .map_or(r.length, |next| next.start - r.start);
                r.content.chord().map(|chord| ChordPlacement {
                    chord,
                    flats,
                    length_beats: length,
                })
            })
            .collect();
        (offset, placements)
    }

    /// The whole timeline as one clip's notes and length.
    fn clip(&self) -> (Vec<GeneratedNote>, f64) {
        let notes = self.performance_notes(&self.arrangement);
        let end = notes
            .iter()
            .map(|n| n.start + n.length)
            .fold(self.arrangement.content_end(), f64::max);
        (notes, end)
    }

    // ── Playback and audition ───────────────────────────────────────────────

    fn bpm(&self, cx: &App) -> f64 {
        (self.callbacks.project_bpm)(cx).clamp(20.0, 400.0) as f64
    }

    fn start_playback(
        &mut self,
        notes: Vec<GeneratedNote>,
        end: f64,
        timeline: bool,
        cx: &mut Context<Self>,
    ) {
        self.stop_playback(cx);
        if notes.is_empty() {
            self.status = Some(if timeline {
                "Nothing to play — add a Comp or Bass pattern under the chords".to_string()
            } else {
                "Nothing to play".to_string()
            });
            cx.notify();
            return;
        }
        self.playback = Some(Playback {
            notes,
            next: 0,
            sounding: Vec::new(),
            beat: 0.0,
            end,
            bpm: self.bpm(cx),
            timeline,
        });
        self.last_tick = Instant::now();
        self.advance_playback(0.0, cx);
        self.ensure_ticking(cx);
        cx.notify();
    }

    fn stop_playback(&mut self, cx: &mut Context<Self>) {
        if let Some(playback) = self.playback.take() {
            let pitches: Vec<u8> = playback.sounding.iter().map(|(p, _)| *p).collect();
            if !pitches.is_empty() {
                (self.callbacks.audition)(&pitches, false, cx);
            }
            cx.notify();
        }
    }

    /// Advance the playback by `beats`: note-offs, then note-ons.
    fn advance_playback(&mut self, beats: f64, cx: &mut Context<Self>) {
        let Some(mut playback) = self.playback.take() else {
            return;
        };
        playback.beat += beats;
        let now = playback.beat;
        let ended: Vec<u8> = playback
            .sounding
            .iter()
            .filter(|(_, end)| *end <= now)
            .map(|(p, _)| *p)
            .collect();
        playback.sounding.retain(|(_, end)| *end > now);
        if !ended.is_empty() {
            (self.callbacks.audition)(&ended, false, cx);
        }
        let mut starting = Vec::new();
        while let Some(note) = playback.notes.get(playback.next) {
            if note.start > now {
                break;
            }
            starting.push(note.pitch);
            playback
                .sounding
                .push((note.pitch, note.start + note.length));
            playback.next += 1;
        }
        if !starting.is_empty() && !(self.callbacks.audition)(&starting, true, cx) {
            self.status = Some("Select an instrument track to hear the chords".to_string());
            self.playback = Some(playback);
            self.stop_playback(cx);
            return;
        }
        let finished = playback.next >= playback.notes.len()
            && playback.sounding.is_empty()
            && now >= playback.end;
        self.playback = Some(playback);
        if finished {
            self.stop_playback(cx);
        }
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if self.playback.as_ref().is_some_and(|p| p.timeline) {
            self.stop_playback(cx);
            return;
        }
        let (notes, end) = self.clip();
        self.start_playback(notes, end, true, cx);
    }

    /// Sound one chord region, voiced as it sits in the progression, with
    /// its bass note.
    fn audition_region(&mut self, id: u64, cx: &mut Context<Self>) {
        let spans = self.arrangement.chord_spans();
        let Some(index) = spans.iter().position(|s| {
            self.arrangement
                .find(id)
                .is_some_and(|r| (r.start - s.start).abs() < 1e-9 && r.lane() == Lane::Chords)
        }) else {
            return;
        };
        let voiced = voice_progression(
            &spans.iter().map(|s| s.chord).collect::<Vec<_>>(),
            VoicingOptions {
                bass: true,
                ..self.voicing
            },
        );
        let length = AUDITION_SECONDS * self.bpm(cx) / 60.0;
        let notes = voiced[index]
            .iter()
            .map(|&pitch| GeneratedNote {
                pitch,
                start: 0.0,
                length,
                velocity: 88,
            })
            .collect();
        self.start_playback(notes, length, false, cx);
    }

    /// Hear a browser item: a progression as held chords, a pattern played
    /// over the current chords.
    fn audition_item(&mut self, item: BrowserItem, cx: &mut Context<Self>) {
        let mut trial = Arrangement::default();
        match item {
            BrowserItem::Progression(index) => {
                let Some(entry) = self.library().get(index).cloned() else {
                    return;
                };
                let items: Vec<(Content, f64)> = entry
                    .chords
                    .iter()
                    .map(|&chord| {
                        (
                            Content::Chord {
                                chord,
                                locked: false,
                            },
                            PATTERN_CHORD_BEATS,
                        )
                    })
                    .collect();
                let length = items.len() as f64 * PATTERN_CHORD_BEATS;
                trial.place_sequence(0.0, &items);
                trial.place(0.0, length, Content::Comp(CompPattern::Sustain));
                trial.place(0.0, length, Content::Bass(BassPattern::Root));
            }
            BrowserItem::Comp(_) | BrowserItem::Bass(_) => {
                // Two bars of the current chords with this pattern.
                trial = self.arrangement.clone();
                let end = trial.chords_end().min(2.0 * BEATS_PER_BAR);
                if end <= 0.0 {
                    self.status = Some("Add chords first to hear a pattern".to_string());
                    cx.notify();
                    return;
                }
                trial.clear_lane(item.lane(), |_| true);
                if let Some(content) = Self::content_for(item) {
                    trial.place(0.0, end, content);
                }
                let other = if item.lane() == Lane::Comp {
                    Lane::Bass
                } else {
                    Lane::Comp
                };
                trial.clear_lane(other, |_| true);
                let notes: Vec<GeneratedNote> = self
                    .performance_notes(&trial)
                    .into_iter()
                    .filter(|n| n.start < end)
                    .collect();
                self.start_playback(notes, end, false, cx);
                return;
            }
        }
        let notes = self.performance_notes(&trial);
        let end = trial.content_end();
        self.start_playback(notes, end, false, cx);
    }

    // ── Commands ────────────────────────────────────────────────────────────

    fn run(&mut self, command: ChordGeneratorCommand, cx: &mut Context<Self>) {
        if let Some(message) = (self.callbacks.on_command)(command, cx) {
            self.status = Some(message);
        }
        cx.notify();
    }

    fn export_chords(&mut self, cx: &mut Context<Self>) {
        let (offset_beats, chords) = self.placements();
        self.run(
            ChordGeneratorCommand::PlaceOnChordTrack {
                chords,
                offset_beats,
            },
            cx,
        );
    }

    fn export_clip(&mut self, cx: &mut Context<Self>) {
        let (notes, length_beats) = self.clip();
        if notes.is_empty() {
            self.status = Some("Nothing to write — the Comp and Bass lanes are empty".to_string());
            cx.notify();
            return;
        }
        self.run(
            ChordGeneratorCommand::CreateMidiClip {
                notes,
                length_beats,
            },
            cx,
        );
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .press
            .is_some_and(|p| p.dragging && p.kind == PressKind::Export)
        {
            (self.callbacks.on_command)(ChordGeneratorCommand::DragCancel, cx);
        }
        self.stop_playback(cx);
        (self.callbacks.on_close)(window.bounds(), cx);
        window.remove_window();
    }

    // ── Gestures ────────────────────────────────────────────────────────────

    /// Pixels per beat and the content column's width, from the window's
    /// fixed layout.
    fn pixels_per_beat(&self, window: &Window) -> f32 {
        let width = timeline_content_width(window);
        width / self.shown().total_beats().max(1.0) as f32
    }

    fn begin_press(&mut self, kind: PressKind, origin: Point<Pixels>, cx: &mut Context<Self>) {
        self.popover = None;
        self.press = Some(Press {
            kind,
            origin,
            dragging: false,
        });
        cx.notify();
    }

    fn cancel_gesture(&mut self) {
        if let Some(press) = self.press.take() {
            if press.dragging && press.kind == PressKind::Export {
                // The Studio's ghost is cleared by the caller's DragCancel.
            }
        }
        self.preview = None;
        self.ghost = None;
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
            self.finish_press(None, window, cx);
            return;
        }
        let dx: f32 = (event.position.x - press.origin.x).into();
        let dy: f32 = (event.position.y - press.origin.y).into();
        if !press.dragging {
            if dx.hypot(dy) < DRAG_THRESHOLD {
                return;
            }
            press.dragging = true;
            self.press = Some(press);
        }
        match press.kind {
            PressKind::Region { id, mode, original } => {
                let ppb = self.pixels_per_beat(window).max(1.0);
                let delta = dx as f64 / ppb as f64;
                let mut preview = self.arrangement.clone();
                match mode {
                    RegionMode::Move => {
                        let start = snap(original.start + delta, GRID).max(0.0);
                        preview.move_region(id, start);
                        self.status = Some(format!("Start {}", bar_beat(start)));
                    }
                    RegionMode::Stretch => {
                        let length = snap(original.length + delta, GRID).max(MIN_LENGTH);
                        preview.resize_region(id, length);
                        self.status = Some(format!("Length {}", beats_label(length)));
                    }
                }
                self.preview = Some(preview);
            }
            PressKind::Browser(item) => {
                self.ghost = self.ghost_at(item, event.position, window);
                self.status = Some(match &self.ghost {
                    Some(g) if g.onto.is_some() => "Release to use this pattern here".to_string(),
                    Some(g) => format!("Release to place at {}", bar_beat(g.start)),
                    None => format!("Drop on the {} lane", item.lane().label()),
                });
            }
            PressKind::Export => {
                if Self::over_self(event.position, window) {
                    self.run(ChordGeneratorCommand::DragCancel, cx);
                    self.status = Some("Drag out over the Chord Track or a MIDI track".to_string());
                } else {
                    let (offset_beats, chords) = self.placements();
                    self.run(
                        ChordGeneratorCommand::DragMove {
                            chords,
                            offset_beats,
                        },
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// Where `item` would land under the pointer, if over its lane.
    fn ghost_at(
        &self,
        item: BrowserItem,
        position: Point<Pixels>,
        window: &Window,
    ) -> Option<Ghost> {
        let bounds = self.lanes_bounds.get()?;
        let x: f32 = (position.x - bounds.origin.x).into();
        let y: f32 = (position.y - bounds.origin.y).into();
        let width: f32 = bounds.size.width.into();
        if x < 0.0 || x > width {
            return None;
        }
        let lane = lane_at(y - RULER_H)?;
        if lane != item.lane() {
            return None;
        }
        let ppb = self.pixels_per_beat(window).max(1.0) as f64;
        let beat = (x as f64 / ppb).floor().max(0.0);
        let arrangement = &self.arrangement;
        match item {
            BrowserItem::Progression(index) => {
                let count = self.library().get(index)?.chords.len();
                Some(Ghost {
                    lane,
                    start: snap(beat, GRID),
                    length: count as f64 * PATTERN_CHORD_BEATS,
                    onto: None,
                })
            }
            BrowserItem::Comp(_) | BrowserItem::Bass(_) => {
                if let Some(region) = arrangement.region_at(lane, beat) {
                    return Some(Ghost {
                        lane,
                        start: region.start,
                        length: region.length,
                        onto: Some(region.id),
                    });
                }
                // Into empty space: up to the next region of the lane, or
                // over the rest of the chords (at least a bar).
                let next = arrangement
                    .lane(lane)
                    .map(|r| r.start)
                    .filter(|&s| s > beat)
                    .fold(f64::INFINITY, f64::min);
                let wanted = (arrangement.chords_end() - beat).max(BEATS_PER_BAR);
                Some(Ghost {
                    lane,
                    start: beat,
                    length: wanted.min(next - beat).max(MIN_LENGTH),
                    onto: None,
                })
            }
        }
    }

    /// End a press. `released_at` is `(point, over this window)` for a
    /// mouse-up; `None` when the gesture was abandoned.
    fn finish_press(
        &mut self,
        released_at: Option<(Point<Pixels>, bool)>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(press) = self.press.take() else {
            return;
        };
        let released = released_at.is_some();
        let over_self = released_at.is_some_and(|(_, over)| over);
        match press.kind {
            PressKind::Region { id, .. } => {
                let preview = self.preview.take();
                if press.dragging {
                    if let (true, Some(preview)) = (released, preview) {
                        self.edit(cx, |this| this.arrangement = preview);
                        self.selected = Some(id);
                    }
                    self.status = None;
                } else if released {
                    self.selected = Some(id);
                    if self
                        .arrangement
                        .find(id)
                        .is_some_and(|r| r.lane() == Lane::Chords)
                    {
                        self.audition_region(id, cx);
                    }
                }
            }
            PressKind::Browser(item) => {
                let ghost = self.ghost.take();
                if press.dragging {
                    if let (true, Some(ghost)) = (released && over_self, ghost) {
                        match item {
                            BrowserItem::Progression(index) => {
                                if let Some(entry) = self.library().get(index).cloned() {
                                    self.edit(cx, |this| {
                                        let ids =
                                            this.place_progression(&entry.chords, ghost.start);
                                        this.selected = ids.first().copied();
                                    });
                                    self.status =
                                        Some(format!("Placed {} chords", entry.chords.len()));
                                }
                            }
                            _ => {
                                self.drop_pattern(item, ghost.start, ghost.length, ghost.onto, cx);
                                self.status = None;
                            }
                        }
                    } else {
                        self.status = None;
                    }
                } else if released {
                    self.audition_item(item, cx);
                }
            }
            PressKind::Export => {
                if press.dragging {
                    if over_self || !released {
                        self.run(ChordGeneratorCommand::DragCancel, cx);
                        self.status = None;
                    } else {
                        let (offset_beats, chords) = self.placements();
                        let (notes, length_beats) = self.clip();
                        self.status = None;
                        self.run(
                            ChordGeneratorCommand::DragEnd {
                                chords,
                                offset_beats,
                                notes,
                                length_beats,
                            },
                            cx,
                        );
                    }
                }
            }
        }
        cx.notify();
    }

    // ── Wheel animation ─────────────────────────────────────────────────────

    /// The chord the wheel shows: under the playhead while the timeline
    /// plays, else the selected one.
    fn focus_chord(&self) -> Option<Chord> {
        if let Some(playback) = self.playback.as_ref().filter(|p| p.timeline) {
            return self
                .arrangement
                .region_at(Lane::Chords, playback.beat)
                .and_then(|r| r.content.chord());
        }
        self.selected
            .and_then(|id| self.arrangement.find(id))
            .and_then(|r| r.content.chord())
    }

    fn wheel_target(&self) -> [f32; 12] {
        let mut energy = [0.0; 12];
        if let Some(chord) = self.focus_chord() {
            for pc in chord.pitch_classes() {
                energy[pc as usize] = if pc == chord.root { 1.0 } else { 0.72 };
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
        let mut changed = false;
        if let Some(bpm) = self.playback.as_ref().map(|p| p.bpm) {
            self.advance_playback(dt as f64 * bpm / 60.0, cx);
            changed = true;
        }
        let target = self.wheel_target();
        let blend = if self.window_active {
            1.0 - (-dt / SETTLE_SECONDS).exp()
        } else {
            1.0
        };
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

    // ── Render: controls ────────────────────────────────────────────────────

    fn controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let flats = self.flats();
        let tonic = self.settings.tonic;
        let keys = fb_segmented_track().children((0..12u8).map(|pc| {
            fb_segment(
                ("chordgen-key", pc as usize),
                pitch_name(pc, flats),
                pc == tonic,
                segment_position(pc as usize, 12),
                cx.listener(move |this, _, _, cx| this.set_tonic(pc, cx)),
            )
        }));
        let scale_trigger = div().id("chordgen-scale").w(px(190.0)).child(
            crate::components::combo_box::combo_box_trigger(
                "chordgen-scale-trigger",
                self.settings.scale.label(),
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
                "COLOR",
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(div().w(px(150.0)).child(voicing))
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

    // ── Render: browser ─────────────────────────────────────────────────────

    fn browser(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = [
            (BrowserTab::Progressions, "Progressions"),
            (BrowserTab::Performance, "Patterns"),
        ];
        let tab_track =
            fb_segmented_track().children(tabs.iter().enumerate().map(|(index, (tab, label))| {
                let tab = *tab;
                fb_segment(
                    ("chordgen-tab", index),
                    *label,
                    self.tab == tab,
                    segment_position(index, tabs.len()),
                    cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }),
                )
            }));
        let pressed = self.press.and_then(|p| match p.kind {
            PressKind::Browser(item) => Some(item),
            _ => None,
        });
        let mut items: Vec<AnyElement> = Vec::new();
        match self.tab {
            BrowserTab::Progressions => {
                let flats = self.flats();
                let tonic = self.settings.tonic;
                let library = self.library();
                let generated = library.iter().filter(|e| e.generated).count();
                for (index, entry) in library.iter().enumerate() {
                    if index == 0 && generated > 0 {
                        items.push(list_heading("GENERATED"));
                    }
                    if index == generated {
                        items.push(list_heading("STYLE PATTERNS"));
                    }
                    let title = entry
                        .chords
                        .iter()
                        .map(|c| c.roman(tonic))
                        .collect::<Vec<_>>()
                        .join(" – ");
                    let detail = entry
                        .chords
                        .iter()
                        .map(|c| c.name(flats))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let item = BrowserItem::Progression(index);
                    items.push(self.browser_card(
                        ("chordgen-prog", index),
                        title,
                        Some(detail),
                        None,
                        item,
                        pressed == Some(item),
                        cx,
                    ));
                }
            }
            BrowserTab::Performance => {
                items.push(list_heading("COMP"));
                for (index, pattern) in CompPattern::ALL.iter().enumerate() {
                    let item = BrowserItem::Comp(*pattern);
                    items.push(self.browser_card(
                        ("chordgen-comp", index),
                        pattern.label().to_string(),
                        None,
                        Some(pattern.onsets()),
                        item,
                        pressed == Some(item),
                        cx,
                    ));
                }
                items.push(list_heading("BASS"));
                for (index, pattern) in BassPattern::ALL.iter().enumerate() {
                    let item = BrowserItem::Bass(*pattern);
                    items.push(self.browser_card(
                        ("chordgen-bass", index),
                        pattern.label().to_string(),
                        None,
                        Some(pattern.onsets()),
                        item,
                        pressed == Some(item),
                        cx,
                    ));
                }
            }
        }
        div()
            .flex_none()
            .w(px(BROWSER_W))
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(tab_track)
            .child(
                div()
                    .id("chordgen-browser")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .children(items),
            )
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child("Click to hear · double-click to add · drag onto a lane"),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn browser_card(
        &self,
        id: impl Into<gpui::ElementId>,
        title: String,
        detail: Option<String>,
        rhythm: Option<Vec<f64>>,
        item: BrowserItem,
        pressed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rest = Colors::surface_panel();
        let fill = if pressed {
            Colors::composite(rest, Colors::state_selected())
        } else {
            rest
        };
        let hover = Colors::composite(fill, Colors::state_hover());
        div()
            .id(id)
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(space::HAIR))
            .px(px(space::BASE))
            .py(px(space::SNUG))
            .rounded(px(radius::CONTROL))
            .bg(fill)
            .border(px(1.0))
            .border_color(if pressed {
                Colors::with_alpha(Colors::accent_primary(), 0.85)
            } else {
                Colors::border_subtle()
            })
            .cursor(gpui::CursorStyle::OpenHand)
            .hover(move |s| s.bg(hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        this.press = None;
                        this.add_item(item, cx);
                        return;
                    }
                    this.begin_press(PressKind::Browser(item), event.position, cx);
                }),
            )
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .truncate()
                    .child(title),
            )
            .children(detail.map(|d| {
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .truncate()
                    .child(d)
            }))
            .children(
                rhythm.map(|onsets| rhythm_glyph(&onsets, BROWSER_W - 2.0 * space::BASE - 2.0)),
            )
            .into_any_element()
    }

    // ── Render: timeline ────────────────────────────────────────────────────

    fn timeline(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let arrangement = self.shown();
        let bars = arrangement.bars();
        let total = arrangement.total_beats();
        let width = timeline_content_width(window);
        let ppb = width / total.max(1.0) as f32;
        let flats = self.flats();
        let tonic = self.settings.tonic;

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
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(space::BASE))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(space::BASE))
                    .child(fb_section_label("TIMELINE"))
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(format!(
                                "{} chords · {} bars",
                                arrangement.lane(Lane::Chords).count(),
                                bars
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child("Bars"),
                    )
                    .child(div().w(px(150.0)).child(length))
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
                    .top(px(space::TIGHT))
                    .text_size(px(typography::UI_XS))
                    .font_features(tabular_features())
                    .text_color(Colors::text_muted())
                    .child(format!("{}", bar + 1))
            }));

        let lanes = Lane::ALL.map(|lane| {
            let height = lane_height(lane);
            let regions: Vec<AnyElement> = arrangement
                .lane(lane)
                .map(|region| self.region(region, ppb, height, flats, tonic, cx))
                .collect();
            let ghost = self.ghost.as_ref().filter(|g| g.lane == lane).map(|g| {
                let x = g.start as f32 * ppb;
                let w = (g.length as f32 * ppb).max(2.0);
                let h = height - 2.0 * REGION_INSET;
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(REGION_INSET))
                    .w(px(w))
                    .h(px(h))
                    .rounded(px(radius::clamped(radius::CONTROL_SM, w, h)))
                    .bg(Colors::with_alpha(Colors::accent_primary(), 0.16))
                    .border(px(1.0))
                    .border_color(Colors::with_alpha(Colors::accent_primary(), 0.85))
            });
            let empty = arrangement.lane(lane).next().is_none() && self.ghost.is_none();
            div()
                .id(("chordgen-lane", lane as usize))
                .relative()
                .h(px(height))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                // A click on empty lane space clears the selection.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        if this.selected.take().is_some() {
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
                            .justify_center()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_faint())
                            .child(match lane {
                                Lane::Chords => "Drop a progression here",
                                Lane::Comp => "Drop a comp pattern — this lane is silent",
                                Lane::Bass => "Drop a bass pattern — this lane is silent",
                            }),
                    )
                })
                .children(regions)
                .children(ghost)
        });

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
            .children(Lane::ALL.map(|lane| {
                div()
                    .h(px(lane_height(lane)))
                    .px(px(space::BASE))
                    .flex()
                    .items_center()
                    .border_b(px(1.0))
                    .border_color(Colors::border_subtle())
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_secondary())
                    .child(lane.label())
            }));

        // Grid (bars lead, beats recede) under the regions; the painted
        // bounds are kept for mapping browser drags onto lanes.
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
            .children(lanes)
            .children(playhead);

        div()
            .flex_1()
            .min_w(px(0.0))
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
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child(
                        "Drag a region to move it, its right edge to stretch it · Delete removes · ⌘Z undoes",
                    ),
            )
    }

    fn region(
        &self,
        region: &Region,
        ppb: f32,
        lane_h: f32,
        flats: bool,
        tonic: u8,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let x = region.start as f32 * ppb;
        let w = (region.length as f32 * ppb).max(2.0);
        let h = lane_h - 2.0 * REGION_INSET;
        let selected = self.selected == Some(region.id);
        let rest = Colors::surface_raised();
        let fill = if selected {
            Colors::composite(rest, Colors::state_selected())
        } else {
            rest
        };
        let id = region.id;
        let original = *region;
        let (title, detail, locked) = match region.content {
            Content::Chord { chord, locked } => {
                (chord.name(flats), Some(chord.roman(tonic)), locked)
            }
            Content::Comp(p) => (p.label().to_string(), None, false),
            Content::Bass(p) => (p.label().to_string(), None, false),
        };
        let onsets = match region.content {
            Content::Comp(p) => Some(p.onsets()),
            Content::Bass(p) => Some(p.onsets()),
            Content::Chord { .. } => None,
        };
        let ticks = onsets.filter(|_| ppb * 0.5 >= 2.5).map(|onsets| {
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
                .h(px(5.0))
                .children(xs.into_iter().map(|x| {
                    div()
                        .absolute()
                        .left(px(x))
                        .top_0()
                        .w(px(2.0))
                        .h(px(5.0))
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
                        cx,
                    );
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(px(space::SNUG))
                    .top(px(space::TIGHT))
                    .right(px(EDGE_W))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(space::HAIR))
                            .child(
                                div()
                                    .text_size(px(if lane_h > 50.0 {
                                        typography::UI_MD
                                    } else {
                                        typography::UI_XS
                                    }))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(Colors::text_primary())
                                    .whitespace_nowrap()
                                    .child(title),
                            )
                            .when(locked, |row| {
                                row.child(
                                    gpui::svg()
                                        .path(assets::ICON_LOCK_PATH)
                                        .w(px(10.0))
                                        .h(px(10.0))
                                        .text_color(Colors::text_muted()),
                                )
                            }),
                    )
                    .children(detail.map(|d| {
                        div()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .whitespace_nowrap()
                            .child(d)
                    })),
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
                                cx,
                            );
                        }),
                    ),
            )
            .into_any_element()
    }

    // ── Render: inspector ───────────────────────────────────────────────────

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
                    .text_size(px(18.0))
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

    fn inspector(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let flats = self.flats();
        let tonic = self.settings.tonic;
        let wheel = self.wheel(window, cx);
        let mut body: Vec<AnyElement> = Vec::new();
        match self.selected_region() {
            Some(region) => {
                let id = region.id;
                body.push(
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
                );
                match region.content {
                    Content::Chord { chord, locked } => {
                        let notes = chord
                            .pitch_classes()
                            .into_iter()
                            .map(|pc| pitch_name(pc, flats))
                            .collect::<Vec<_>>()
                            .join(" · ");
                        body.push(
                            div()
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_secondary())
                                .child(notes)
                                .into_any_element(),
                        );
                        body.push(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(space::TIGHT))
                                .child(fb_button(
                                    "chordgen-lock",
                                    if locked { "Unlock" } else { "Lock" },
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
                                    cx.listener(|this, _, _, cx| this.delete_selected(cx)),
                                ))
                                .into_any_element(),
                        );
                        body.push(list_heading("ALTERNATIVES"));
                        let options = alternatives(&self.settings, self.next_chord(&region));
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
                                        option.name(flats),
                                        option.roman(tonic),
                                        option == chord,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.set_chord(id, option, cx),
                                    ))
                                }))
                                .into_any_element(),
                        );
                    }
                    Content::Comp(current) => {
                        body.push(list_heading("COMP PATTERN"));
                        body.push(
                            div()
                                .id("chordgen-comp-choices")
                                .flex_1()
                                .min_h(px(0.0))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .children(CompPattern::ALL.iter().enumerate().map(|(i, p)| {
                                    let p = *p;
                                    choice_row(
                                        ("chordgen-comp-pick", i),
                                        p.label().to_string(),
                                        String::new(),
                                        p == current,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.edit(cx, |this| {
                                                this.arrangement.set_content(id, Content::Comp(p));
                                            });
                                        },
                                    ))
                                }))
                                .into_any_element(),
                        );
                        body.push(delete_button(cx));
                    }
                    Content::Bass(current) => {
                        body.push(list_heading("BASS PATTERN"));
                        body.push(
                            div()
                                .id("chordgen-bass-choices")
                                .flex_1()
                                .min_h(px(0.0))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .children(BassPattern::ALL.iter().enumerate().map(|(i, p)| {
                                    let p = *p;
                                    choice_row(
                                        ("chordgen-bass-pick", i),
                                        p.label().to_string(),
                                        String::new(),
                                        p == current,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.edit(cx, |this| {
                                                this.arrangement.set_content(id, Content::Bass(p));
                                            });
                                        },
                                    ))
                                }))
                                .into_any_element(),
                        );
                        body.push(delete_button(cx));
                    }
                }
            }
            None => {
                body.push(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(format!(
                            "{} {} — select a chord or pattern on the timeline to edit it",
                            pitch_name(tonic, flats),
                            self.settings.scale.label()
                        ))
                        .into_any_element(),
                );
            }
        }
        div()
            .flex_none()
            .w(px(INSPECTOR_W))
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(div().flex().justify_center().child(wheel))
            .children(body)
    }

    // ── Render: footer and popover ──────────────────────────────────────────

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone().unwrap_or_else(|| {
            "Plays on the selected instrument track · export at the playhead or drag onto the arrangement"
                .to_string()
        });
        let has_chords = self.arrangement.lane(Lane::Chords).next().is_some();
        let playing = self.playback.as_ref().is_some_and(|p| p.timeline);
        let exporting = self
            .press
            .is_some_and(|p| p.dragging && p.kind == PressKind::Export);
        let handle_rest = Colors::surface_raised();
        let handle_hover = Colors::composite(handle_rest, Colors::state_hover());
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
                "chordgen-undo",
                "Undo",
                FbButtonKind::Ghost,
                true,
                cx.listener(|this, _, _, cx| this.undo(cx)),
            ))
            .child(fb_button(
                "chordgen-redo",
                "Redo",
                FbButtonKind::Ghost,
                true,
                cx.listener(|this, _, _, cx| this.redo(cx)),
            ))
            .child(fb_button(
                "chordgen-play",
                if playing { "Stop" } else { "Play" },
                FbButtonKind::Ghost,
                has_chords,
                cx.listener(|this, _, _, cx| this.toggle_playback(cx)),
            ))
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
                    .bg(if exporting {
                        Colors::composite(handle_rest, Colors::state_selected())
                    } else {
                        handle_rest
                    })
                    .border(px(1.0))
                    .border_color(if exporting {
                        Colors::with_alpha(Colors::accent_primary(), 0.85)
                    } else {
                        Colors::border_normal()
                    })
                    .cursor(gpui::CursorStyle::OpenHand)
                    .hover(move |s| s.bg(handle_hover))
                    .tooltip(fb_tooltip(
                        "Drag onto the Chord Track for the chords, or onto a MIDI track for the Comp and Bass as a clip",
                    ))
                    .when(has_chords, |el| {
                        el.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                                this.begin_press(PressKind::Export, event.position, cx);
                            }),
                        )
                    })
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
                            .text_color(if has_chords {
                                Colors::text_primary()
                            } else {
                                Colors::text_disabled()
                            })
                            .child("Drag to arrangement"),
                    ),
            )
            .child(fb_button(
                "chordgen-to-lane",
                "Add to Chord Track",
                FbButtonKind::Default,
                has_chords,
                cx.listener(|this, _, _, cx| this.export_chords(cx)),
            ))
            .child(fb_button(
                "chordgen-to-midi",
                "Create MIDI Clip",
                FbButtonKind::Default,
                has_chords,
                cx.listener(|this, _, _, cx| this.export_clip(cx)),
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
        let Popover::Scale(point) = self.popover?;
        let viewport = window.viewport_size();
        let (w, h) = (220.0_f32, 360.0_f32);
        let vw: f32 = viewport.width.into();
        let vh: f32 = viewport.height.into();
        let x: f32 = point.x.into();
        let y: f32 = point.y.into();
        let left = x.min(vw - w - space::BASE).max(space::BASE);
        let top = (y + space::TIGHT)
            .min(vh - h - space::BASE)
            .max(space::BASE);
        let current = self.settings.scale;
        Some(
            div()
                .id("chordgen-scale-menu")
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
                .overflow_y_scroll()
                // Clicks inside the menu must not reach the root's
                // click-away handler before the item's own click fires.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(Scale::ALL.iter().enumerate().map(|(index, scale)| {
                    let scale = *scale;
                    let detail = if scale.is_heptatonic() {
                        String::new()
                    } else {
                        "symmetric".to_string()
                    };
                    choice_row(
                        ("chordgen-scale-item", index),
                        scale.label().to_string(),
                        detail,
                        scale == current,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.popover = None;
                        this.update_settings(cx, |s| s.scale = scale);
                    }))
                }))
                .into_any_element(),
        )
    }
}

impl Render for ChordGeneratorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dragging = self.press.is_some_and(|p| p.dragging);
        // While a press is live, listen window-wide: region and browser drags
        // cross element bounds, and the export drag leaves this window on its
        // way to the arrangement.
        let tracker = self.press.is_some().then(|| {
            let this = cx.weak_entity();
            let up = this.clone();
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, w, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        let _ = this.update(cx, |this, cx| this.pointer_moved(event, w, cx));
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, w, cx| {
                        if phase != gpui::DispatchPhase::Bubble || event.button != MouseButton::Left
                        {
                            return;
                        }
                        let over = ChordGeneratorWindow::over_self(event.position, w);
                        let _ = up.update(cx, |this, cx| {
                            this.finish_press(Some((event.position, over)), w, cx)
                        });
                    });
                },
            )
            .absolute()
            .inset_0()
        });
        let popover = self.popover(window, cx);
        let inspector = self.inspector(window, cx);
        let timeline = self.timeline(window, cx);

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
                let modifiers = &event.keystroke.modifiers;
                let command = modifiers.platform || modifiers.control;
                match event.keystroke.key.as_str() {
                    "escape" if this.press.is_some() => {
                        let exporting = this
                            .press
                            .is_some_and(|p| p.dragging && p.kind == PressKind::Export);
                        this.cancel_gesture();
                        if exporting {
                            this.run(ChordGeneratorCommand::DragCancel, cx);
                        }
                        this.status = None;
                        cx.notify();
                    }
                    "escape" if this.popover.is_some() => {
                        this.popover = None;
                        cx.notify();
                    }
                    "escape" => this.close(window, cx),
                    "z" if command && modifiers.shift => this.redo(cx),
                    "z" if command => this.undo(cx),
                    "y" if command => this.redo(cx),
                    "backspace" | "delete" if this.press.is_none() => this.delete_selected(cx),
                    "space" => this.toggle_playback(cx),
                    "g" if !modifiers.modified() => this.regenerate(cx),
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
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.finish_press(Some((event.position, true)), window, cx);
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
                    .child(self.browser(cx))
                    .child(timeline)
                    .child(inspector),
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

/// Width of the timeline's content column: the window less its fixed-width
/// columns, padding and the lane labels.
fn timeline_content_width(window: &Window) -> f32 {
    let viewport: f32 = window.viewport_size().width.into();
    (viewport - 4.0 * space::SECTION - BROWSER_W - INSPECTOR_W - LABEL_W - 3.0).max(120.0)
}

/// The lane under `y`, measured from the top of the first lane.
fn lane_at(y: f32) -> Option<Lane> {
    let mut top = 0.0;
    for lane in Lane::ALL {
        let height = lane_height(lane);
        if y >= top && y < top + height {
            return Some(lane);
        }
        top += height;
    }
    None
}

fn tabular_features() -> gpui::FontFeatures {
    static FEATURES: std::sync::OnceLock<gpui::FontFeatures> = std::sync::OnceLock::new();
    FEATURES
        .get_or_init(|| gpui::FontFeatures(Arc::new(vec![("tnum".to_string(), 1)])))
        .clone()
}

fn time_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED)
}

/// `Bar 3 · beat 2` (1-based).
fn bar_beat(beat: f64) -> String {
    let bar = (beat / BEATS_PER_BAR).floor() as i64 + 1;
    let within = beat - (bar - 1) as f64 * BEATS_PER_BAR + 1.0;
    if (within - within.round()).abs() < 1e-9 {
        format!("Bar {bar} · beat {}", within.round() as i64)
    } else {
        format!("Bar {bar} · beat {within:.1}")
    }
}

fn beats_label(beats: f64) -> String {
    let bars = beats / BEATS_PER_BAR;
    if (bars - bars.round()).abs() < 1e-9 && bars >= 1.0 {
        let n = bars.round() as i64;
        format!("{n} {}", if n == 1 { "bar" } else { "bars" })
    } else if (beats - beats.round()).abs() < 1e-9 {
        let n = beats.round() as i64;
        format!("{n} {}", if n == 1 { "beat" } else { "beats" })
    } else {
        format!("{beats:.1} beats")
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

fn list_heading(label: &'static str) -> AnyElement {
    div()
        .pt(px(space::TIGHT))
        .child(fb_section_label(label))
        .into_any_element()
}

/// A selectable row in a list or menu.
fn choice_row(
    id: impl Into<gpui::ElementId>,
    label: String,
    detail: String,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    let rest = if active {
        Colors::composite(Colors::surface_card(), Colors::state_selected())
    } else {
        Colors::with_alpha(Colors::surface_card(), 0.0)
    };
    div()
        .id(id)
        .flex_none()
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
}

fn delete_button(cx: &mut Context<ChordGeneratorWindow>) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .child(fb_button(
            "chordgen-delete-pattern",
            "Delete",
            FbButtonKind::Ghost,
            true,
            cx.listener(|this, _, _, cx| this.delete_selected(cx)),
        ))
        .into_any_element()
}

/// A bar of the pattern's rhythm: a tick per hit, beats marked underneath.
fn rhythm_glyph(onsets: &[f64], width: f32) -> impl IntoElement {
    let per_beat = width / BEATS_PER_BAR as f32;
    div()
        .relative()
        .w(px(width))
        .h(px(10.0))
        .children((0..BEATS_PER_BAR as usize).map(move |beat| {
            div()
                .absolute()
                .left(px(beat as f32 * per_beat))
                .bottom_0()
                .w(px(1.0))
                .h(px(3.0))
                .bg(Colors::border_normal())
        }))
        .children(onsets.iter().map(move |&at| {
            div()
                .absolute()
                .left(px(at as f32 * per_beat))
                .top_0()
                .w(px(3.0))
                .h(px(6.0))
                .rounded(px(radius::MICRO))
                .bg(Colors::accent_primary())
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_read_as_bars_and_beats() {
        assert_eq!(bar_beat(0.0), "Bar 1 · beat 1");
        assert_eq!(bar_beat(5.0), "Bar 2 · beat 2");
        assert_eq!(bar_beat(6.5), "Bar 2 · beat 3.5");
        assert_eq!(beats_label(4.0), "1 bar");
        assert_eq!(beats_label(8.0), "2 bars");
        assert_eq!(beats_label(3.0), "3 beats");
        assert_eq!(beats_label(1.0), "1 beat");
    }

    #[test]
    fn lanes_stack_under_the_ruler() {
        assert_eq!(lane_at(1.0), Some(Lane::Chords));
        assert_eq!(lane_at(lane_height(Lane::Chords) + 1.0), Some(Lane::Comp));
        assert_eq!(
            lane_at(lane_height(Lane::Chords) + lane_height(Lane::Comp) + 1.0),
            Some(Lane::Bass)
        );
        assert_eq!(lane_at(-1.0), None);
        assert_eq!(lane_at(1000.0), None);
    }
}
