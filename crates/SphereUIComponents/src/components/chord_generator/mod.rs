//! Chord Generator: an independent utility window for writing a chord
//! progression from a palette, then handing it to the arrangement.
//!
//! - **Palette** — the chords of a key laid out by scale degree (columns)
//!   and kind (rows: triads, sevenths, ninths, suspended and added colours,
//!   the parallel key's chords, secondary dominants). Each chord is coloured
//!   by family and marked by whether all its notes lie in the song's key.
//!   The arrows step the palette round the circle of fifths away from the
//!   song key for more distant colours; Home brings it back; Parallel swaps
//!   major and minor. Click a chord to hear it, double-click to add it, drag
//!   it into the sequence.
//! - **Sequence** — sections of chords played back to back. A chord dropped
//!   inside a section joins it; dropped at a section's edge or past the end
//!   it starts a new section. Chords move by their body (Alt copies), stretch
//!   by their right edge, and Shift-click selects several; a section moves by
//!   its tab (Alt copies). Under the chords, **Comp** and **Bass** lanes hold
//!   performance patterns saying how the chords are played; double-click an
//!   empty stretch to add one.
//! - **Fill Section** writes a generated progression in the chosen style into
//!   the selected section's unlocked chords, or as a new section.
//!
//! Every gesture is a transient preview committed once on release, and every
//! edit is one undo step (window-local: nothing here touches the project
//! until it is exported). Export goes to the Studio as a
//! [`ChordGeneratorCommand`]: the chords to the Chord Track, or the rendered
//! Comp + Bass performance as one MIDI clip — from the footer buttons at the
//! playhead, or by dragging the footer handle onto the arrangement, where the
//! Studio resolves the drop against the main window's live pointer.

mod model;
mod palette_view;
mod sequence;
mod side_panel;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, size, AnyElement, App, AppContext, Bounds, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, Rgba, StatefulInteractiveElement, Styled,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};
use sphere_midi_service::chords::{
    alternatives, generate, pitch_name, Chord, ChordFamily, Colors as ChordColors,
    GeneratorSettings, Richness, Rng, Scale, Style, VoicingOptions,
};
use sphere_midi_service::performance::{render_performance, GeneratedNote};

use self::model::{
    snap, Arrangement, History, Insert, Lane, Pattern, Placed, Region, BEATS_PER_BAR, MIN_LENGTH,
};
use crate::assets;
use crate::components::controls::{
    fb_button, fb_checkbox, fb_icon_button, fb_section_label, fb_segment, fb_segmented_track,
    fb_tooltip, FbButtonKind, FbSegment,
};
use crate::components::timeline::timeline_state::ChordPlacement;
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const CHORD_GENERATOR_WIDTH: f32 = 1120.0;
pub const CHORD_GENERATOR_HEIGHT: f32 = 760.0;
pub(crate) const MIN_WIDTH: f32 = 980.0;
pub(crate) const MIN_HEIGHT: f32 = 640.0;
/// Right column: the pitch wheel and the selection's properties.
const SIDE_W: f32 = 248.0;
const WHEEL_SIZE: f32 = 148.0;
/// Sequence geometry: lane label column, ruler, lane heights, region inset,
/// stretch handle width, section tab height.
const LABEL_W: f32 = 64.0;
const RULER_H: f32 = 20.0;
const CHORD_LANE_H: f32 = 76.0;
const PATTERN_LANE_H: f32 = 30.0;
const REGION_INSET: f32 = 3.0;
const EDGE_W: f32 = 6.0;
const SECTION_TAB_H: f32 = 16.0;
/// How close to a section's edge a dropped chord starts a new section
/// beside it rather than joining it.
const SECTION_EDGE_PX: f32 = 12.0;
/// Beats a chord or pattern snaps to.
const GRID: f64 = 1.0;
/// Pointer travel before a press becomes a drag.
const DRAG_THRESHOLD: f32 = 4.0;
const FRAME: Duration = Duration::from_millis(16);
const SETTLE_SECONDS: f32 = 0.12;
/// How long a clicked chord rings.
const AUDITION_SECONDS: f64 = 1.4;
const LENGTHS: [u32; 4] = [4, 8, 16, 32];

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

#[derive(Debug, Clone, PartialEq)]
enum Selection {
    None,
    /// Chords of the sequence, in the order they were picked.
    Chords(Vec<u64>),
    Section(u64),
    Region(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionMode {
    Move,
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PressKind {
    /// A sequence chord: its body moves it, its right edge stretches it.
    Chord {
        id: u64,
        stretch: bool,
        original_beats: f64,
    },
    /// A section's tab.
    Section(u64),
    /// A Comp or Bass pattern region.
    Region {
        id: u64,
        mode: RegionMode,
        original: Region,
    },
    /// A palette chord.
    Palette(Chord),
    /// The footer handle: the whole sequence to the arrangement.
    Export,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    kind: PressKind,
    origin: Point<Pixels>,
    dragging: bool,
    /// Shift was held at the press (adds to the chord selection).
    shift: bool,
    /// Alt is held: a move becomes a copy.
    copy: bool,
}

/// Where a dragged chord or section would land.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DropMarker {
    at: Insert,
    beat: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopoverKind {
    Tonic,
    Scale,
    Style,
    Colour,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Popover {
    kind: PopoverKind,
    at: Point<Pixels>,
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
    /// Draw the playhead (sequence playback, not an audition).
    timeline: bool,
}

pub struct ChordGeneratorWindow {
    callbacks: ChordGeneratorCallbacks,
    focus: FocusHandle,
    /// The song's key, and the generator's style and colours.
    settings: GeneratorSettings,
    /// Fifths the palette is stepped away from the song key.
    palette_steps: i32,
    /// The palette shows the parallel mode of its key.
    palette_parallel: bool,
    /// The palette chord last clicked: the wheel shows it until a sequence
    /// chord is picked.
    palette_pick: Option<Chord>,
    seed: u64,
    voicing: VoicingOptions,
    arrangement: Arrangement,
    /// The arrangement as a live gesture would leave it.
    preview: Option<Arrangement>,
    history: History,
    selection: Selection,
    press: Option<Press>,
    marker: Option<DropMarker>,
    popover: Option<Popover>,
    playback: Option<Playback>,
    status: Option<String>,
    /// Painted bounds of the sequence's content column (ruler + lanes), for
    /// mapping the pointer to beats.
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
        Self::with_seed(callbacks, time_seed(), cx)
    }

    /// The window as the UI preview renders it: nothing to play through or
    /// export to, and a fixed opening progression.
    #[cfg(feature = "ui-preview")]
    pub(crate) fn for_preview(seed: u64, cx: &mut Context<Self>) -> Self {
        let callbacks = ChordGeneratorCallbacks {
            on_command: Arc::new(|_, _| None),
            audition: Arc::new(|_, _, _| false),
            project_bpm: Arc::new(|_| 120.0),
            on_close: Arc::new(|_, _| {}),
        };
        let mut window = Self::with_seed(callbacks, seed, cx);
        // A second section, so the preview shows how sections read.
        let chords = generate(
            &GeneratorSettings {
                length: 3,
                richness: Richness::Sevenths,
                ..window.settings
            },
            seed ^ 0x2,
        );
        window.arrangement.fill_section(None, &chords);
        window
    }

    /// [`Self::for_preview`] with the palette a fifth away from the song key
    /// and the second section selected.
    #[cfg(feature = "ui-preview")]
    pub(crate) fn for_preview_browsing(seed: u64, cx: &mut Context<Self>) -> Self {
        let mut window = Self::for_preview(seed, cx);
        window.palette_steps = 1;
        if let Some(section) = window.arrangement.sections().get(1) {
            window.selection = Selection::Section(section.id);
        }
        window
    }

    fn with_seed(callbacks: ChordGeneratorCallbacks, seed: u64, cx: &mut Context<Self>) -> Self {
        let mut window = Self {
            callbacks,
            focus: cx.focus_handle(),
            settings: GeneratorSettings::default(),
            palette_steps: 0,
            palette_parallel: false,
            palette_pick: None,
            seed,
            voicing: VoicingOptions {
                bass: false,
                ..VoicingOptions::default()
            },
            arrangement: Arrangement::default(),
            preview: None,
            history: History::default(),
            selection: Selection::None,
            press: None,
            marker: None,
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
        // Open on a generated section with a sustained comp and a root bass
        // under it, so Play sounds straight away.
        let chords = generate(&window.settings, window.seed);
        window.arrangement.fill_section(None, &chords);
        let end = window.arrangement.chords_end();
        for lane in Lane::ALL {
            window
                .arrangement
                .place_pattern(0.0, end, lane.default_pattern());
        }
        if let Some(first) = window.arrangement.placed().first() {
            window.selection = Selection::Chords(vec![first.slot.id]);
        }
        window.ensure_ticking(cx);
        window
    }

    // ── State helpers ───────────────────────────────────────────────────────

    /// Spell in flats in the song's key.
    fn flats(&self) -> bool {
        self.settings.scale.prefers_flats(self.settings.tonic)
    }

    /// Spell `chord` in flats? A chord rooted in the song's scale follows
    /// the key; one rooted a lowered step away (♭II, ♭III, ♭VI, ♭VII, the
    /// borrowed and Neapolitan colours) is spelled flat, so a borrowed E♭ in
    /// C major does not read as D♯.
    fn chord_flats(&self, chord: &Chord) -> bool {
        self.pc_flats(chord.root)
    }

    /// Spell pitch class `pc` in flats? See [`Self::chord_flats`].
    fn pc_flats(&self, pc: u8) -> bool {
        let tonic = self.settings.tonic;
        if self.settings.scale.pitch_classes(tonic).contains(&pc) {
            return self.flats();
        }
        matches!((pc + 12 - tonic) % 12, 1 | 3 | 8 | 10) || self.flats()
    }

    fn chord_name(&self, chord: &Chord) -> String {
        chord.name(self.chord_flats(chord))
    }

    /// The key the palette shows.
    fn palette_key(&self) -> (u8, Scale) {
        let tonic = (self.settings.tonic as i32 + 7 * self.palette_steps).rem_euclid(12) as u8;
        let scale = if self.palette_parallel {
            self.settings.scale.parallel()
        } else {
            self.settings.scale
        };
        (tonic, scale)
    }

    fn palette_is_home(&self) -> bool {
        self.palette_steps == 0 && !self.palette_parallel
    }

    /// Whether every note of `chord` lies in the song's key.
    fn in_song_key(&self, chord: &Chord) -> bool {
        chord.fits(self.settings.scale, self.settings.tonic)
    }

    /// What the sequence shows: the live gesture's result, or the committed
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
        self.drop_stale_selection();
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
        let arrangement = &self.arrangement;
        self.selection = match std::mem::replace(&mut self.selection, Selection::None) {
            Selection::Chords(mut ids) => {
                ids.retain(|id| arrangement.find_slot(*id).is_some());
                if ids.is_empty() {
                    Selection::None
                } else {
                    Selection::Chords(ids)
                }
            }
            Selection::Section(id) if arrangement.section_index(id).is_some() => {
                Selection::Section(id)
            }
            Selection::Region(id) if arrangement.find_region(id).is_some() => Selection::Region(id),
            _ => Selection::None,
        };
    }

    fn selected_chords(&self) -> Vec<u64> {
        match &self.selection {
            Selection::Chords(ids) => ids.clone(),
            _ => Vec::new(),
        }
    }

    /// The one selected chord, with its place.
    fn selected_slot(&self) -> Option<Placed> {
        match &self.selection {
            Selection::Chords(ids) if ids.len() == 1 => self.arrangement.find_slot(ids[0]),
            _ => None,
        }
    }

    /// The section a Fill or an added chord goes to: the selected section,
    /// or the selected chord's.
    fn target_section(&self) -> Option<u64> {
        match &self.selection {
            Selection::Section(id) => Some(*id),
            Selection::Chords(ids) => ids
                .first()
                .and_then(|id| self.arrangement.section_of(*id))
                .map(|index| self.arrangement.sections()[index].id),
            _ => None,
        }
    }

    // ── Key and palette ─────────────────────────────────────────────────────

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
        self.palette_steps = 0;
        self.palette_parallel = false;
        self.palette_pick = None;
        self.edit(cx, |this| this.arrangement.transpose(delta));
        self.status = Some(format!(
            "Song key {} {} — the sequence moved with it",
            pitch_name(tonic, self.flats()),
            self.settings.scale.label()
        ));
    }

    fn set_scale(&mut self, scale: Scale, cx: &mut Context<Self>) {
        self.settings.scale = scale;
        self.palette_steps = 0;
        self.palette_parallel = false;
        cx.notify();
    }

    fn step_palette(&mut self, delta: i32, cx: &mut Context<Self>) {
        // Twelve fifths come back round; keep the count readable.
        self.palette_steps = (self.palette_steps + delta).clamp(-6, 6);
        cx.notify();
    }

    fn palette_home(&mut self, cx: &mut Context<Self>) {
        self.palette_steps = 0;
        self.palette_parallel = false;
        cx.notify();
    }

    fn toggle_parallel(&mut self, cx: &mut Context<Self>) {
        self.palette_parallel = !self.palette_parallel;
        cx.notify();
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut GeneratorSettings),
    ) {
        change(&mut self.settings);
        cx.notify();
    }

    // ── Sequence edits ──────────────────────────────────────────────────────

    /// Where a double-clicked palette chord goes: after the selected chord,
    /// at the end of the selected section, or at the end of the last one.
    fn add_destination(&self) -> Insert {
        let arrangement = &self.arrangement;
        if let Some(placed) = self.selected_slot() {
            let section = &arrangement.sections()[placed.section_index];
            let index = section
                .slots
                .iter()
                .position(|s| s.id == placed.slot.id)
                .map_or(section.slots.len(), |i| i + 1);
            return Insert::Into {
                section: section.id,
                index,
            };
        }
        let section = self
            .target_section()
            .or_else(|| arrangement.sections().last().map(|s| s.id));
        match section.and_then(|id| arrangement.section_index(id)) {
            Some(index) => Insert::Into {
                section: arrangement.sections()[index].id,
                index: arrangement.sections()[index].slots.len(),
            },
            None => Insert::NewSection(arrangement.sections().len()),
        }
    }

    fn insert_palette_chord(&mut self, chord: Chord, at: Insert, cx: &mut Context<Self>) {
        let mut placed = Vec::new();
        self.edit(cx, |this| {
            placed = this
                .arrangement
                .insert_chords(at, &[(chord, BEATS_PER_BAR)]);
            if !placed.is_empty() {
                this.selection = Selection::Chords(placed.clone());
            }
        });
        self.palette_pick = None;
        self.status = Some(if placed.is_empty() {
            "The sequence is full — 32 bars".to_string()
        } else {
            format!("Added {}", self.chord_name(&chord))
        });
    }

    /// Generate a progression into the target section's unlocked chords, or
    /// as a new section.
    fn fill(&mut self, cx: &mut Context<Self>) {
        let target = self.target_section();
        let length = target
            .and_then(|id| self.arrangement.section_index(id))
            .map_or(4, |index| self.arrangement.sections()[index].slots.len());
        let mut rng = Rng::new(self.seed ^ time_seed());
        self.seed = rng.next_u64();
        let settings = GeneratorSettings {
            length,
            ..self.settings
        };
        let chords = generate(&settings, self.seed);
        let mut filled = None;
        self.edit(cx, |this| {
            filled = this.arrangement.fill_section(target, &chords);
            if let Some(id) = filled {
                this.selection = Selection::Section(id);
            }
        });
        let label = filled
            .and_then(|id| self.arrangement.section_index(id))
            .map(section_label);
        self.status = Some(match (target, label) {
            (_, None) => "The sequence is full — 32 bars".to_string(),
            (Some(_), Some(label)) => format!(
                "Section {label}: new {} progression — locked chords kept",
                self.settings.style.label()
            ),
            (None, Some(label)) => format!(
                "Section {label}: new {} progression",
                self.settings.style.label()
            ),
        });
    }

    fn swap_chord(&mut self, id: u64, chord: Chord, cx: &mut Context<Self>) {
        self.edit(cx, |this| {
            this.arrangement.set_chord(id, chord, true);
        });
        self.audition_slot(id, cx);
    }

    fn reroll(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(current) = self.arrangement.find_slot(id).map(|p| p.slot.chord) else {
            return;
        };
        let choices: Vec<Chord> = alternatives(&self.settings, self.arrangement.next_chord(id))
            .into_iter()
            .filter(|c| *c != current)
            .collect();
        if choices.is_empty() {
            return;
        }
        let mut rng = Rng::new(time_seed() ^ id);
        let chord = choices[rng.below(choices.len())];
        self.edit(cx, |this| {
            this.arrangement.set_chord(id, chord, false);
        });
        self.audition_slot(id, cx);
    }

    fn toggle_lock(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit(cx, |this| {
            if let Some(placed) = this.arrangement.find_slot(id) {
                this.arrangement.set_locked(id, !placed.slot.locked);
            }
        });
    }

    fn duplicate_section(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.arrangement.section_index(id) else {
            return;
        };
        let mut copy = None;
        self.edit(cx, |this| {
            copy = this.arrangement.move_section(id, index + 1, true);
            if let Some(copy) = copy {
                this.selection = Selection::Section(copy);
            }
        });
        if copy.is_none() {
            self.status = Some("No room for a copy — the sequence holds 32 bars".to_string());
        }
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let selection = self.selection.clone();
        self.edit(cx, |this| {
            match selection {
                Selection::Chords(ids) => {
                    this.arrangement.remove_chords(&ids);
                }
                Selection::Section(id) => {
                    this.arrangement.remove_section(id);
                }
                Selection::Region(id) => {
                    this.arrangement.remove_region(id);
                }
                Selection::None => {}
            }
            this.selection = Selection::None;
        });
    }

    fn set_bars(&mut self, bars: u32, cx: &mut Context<Self>) {
        self.edit(cx, |this| this.arrangement.set_bars(bars));
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.stop_playback(cx);
        self.edit(cx, |this| {
            this.arrangement.clear();
            this.selection = Selection::None;
        });
        self.status = Some("Sequence cleared — Undo brings it back".to_string());
    }

    /// Fill the empty stretch of `lane` around `beat` with its default
    /// pattern.
    fn add_pattern_at(&mut self, lane: Lane, beat: f64, cx: &mut Context<Self>) {
        let Some((start, length)) = self.arrangement.gap_at(lane, beat) else {
            return;
        };
        self.edit(cx, |this| {
            if let Some(id) = this
                .arrangement
                .place_pattern(start, length, lane.default_pattern())
            {
                this.selection = Selection::Region(id);
            }
        });
    }

    fn set_region_pattern(&mut self, id: u64, pattern: Pattern, cx: &mut Context<Self>) {
        self.edit(cx, |this| {
            this.arrangement.set_pattern(id, pattern);
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

    /// Chords for the Chord Track, back to back from the start.
    fn placements(&self) -> (f64, Vec<ChordPlacement>) {
        let placements = self
            .arrangement
            .placed()
            .into_iter()
            .map(|p| ChordPlacement {
                chord: p.slot.chord,
                flats: self.chord_flats(&p.slot.chord),
                length_beats: p.slot.beats,
            })
            .collect();
        (0.0, placements)
    }

    /// The whole sequence as one clip's notes and length.
    fn clip(&self) -> (Vec<GeneratedNote>, f64) {
        let notes = self.performance_notes(&self.arrangement);
        let end = notes
            .iter()
            .map(|n| n.start + n.length)
            .fold(self.arrangement.chords_end(), f64::max);
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

    fn held_chord(&self, voiced: &[u8], cx: &App) -> (Vec<GeneratedNote>, f64) {
        let length = AUDITION_SECONDS * self.bpm(cx) / 60.0;
        let notes = voiced
            .iter()
            .map(|&pitch| GeneratedNote {
                pitch,
                start: 0.0,
                length,
                velocity: 88,
            })
            .collect();
        (notes, length)
    }

    /// Sound one sequence chord, voiced as it sits in the progression, with
    /// its bass note.
    fn audition_slot(&mut self, id: u64, cx: &mut Context<Self>) {
        let placed = self.arrangement.placed();
        let Some(index) = placed.iter().position(|p| p.slot.id == id) else {
            return;
        };
        let chords: Vec<Chord> = placed.iter().map(|p| p.slot.chord).collect();
        let voiced = sphere_midi_service::chords::voice_progression(
            &chords,
            VoicingOptions {
                bass: true,
                ..self.voicing
            },
        );
        let (notes, length) = self.held_chord(&voiced[index], cx);
        self.start_playback(notes, length, false, cx);
    }

    /// Sound a palette chord on its own, with its bass note.
    fn audition_chord(&mut self, chord: Chord, cx: &mut Context<Self>) {
        let voiced = sphere_midi_service::chords::voice_progression(
            &[chord],
            VoicingOptions {
                bass: true,
                ..self.voicing
            },
        );
        let (notes, length) = self.held_chord(&voiced[0], cx);
        self.start_playback(notes, length, false, cx);
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

    /// Pixels per beat of the sequence, from its painted width (or, before
    /// the first paint, from the window's fixed layout).
    fn pixels_per_beat(&self, window: &Window) -> f32 {
        sequence_content_width(window) / self.shown().total_beats().max(1.0) as f32
    }

    fn begin_press(
        &mut self,
        kind: PressKind,
        origin: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        self.popover = None;
        self.press = Some(Press {
            kind,
            origin,
            dragging: false,
            shift: modifiers.shift,
            copy: modifiers.alt,
        });
        cx.notify();
    }

    fn cancel_gesture(&mut self) {
        self.press = None;
        self.preview = None;
        self.marker = None;
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
        if !press.dragging && dx.hypot(dy) < DRAG_THRESHOLD {
            return;
        }
        press.dragging = true;
        press.copy = event.modifiers.alt;
        self.press = Some(press);
        let ppb = self.pixels_per_beat(window).max(1.0) as f64;
        match press.kind {
            PressKind::Chord {
                id,
                stretch: true,
                original_beats,
            } => {
                let beats = snap(original_beats + dx as f64 / ppb, GRID).max(MIN_LENGTH);
                let mut preview = self.arrangement.clone();
                preview.resize_chord(id, beats);
                self.preview = Some(preview);
                self.status = Some(format!("Length {}", beats_label(beats)));
            }
            PressKind::Chord { .. } | PressKind::Palette(_) => {
                self.marker = self.marker_at(event.position, false);
                self.status = Some(match (self.marker, press.kind) {
                    (None, _) => "Drop on the sequence".to_string(),
                    (Some(m), PressKind::Palette(_)) => drop_hint(m.at, "Release to add"),
                    (Some(m), _) if press.copy => drop_hint(m.at, "Release to copy"),
                    (Some(m), _) => drop_hint(m.at, "Release to move"),
                });
            }
            PressKind::Section(_) => {
                self.marker = self.marker_at(event.position, true);
                self.status = Some(if press.copy {
                    "Release to copy the section here".to_string()
                } else {
                    "Release to move the section here".to_string()
                });
            }
            PressKind::Region { id, mode, original } => {
                let delta = dx as f64 / ppb;
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

    /// Where a chord (or, with `sections_only`, a section) dropped at
    /// `position` would go, if over the chord lane.
    fn marker_at(&self, position: Point<Pixels>, sections_only: bool) -> Option<DropMarker> {
        let bounds = self.lanes_bounds.get()?;
        let x: f32 = (position.x - bounds.origin.x).into();
        let y: f32 = (position.y - bounds.origin.y).into();
        let width: f32 = bounds.size.width.into();
        if x < -SECTION_EDGE_PX || x > width + SECTION_EDGE_PX {
            return None;
        }
        if y < 0.0 || y > RULER_H + CHORD_LANE_H + space::BASE {
            return None;
        }
        let arrangement = &self.arrangement;
        let ppb = width / arrangement.total_beats().max(1.0) as f32;
        let at = drop_target(arrangement, x.max(0.0), ppb, sections_only);
        Some(DropMarker {
            at,
            beat: arrangement.insert_beat(at),
        })
    }

    /// End a press. `released_at` is `(point, over this window, modifiers)`
    /// for a mouse-up; `None` when the gesture was abandoned.
    fn finish_press(
        &mut self,
        released_at: Option<(Point<Pixels>, bool, Modifiers)>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(press) = self.press.take() else {
            return;
        };
        let released = released_at.is_some();
        let over_self = released_at.is_some_and(|(_, over, _)| over);
        let copy = released_at.map_or(press.copy, |(_, _, m)| m.alt);
        let marker = self.marker.take();
        let preview = self.preview.take();
        if press.dragging {
            self.status = None;
        }
        match press.kind {
            PressKind::Chord { id, stretch, .. } => {
                if press.dragging {
                    match (stretch, released && over_self) {
                        (true, true) => {
                            if let Some(preview) = preview {
                                self.edit(cx, |this| this.arrangement = preview);
                            }
                        }
                        (false, true) => {
                            if let Some(marker) = marker {
                                let mut ids = self.selected_chords();
                                if !ids.contains(&id) {
                                    ids = vec![id];
                                }
                                self.edit(cx, |this| {
                                    let landed =
                                        this.arrangement.move_chords(&ids, marker.at, copy);
                                    if !landed.is_empty() {
                                        this.selection = Selection::Chords(landed);
                                    }
                                });
                            }
                        }
                        _ => {}
                    }
                } else if released {
                    self.palette_pick = None;
                    self.selection = match (press.shift, &self.selection) {
                        (true, Selection::Chords(ids)) => {
                            let mut ids = ids.clone();
                            if let Some(i) = ids.iter().position(|x| *x == id) {
                                ids.remove(i);
                            } else {
                                ids.push(id);
                            }
                            if ids.is_empty() {
                                Selection::None
                            } else {
                                Selection::Chords(ids)
                            }
                        }
                        _ => Selection::Chords(vec![id]),
                    };
                    self.audition_slot(id, cx);
                }
            }
            PressKind::Section(id) => {
                if press.dragging {
                    if let (
                        true,
                        Some(DropMarker {
                            at: Insert::NewSection(to),
                            ..
                        }),
                    ) = (released && over_self, marker)
                    {
                        let mut landed = None;
                        self.edit(cx, |this| {
                            landed = this.arrangement.move_section(id, to, copy);
                            if let Some(landed) = landed {
                                this.selection = Selection::Section(landed);
                            }
                        });
                        if landed.is_none() {
                            self.status =
                                Some("No room for a copy — the sequence holds 32 bars".to_string());
                        }
                    }
                } else if released {
                    self.selection = Selection::Section(id);
                }
            }
            PressKind::Region { id, .. } => {
                if press.dragging {
                    if let (true, Some(preview)) = (released, preview) {
                        self.edit(cx, |this| this.arrangement = preview);
                        self.selection = Selection::Region(id);
                    }
                } else if released {
                    self.selection = Selection::Region(id);
                }
            }
            PressKind::Palette(chord) => {
                if press.dragging {
                    if let (true, Some(marker)) = (released && over_self, marker) {
                        self.insert_palette_chord(chord, marker.at, cx);
                    }
                } else if released {
                    self.palette_pick = Some(chord);
                    self.audition_chord(chord, cx);
                }
            }
            PressKind::Export => {
                if press.dragging {
                    if over_self || !released {
                        self.run(ChordGeneratorCommand::DragCancel, cx);
                    } else {
                        let (offset_beats, chords) = self.placements();
                        let (notes, length_beats) = self.clip();
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
        self.ensure_ticking(cx);
        cx.notify();
    }

    // ── Wheel animation ─────────────────────────────────────────────────────

    /// The chord the wheel shows: under the playhead while the sequence
    /// plays, else the palette chord just clicked, else the selected one.
    fn focus_chord(&self) -> Option<Chord> {
        if let Some(playback) = self.playback.as_ref().filter(|p| p.timeline) {
            return self.arrangement.slot_at(playback.beat).map(|s| s.chord);
        }
        self.palette_pick
            .or_else(|| self.selected_slot().map(|p| p.slot.chord))
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

    // ── Render: toolbar ─────────────────────────────────────────────────────

    fn popover_trigger(
        &self,
        id: &'static str,
        kind: PopoverKind,
        label: String,
        width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.popover.is_some_and(|p| p.kind == kind);
        div()
            .flex_none()
            .w(px(width))
            .child(crate::components::combo_box::combo_box_trigger(
                id,
                label,
                open,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.popover = match this.popover {
                        Some(p) if p.kind == kind => None,
                        _ => Some(Popover {
                            kind,
                            at: event.position,
                        }),
                    };
                    cx.notify();
                }),
            ))
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let flats = self.flats();
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
        let colours_on = colour_options()
            .iter()
            .filter(|(_, _, get, _)| get(&self.settings.colors))
            .count();
        let colour_label = match colours_on {
            0 => "Colour".to_string(),
            n => format!("Colour · {n}"),
        };
        let playing = self.playback.as_ref().is_some_and(|p| p.timeline);
        let has_chords = self.arrangement.chord_count() > 0;
        let fill_label = if self.target_section().is_some() {
            "Fill Section"
        } else {
            "New Section"
        };
        let module = |label: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(fb_section_label(label))
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .px(px(space::SECTION))
            .py(px(space::BASE))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(
                module("KEY")
                    .child(self.popover_trigger(
                        "chordgen-tonic",
                        PopoverKind::Tonic,
                        pitch_name(self.settings.tonic, flats).to_string(),
                        60.0,
                        cx,
                    ))
                    .child(self.popover_trigger(
                        "chordgen-scale",
                        PopoverKind::Scale,
                        self.settings.scale.label().to_string(),
                        136.0,
                        cx,
                    )),
            )
            .child(
                module("GENERATE")
                    .child(self.popover_trigger(
                        "chordgen-style",
                        PopoverKind::Style,
                        self.settings.style.label().to_string(),
                        98.0,
                        cx,
                    ))
                    .child(div().flex_none().w(px(184.0)).child(richness))
                    .child(self.popover_trigger(
                        "chordgen-colour",
                        PopoverKind::Colour,
                        colour_label,
                        98.0,
                        cx,
                    ))
                    .child(
                        div()
                            .id("chordgen-fill-tip")
                            .tooltip(fb_tooltip(
                                "Write a progression in this style into the selected section's \
                                 unlocked chords, or as a new section (G)",
                            ))
                            .child(fb_button(
                                "chordgen-fill",
                                fill_label,
                                FbButtonKind::Primary,
                                true,
                                cx.listener(|this, _, _, cx| this.fill(cx)),
                            )),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::TIGHT))
                    .child(fb_button(
                        "chordgen-play",
                        if playing { "Stop" } else { "Play" },
                        FbButtonKind::Default,
                        has_chords,
                        cx.listener(|this, _, _, cx| this.toggle_playback(cx)),
                    ))
                    .child(
                        div()
                            .id("chordgen-undo-tip")
                            .tooltip(fb_tooltip("Undo (Ctrl+Z)"))
                            .child(fb_icon_button(
                                "chordgen-undo",
                                assets::ICON_UNDO_PATH,
                                "Undo",
                                crate::theme::size::DEFAULT,
                                None,
                                cx.listener(|this, _, _, cx| this.undo(cx)),
                            )),
                    )
                    .child(
                        div()
                            .id("chordgen-redo-tip")
                            .tooltip(fb_tooltip("Redo (Ctrl+Shift+Z)"))
                            .child(fb_icon_button(
                                "chordgen-redo",
                                assets::ICON_REDO_PATH,
                                "Redo",
                                crate::theme::size::DEFAULT,
                                None,
                                cx.listener(|this, _, _, cx| this.redo(cx)),
                            )),
                    ),
            )
    }

    // ── Render: footer and popover ──────────────────────────────────────────

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone().unwrap_or_else(|| {
            "Plays on the selected instrument track · export at the playhead or drag onto the arrangement"
                .to_string()
        });
        let has_chords = self.arrangement.chord_count() > 0;
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
                                this.begin_press(
                                    PressKind::Export,
                                    event.position,
                                    event.modifiers,
                                    cx,
                                );
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
    }

    fn popover(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Popover { kind, at } = self.popover?;
        let (w, h) = match kind {
            PopoverKind::Tonic => (204.0_f32, 132.0_f32),
            PopoverKind::Scale => (220.0, 360.0),
            PopoverKind::Style => (180.0, 230.0),
            PopoverKind::Colour => (230.0, 196.0),
        };
        let viewport = window.viewport_size();
        let vw: f32 = viewport.width.into();
        let vh: f32 = viewport.height.into();
        let x: f32 = at.x.into();
        let y: f32 = at.y.into();
        let left = x.min(vw - w - space::BASE).max(space::BASE);
        let top = (y + space::BASE).min(vh - h - space::BASE).max(space::BASE);
        let body: Vec<AnyElement> = match kind {
            PopoverKind::Tonic => {
                let flats = self.flats();
                let tonic = self.settings.tonic;
                vec![div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(space::TIGHT))
                    .children((0..12u8).map(|pc| {
                        let rest = if pc == tonic {
                            Colors::composite(Colors::surface_card(), Colors::state_selected())
                        } else {
                            Colors::surface_raised()
                        };
                        let hover = Colors::composite(rest, Colors::state_hover());
                        div()
                            .id(("chordgen-tonic-item", pc as usize))
                            .w(px(44.0))
                            .h(px(crate::theme::size::DEFAULT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(radius::CONTROL))
                            .bg(rest)
                            .border(px(1.0))
                            .border_color(if pc == tonic {
                                Colors::with_alpha(Colors::accent_primary(), 0.85)
                            } else {
                                Colors::border_subtle()
                            })
                            .hover(move |s| s.bg(hover))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .text_size(px(typography::UI_SM))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Colors::text_primary())
                            .child(pitch_name(pc, flats))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.popover = None;
                                this.set_tonic(pc, cx);
                            }))
                    }))
                    .into_any_element()]
            }
            PopoverKind::Scale => {
                let current = self.settings.scale;
                Scale::ALL
                    .iter()
                    .enumerate()
                    .map(|(index, scale)| {
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
                            this.set_scale(scale, cx);
                        }))
                        .into_any_element()
                    })
                    .collect()
            }
            PopoverKind::Style => {
                let current = self.settings.style;
                Style::ALL
                    .iter()
                    .enumerate()
                    .map(|(index, style)| {
                        let style = *style;
                        choice_row(
                            ("chordgen-style-item", index),
                            style.label().to_string(),
                            String::new(),
                            style == current,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.popover = None;
                            this.update_settings(cx, |s| s.style = style);
                        }))
                        .into_any_element()
                    })
                    .collect()
            }
            PopoverKind::Colour => {
                let colors = self.settings.colors;
                colour_options()
                    .iter()
                    .map(|&(id, label, get, set)| {
                        let on = get(&colors);
                        div()
                            .child(fb_checkbox(
                                id,
                                label,
                                on,
                                true,
                                cx.listener(move |this, _, _, cx| {
                                    this.update_settings(cx, |s| set(&mut s.colors, !on));
                                }),
                            ))
                            .into_any_element()
                    })
                    .collect()
            }
        };
        Some(
            div()
                .id("chordgen-popover")
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
                .gap(px(if kind == PopoverKind::Colour {
                    space::TIGHT
                } else {
                    0.0
                }))
                .overflow_y_scroll()
                // Clicks inside the menu must not reach the root's
                // click-away handler before the item's own click fires.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(body)
                .into_any_element(),
        )
    }
}

impl Render for ChordGeneratorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dragging = self.press.is_some_and(|p| p.dragging);
        // While a press is live, listen window-wide: drags cross element
        // bounds, and the export drag leaves this window on its way to the
        // arrangement.
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
                            this.finish_press(Some((event.position, over, event.modifiers)), w, cx)
                        });
                    });
                },
            )
            .absolute()
            .inset_0()
        });
        let popover = self.popover(window, cx);
        let side = self.side_panel(window, cx);
        let sequence = self.sequence(window, cx);
        let palette = self.palette(cx);

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
                    "backspace" | "delete" if this.press.is_none() => this.delete_selection(cx),
                    "space" => this.toggle_playback(cx),
                    "g" if !modifiers.modified() => this.fill(cx),
                    "left" if !modifiers.modified() => this.step_palette(-1, cx),
                    "right" if !modifiers.modified() => this.step_palette(1, cx),
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
                    this.finish_press(Some((event.position, true, event.modifiers)), window, cx);
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
            .child(self.toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(space::SECTION))
                    .p(px(space::SECTION))
                    .child(sequence)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .flex()
                            .flex_row()
                            .gap(px(space::SECTION))
                            .child(palette)
                            .child(side),
                    ),
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
    crate::dialog_frame::open_dialog_window(cx, options, move |window, cx| {
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

/// Width of the sequence's content column: the window less the body
/// padding, the lane-label column and the panel's two border pixels.
fn sequence_content_width(window: &Window) -> f32 {
    let viewport: f32 = window.viewport_size().width.into();
    (viewport - 2.0 * space::SECTION - LABEL_W - 2.0).max(120.0)
}

/// Where a drop at `x` pixels into the sequence goes. Inside a section it
/// joins at the nearest chord boundary, within [`SECTION_EDGE_PX`] of a
/// section's edge (or past the end) it starts a new section there. With
/// `sections_only`, only section boundaries count.
fn drop_target(arrangement: &Arrangement, x: f32, ppb: f32, sections_only: bool) -> Insert {
    let mut start = 0.0_f64;
    for (index, section) in arrangement.sections().iter().enumerate() {
        let end = start + section.beats();
        let (sx, ex) = (start as f32 * ppb, end as f32 * ppb);
        if x <= ex {
            if sections_only {
                return Insert::NewSection(if x - sx < ex - x { index } else { index + 1 });
            }
            if x - sx < SECTION_EDGE_PX {
                return Insert::NewSection(index);
            }
            if ex - x < SECTION_EDGE_PX {
                return Insert::NewSection(index + 1);
            }
            let mut at = start;
            let mut slot_index = section.slots.len();
            for (k, slot) in section.slots.iter().enumerate() {
                if x < (at + slot.beats * 0.5) as f32 * ppb {
                    slot_index = k;
                    break;
                }
                at += slot.beats;
            }
            return Insert::Into {
                section: section.id,
                index: slot_index,
            };
        }
        start = end;
    }
    Insert::NewSection(arrangement.sections().len())
}

fn drop_hint(at: Insert, verb: &str) -> String {
    match at {
        Insert::NewSection(_) => format!("{verb} as a new section"),
        Insert::Into { .. } => format!("{verb} into this section"),
    }
}

/// `A`, `B`, … `Z`, `AA`, … for section index `index`.
fn section_label(index: usize) -> String {
    let mut n = index;
    let mut label = String::new();
    loop {
        label.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    label
}

/// The colour a chord's family is drawn in.
fn family_color(chord: &Chord) -> Rgba {
    match chord.quality.family() {
        ChordFamily::Major => Colors::harmony_major(),
        ChordFamily::Minor => Colors::harmony_minor(),
        ChordFamily::Other => Colors::harmony_other(),
    }
}

/// The in-key mark: a filled dot when every note is in the song's key, a
/// ring when some are not.
fn key_dot(in_key: bool, color: Rgba) -> impl IntoElement {
    let d = 7.0;
    div()
        .flex_none()
        .size(px(d))
        .rounded(px(radius::PILL))
        .border(px(1.5))
        .border_color(color)
        .when(in_key, |dot| dot.bg(color))
}

type ColourOption = (
    &'static str,
    &'static str,
    fn(&ChordColors) -> bool,
    fn(&mut ChordColors, bool),
);

fn colour_options() -> [ColourOption; 5] {
    [
        ("chordgen-sus", "Sus", |c| c.sus, |c, v| c.sus = v),
        (
            "chordgen-borrowed",
            "Borrowed",
            |c| c.borrowed,
            |c, v| c.borrowed = v,
        ),
        (
            "chordgen-secondary",
            "Secondary dominants",
            |c| c.secondary_dominants,
            |c, v| c.secondary_dominants = v,
        ),
        (
            "chordgen-passing",
            "Passing diminished",
            |c| c.passing_diminished,
            |c, v| c.passing_diminished = v,
        ),
        (
            "chordgen-blackadder",
            "Black Adder",
            |c| c.black_adder,
            |c, v| c.black_adder = v,
        ),
    ]
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

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_midi_service::chords::ChordQuality;

    fn two_sections() -> Arrangement {
        let mut a = Arrangement::default();
        let bar = |root| (Chord::new(root, ChordQuality::Major), BEATS_PER_BAR);
        a.insert_chords(Insert::NewSection(0), &[bar(0), bar(9)]);
        a.insert_chords(Insert::NewSection(1), &[bar(5), bar(7)]);
        a
    }

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
    fn sections_are_lettered() {
        assert_eq!(section_label(0), "A");
        assert_eq!(section_label(25), "Z");
        assert_eq!(section_label(26), "AA");
        assert_eq!(section_label(27), "AB");
    }

    #[test]
    fn a_drop_joins_a_section_or_starts_one_at_its_edges() {
        let a = two_sections();
        // Two sections of two bars at 10 px a beat: 0–80 and 80–160.
        let ppb = 10.0;
        let first = a.sections()[0].id;
        let second = a.sections()[1].id;
        assert_eq!(drop_target(&a, 3.0, ppb, false), Insert::NewSection(0));
        assert_eq!(
            drop_target(&a, 30.0, ppb, false),
            Insert::Into {
                section: first,
                index: 1
            }
        );
        assert_eq!(
            drop_target(&a, 60.0, ppb, false),
            Insert::Into {
                section: first,
                index: 2
            }
        );
        assert_eq!(drop_target(&a, 75.0, ppb, false), Insert::NewSection(1));
        assert_eq!(drop_target(&a, 85.0, ppb, false), Insert::NewSection(1));
        assert_eq!(
            drop_target(&a, 95.0, ppb, false),
            Insert::Into {
                section: second,
                index: 0
            }
        );
        assert_eq!(drop_target(&a, 400.0, ppb, false), Insert::NewSection(2));
        // Sections move to the nearest section boundary.
        assert_eq!(drop_target(&a, 30.0, ppb, true), Insert::NewSection(0));
        assert_eq!(drop_target(&a, 50.0, ppb, true), Insert::NewSection(1));
        assert_eq!(drop_target(&a, 130.0, ppb, true), Insert::NewSection(2));
    }
}
