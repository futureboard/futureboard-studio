//! The built-in Drum Sampler's panel, drawn natively on the plate.
//!
//! The standard 1024 × 768 plate (see [`crate::components::plate`]), scaled
//! to the window, laid out the way a pad sampler is played and read:
//!
//! * the **well** along the top — the header (the name, how many pads hold
//!   a sample, the Pads / Mixer pages and banks A–D, the kit's master gain
//!   and tune) over the **selected pad's sample**: its waveform with the
//!   region it plays (drag either edge, double-click one to reset it) and the
//!   envelope laid over it in time;
//! * on the deck's left, the **pads** — sixteen in a 4 × 4 grid, pad 1
//!   bottom-left as on a hardware controller. Each shows its number, note,
//!   tags (choke group, filter, output, mute, solo), a silhouette of its
//!   sample with the played region bright, the file name and a level bar that
//!   lights the pad while it sounds — from whatever played it — and flashes on
//!   every hit. Press a pad to select and hear it, as fast as you like; drag
//!   up or down to set its gain (Shift: its tune). A file dropped on a pad
//!   loads there; several fill the pads after it;
//! * under them, the **Samples folder** — search it, click a file to load it
//!   onto the selected pad, Browse… to bring one in from anywhere; a file
//!   dropped here is only added to the folder;
//! * on the deck's right, the selected pad's **Voice**, **Envelope**,
//!   **Filter**, **Trigger** (note and choke group) and **Output** cards —
//!   or, on the Mixer page, a strip per pad of the bank
//!   ([`crate::components::drum_sampler_deck`]).
//!
//! This file only renders; the state lives in
//! [`crate::components::drum_sampler_window::DrumSamplerEditorWindow`].

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use drumsampler::{FilterMode, Pad, BANK_PADS, MIN_REGION, OUTPUTS, PADS};
use gpui::prelude::FluentBuilder;
use gpui::{
    canvas, div, px, relative, Animation, AnimationExt, AnyElement, App, Bounds, ExternalPaths,
    FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Rgba, SharedString,
    StatefulInteractiveElement, Styled,
};

use crate::components::builtin_plugin_editor::DrumPadWaveform;
use crate::components::builtin_plugin_files::BuiltinFileEntry;
use crate::components::drum_sampler_deck::{mixer_deck, pad_cards};
use crate::components::drum_sampler_menu::DrumMenuTarget;
use crate::components::instrument_plate::{self, display_colors, WELL_TOP};
use crate::components::plate::{
    self, caption, color, color_a, numbers, paint_polygon, paint_rect, plate_frame, type_px, words,
    Dial, DialUnit, KnobSet, CARD, CARD_EDGE, FAINT, GHOST, INK, PALE, PLATE_H, PLATE_W, SOFT,
    WELL, WHITE,
};
use crate::components::quick_sampler_panel::{
    format_seconds, paint_display_frame, paint_waveform, ruler_labels, FLAG_H, FLAG_W,
};
use crate::components::soundfont_player_mdi::note_label;
use crate::components::time_plate::{fmt_time, BOX_EDGE, BOX_GROUND};

/// Pad 1 bottom-left, as on a hardware pad controller: the grid's rows, top
/// to bottom.
const BANK_GRID: [usize; BANK_PADS] = [12, 13, 14, 15, 8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3];
/// Banks of [`BANK_PADS`] pads, named A–D.
pub const BANKS: usize = PADS / BANK_PADS;
/// Drum Sampler's accent on the plate.
pub const ACCENT: u32 = 0xff9f43;
/// A failed load, a muted pad; a soloed pad.
pub const ERROR: u32 = 0xff6b6b;
pub const SOLO: u32 = 0xf5c542;

// ── Plate layout, in design units ──────────────────────────────────────────

/// The well: the header over the selected pad's sample.
pub const WELL_H: f32 = 206.0;
/// The deck under it, to the plate's foot (the pads are the playing surface,
/// so the plate has no keyboard strip).
pub const DECK_TOP: f32 = WELL_TOP + WELL_H + 4.0;
pub const DECK_H: f32 = PLATE_H - DECK_TOP - 7.0;
/// The deck's inset, and the gap between its blocks.
pub const DECK_PAD: f32 = 14.0;
pub const DECK_GAP: f32 = 12.0;
/// The pads' column: the grid and the Samples folder under it.
pub const PADS_W: f32 = 380.0;
const PAD_GAP: f32 = 6.0;
const PAD_H: f32 = 74.0;
const MINI_WAVE_H: f32 = 20.0;
/// The display's caption row over the waveform.
const DISPLAY_HEAD_H: f32 = 28.0;

/// The mixer's faders: how tall (design units), and the gain they span.
pub const FADER_H: f32 = 240.0;
pub const FADER_MIN_DB: f32 = -60.0;
pub const FADER_MAX_DB: f32 = 12.0;
/// How near a press must land to a region edge to grab it, in pixels.
pub const EDGE_GRAB_PX: f32 = 10.0;
/// A pad drag's travel: dB and semitones per design unit.
const GAIN_PER_PX: f32 = 0.25;
const TUNE_PER_PX: f32 = 0.1;
/// Pixels a press must move before it is a drag rather than a hit.
pub const DRAG_SLOP_PX: f32 = 4.0;
pub(crate) const ATTACK_MAX_MS: f32 = 250.0;
/// `drumsampler`'s envelope floor: the decay reaches −60 dB at its time.
const DECAY_FLOOR: f32 = 0.001;
const MIN_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
const MAX_Q: f32 = 12.0;
/// How long a pad's flash lasts after a hit.
const HIT_FLASH: Duration = Duration::from_millis(260);
/// The drag-over group: the hint along the foot shows while a file from
/// outside is over the window.
const DROP_GROUP: &str = "drum-sampler-drop";

pub(crate) fn accent() -> Rgba {
    color(ACCENT)
}

/// What the body shows: the pads with the selected one's editor, or a
/// mixer strip per pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DrumView {
    #[default]
    Pads,
    Mixer,
}

/// One edge of a pad's region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionEdge {
    Start,
    End,
}

/// What a vertical drag on a pad sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadAdjust {
    Gain,
    Tune,
}

/// What the panel draws: the insert's kit, what is known of each pad's
/// sample, and the window's transient state.
#[derive(Clone)]
pub struct DrumSamplerPanelState {
    /// The insert's params — what its DSP in the plug-in host plays.
    pub params: drumsampler::Params,
    pub selected: usize,
    /// Each pad's loaded sample, as the host described it.
    pub waveforms: Vec<Option<DrumPadWaveform>>,
    pub loading: [bool; PADS],
    pub errors: Vec<Option<String>>,
    /// Each pad's held output peak, linear, from the plug-in's telemetry.
    pub levels: [f32; PADS],
    /// The Samples folder, or `None` while it is read.
    pub files: Option<Vec<BuiltinFileEntry>>,
    pub region_drag: Option<RegionEdge>,
    /// A pad being dragged to set its gain or tune.
    pub adjusting: Option<(usize, PadAdjust)>,
    /// A short-lived note: files skipped on a drop, and the like.
    pub note: Option<String>,
    /// The plug-in host is running, so samples can be sent to it.
    pub connected: bool,
    /// Counts each pad's hits — a press here, or a new attack in its level —
    /// so every hit starts its flash over, however fast they come.
    pub hits: [u64; PADS],
    /// The pad held down under the pointer.
    pub pressed: Option<usize>,
    /// The bank of sixteen pads on screen (0 = A).
    pub bank: usize,
    pub view: DrumView,
    /// The plate's scale at the last render: window pixels per design unit,
    /// which the window's drags measure in.
    pub scale: f32,
}

impl Default for DrumSamplerPanelState {
    fn default() -> Self {
        Self {
            params: drumsampler::default_params(),
            selected: 0,
            waveforms: vec![None; PADS],
            loading: [false; PADS],
            errors: vec![None; PADS],
            levels: [0.0; PADS],
            files: None,
            region_drag: None,
            adjusting: None,
            note: None,
            connected: false,
            hits: [0; PADS],
            pressed: None,
            bank: 0,
            view: DrumView::Pads,
            scale: 1.0,
        }
    }
}

impl DrumSamplerPanelState {
    pub fn pad(&self) -> &Pad {
        &self.params.pads[self.selected]
    }
}

/// A press on a pad.
#[derive(Clone, Copy, Debug)]
pub struct PadPress {
    pub pad: usize,
    /// Window-space y, where a drag measures from.
    pub y: f32,
    pub shift: bool,
}

/// A press on a mixer fader.
#[derive(Clone, Copy, Debug)]
pub struct FaderPress {
    pub pad: usize,
    /// Window-space y, where the drag measures from.
    pub y: f32,
    pub clicks: usize,
}

/// A handler the panel calls with what was done.
pub type Cb<T> = Rc<dyn Fn(T, &mut App)>;

#[derive(Clone)]
pub struct DrumSamplerCallbacks {
    pub on_pad_press: Cb<PadPress>,
    /// A whole new value for one pad.
    pub on_set_pad: Cb<(usize, Pad)>,
    /// Master gain (dB) and tune (semitones).
    pub on_set_master: Cb<(f32, f32)>,
    /// Dropped files: onto a pad (and the ones after it), or `None` for the
    /// Samples folder only.
    pub on_drop: Cb<(Option<usize>, Vec<PathBuf>)>,
    /// A press on the waveform: window-space x, and the click count.
    pub on_waveform_press: Cb<(f32, usize)>,
    pub on_load_file: Cb<String>,
    pub on_browse: Cb<()>,
    pub on_refresh: Cb<()>,
    /// Every pad's output at once (index = pad).
    pub on_set_outputs: Cb<[u8; PADS]>,
    pub on_view: Cb<DrumView>,
    /// A bank, or a pad, by index.
    pub on_bank: Cb<usize>,
    pub on_select: Cb<usize>,
    pub on_fader_press: Cb<FaderPress>,
    /// A right-click: on what, and where (window space).
    pub on_context: Cb<(DrumMenuTarget, f32, f32)>,
}

impl DrumSamplerCallbacks {
    /// A knob's setter for the selected pad: `apply` writes the knob's value
    /// into a copy of the pad as it is now.
    pub(crate) fn pad_knob(
        &self,
        panel: &DrumSamplerPanelState,
        index: usize,
        apply: impl Fn(&mut Pad, f32) + 'static,
    ) -> KnobSet {
        let pad = panel.params.pads[index].clone();
        let set = self.on_set_pad.clone();
        Rc::new(move |value, cx| {
            let mut next = pad.clone();
            apply(&mut next, value);
            set((index, next), cx)
        })
    }

    /// A click that sends pad `index` as `next`.
    pub(crate) fn send_pad(&self, index: usize, next: Pad) -> instrument_plate::Click {
        let set = self.on_set_pad.clone();
        Rc::new(move |_window, cx| set((index, next.clone()), cx))
    }
}

// ── Pure helpers ───────────────────────────────────────────────────────────

/// An output's name, counted from 1: Out 1 is the instrument's own
/// channel; Out 2 … Out 16 are the mixer strips added for them, which carry
/// the same number.
pub fn output_label(output: u8) -> String {
    format!("Out {}", output + 1)
}

/// `outputs` with each pad of `bank` on an output of its own: its first pad
/// on Out 1, the next on Out 2, … The other banks keep theirs.
pub fn one_output_per_pad(outputs: [u8; PADS], bank: usize) -> [u8; PADS] {
    let mut next = outputs;
    for (offset, output) in next
        .iter_mut()
        .skip(bank * BANK_PADS)
        .take(BANK_PADS)
        .enumerate()
    {
        *output = offset.min(OUTPUTS - 1) as u8;
    }
    next
}

/// Every pad's output, in pad order.
pub fn pad_outputs(params: &drumsampler::Params) -> [u8; PADS] {
    std::array::from_fn(|pad| params.pads[pad].output)
}

/// The bank a pad is in, and its letter.
pub fn bank_of(pad: usize) -> usize {
    pad / BANK_PADS
}

pub fn bank_letter(bank: usize) -> char {
    (b'A' + bank.min(25) as u8) as char
}

/// Fader travel for a gain: linear in dB across the fader's span.
pub fn fader_position(db: f32) -> f32 {
    ((db - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0)
}

/// The gain a fader dragged `rise` design units up from `start` sets, to
/// 0.1 dB.
pub fn fader_dragged(start: f32, rise: f32) -> f32 {
    let db = start + rise / FADER_H * (FADER_MAX_DB - FADER_MIN_DB);
    ((db * 10.0).round() / 10.0).clamp(FADER_MIN_DB, FADER_MAX_DB)
}

/// "01" … "64".
pub fn pad_number(index: usize) -> String {
    format!("{:02}", index + 1)
}

/// The region a pad plays, as `drumsampler::region_frames` reads it: in
/// order, and at least [`MIN_REGION`] long.
pub fn effective_region(pad: &Pad) -> (f32, f32) {
    let (a, b) = (pad.start.clamp(0.0, 1.0), pad.end.clamp(0.0, 1.0));
    let (mut lo, mut hi) = if a <= b { (a, b) } else { (b, a) };
    if hi - lo < MIN_REGION {
        hi = (lo + MIN_REGION).min(1.0);
        lo = hi - MIN_REGION;
    }
    (lo, hi)
}

/// `pad` with one region edge moved to `fraction`, kept [`MIN_REGION`]
/// clear of the other.
pub fn with_edge(pad: &Pad, edge: RegionEdge, fraction: f32) -> Pad {
    let (lo, hi) = effective_region(pad);
    let mut next = pad.clone();
    match edge {
        RegionEdge::Start => next.start = fraction.clamp(0.0, hi - MIN_REGION),
        RegionEdge::End => next.end = fraction.clamp(lo + MIN_REGION, 1.0),
    }
    next
}

/// The edge within `tolerance` of `fraction`, the nearer one if both.
pub fn edge_near(pad: &Pad, fraction: f32, tolerance: f32) -> Option<RegionEdge> {
    let (lo, hi) = effective_region(pad);
    let (to_start, to_end) = ((fraction - lo).abs(), (fraction - hi).abs());
    if to_start.min(to_end) > tolerance {
        None
    } else if to_start <= to_end {
        Some(RegionEdge::Start)
    } else {
        Some(RegionEdge::End)
    }
}

/// How long the region plays, at the pad's and the kit's tune.
pub fn region_seconds(pad: &Pad, waveform: &DrumPadWaveform, master_tune: f32) -> f32 {
    if waveform.frames == 0 || waveform.sample_rate == 0 {
        return 0.0;
    }
    let (lo, hi) = effective_region(pad);
    let speed = 2.0_f32.powf((pad.tune_semitones + master_tune) / 12.0);
    (hi - lo) * waveform.frames as f32 / waveform.sample_rate as f32 / speed
}

/// The amp envelope's level `t` seconds after the hit: attack, hold, then an
/// exponential decay to −60 dB at the decay time. No decay plays the region
/// out at full level.
pub fn envelope_at(pad: &Pad, t: f32) -> f32 {
    let attack = pad.attack_ms.max(0.0) / 1_000.0;
    if t < attack {
        return if attack > 0.0 { t / attack } else { 1.0 };
    }
    if pad.decay_ms <= 0.0 {
        return 1.0;
    }
    let hold = pad.hold_ms.max(0.0) / 1_000.0;
    if t < attack + hold {
        return 1.0;
    }
    let level = DECAY_FLOOR.powf((t - attack - hold) / (pad.decay_ms / 1_000.0));
    if level <= DECAY_FLOOR {
        0.0
    } else {
        level
    }
}

/// The pad filter's response in dB at `hz`: the analog prototype of the
/// DSP's state-variable filter, with its resonance → Q mapping.
pub fn filter_db(pad: &Pad, hz: f32) -> f32 {
    let q = MIN_Q * (MAX_Q / MIN_Q).powf(pad.resonance.clamp(0.0, 100.0) / 100.0);
    let k = 1.0 / q;
    let w = hz / pad.cutoff_hz.max(1.0);
    let den = ((1.0 - w * w).powi(2) + (k * w).powi(2)).sqrt().max(1.0e-9);
    let magnitude = match pad.filter_mode {
        FilterMode::Off => 1.0,
        FilterMode::LowPass => 1.0 / den,
        FilterMode::HighPass => w * w / den,
        FilterMode::BandPass => w / den,
    };
    20.0 * magnitude.max(1.0e-6).log10()
}

/// A held peak as a 0..1 bar: the top 60 dB.
pub fn level_unit(linear: f32) -> f32 {
    if linear > 0.0 {
        (1.0 + linear.log10() / 3.0).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The value a pad drag sets: `start` moved by `rise` design units upward.
pub fn adjusted(field: PadAdjust, start: f32, rise: f32) -> f32 {
    match field {
        PadAdjust::Gain => {
            ((start + rise * GAIN_PER_PX) * 10.0)
                .round()
                .clamp(-600.0, 120.0)
                / 10.0
        }
        PadAdjust::Tune => (start + rise * TUNE_PER_PX).round().clamp(-24.0, 24.0),
    }
}

/// Square-root knob travel for a time: short times get most of the sweep.
pub(crate) fn time_position(ms: f32, max: f32) -> f32 {
    (ms.max(0.0) / max).sqrt().clamp(0.0, 1.0)
}

pub(crate) fn time_from_position(position: f32, max: f32) -> f32 {
    let ms = position.clamp(0.0, 1.0).powi(2) * max;
    if ms < 10.0 {
        (ms * 10.0).round() / 10.0
    } else {
        ms.round()
    }
}

pub(crate) fn format_db(db: f32) -> String {
    plate::fmt_db(db)
}

pub(crate) fn format_semis(semis: f32) -> String {
    let semis = semis.round() as i32;
    if semis < 0 {
        format!("\u{2212}{} st", -semis)
    } else if semis > 0 {
        format!("+{semis} st")
    } else {
        "0 st".to_string()
    }
}

/// The pads in choke group `group` (1–8): a hit on one cuts the others.
pub fn choke_members(params: &drumsampler::Params, group: u8) -> Vec<usize> {
    if group == 0 {
        return Vec::new();
    }
    (0..PADS)
        .filter(|index| params.pads[*index].choke_group == group)
        .collect()
}

// ── Panel ──────────────────────────────────────────────────────────────────

/// The whole panel at plate scale `s`. `search_field` is the library's
/// search box (the window owns its text state), `query` what it holds;
/// `wave_bounds` is filled in by the waveform's paint, so the window can
/// turn a pointer x into a position in the sample.
pub fn drum_sampler_panel(
    panel: &DrumSamplerPanelState,
    cb: DrumSamplerCallbacks,
    search_field: AnyElement,
    query: &str,
    wave_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let anywhere = cb.on_drop.clone();
    let selected = panel.selected;
    let mut children = vec![
        well_view(panel, &cb, wave_bounds, s),
        match panel.view {
            DrumView::Pads => pads_deck(panel, &cb, search_field, query, s),
            DrumView::Mixer => mixer_deck(panel, &cb, s),
        },
    ];
    if let Some(note) = panel.note.clone() {
        children.push(note_banner(note, s));
    }
    div()
        .relative()
        .size_full()
        .group(DROP_GROUP)
        // A drop that lands on neither a pad nor the library loads the
        // selected pad.
        .on_drop::<ExternalPaths>(move |paths, _, cx| {
            anywhere((Some(selected), paths.paths().to_vec()), cx)
        })
        .child(plate_frame(s, children))
        .child(drop_hint(s))
        .into_any_element()
}

/// A short-lived note over the top of the deck.
fn note_banner(note: String, s: f32) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .w(px(PLATE_W * s))
        .top(px((DECK_TOP - 13.0) * s))
        .flex()
        .justify_center()
        .child(
            div()
                .px(px(14.0 * s))
                .py(px(5.0 * s))
                .rounded(px(4.0 * s))
                .border(px(1.0))
                .border_color(accent())
                .bg(color(0x1d1d1d))
                .child(words(note, 12.0, WHITE, s)),
        )
        .into_any_element()
}

/// Shown along the foot while a file is dragged over the window.
fn drop_hint(s: f32) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .justify_center()
        .py(px(7.0 * s))
        .opacity(0.0)
        .group_drag_over::<ExternalPaths>(DROP_GROUP, |style| style.opacity(1.0))
        .bg(color_a(ACCENT, 0.18))
        .border_t(px(1.0))
        .border_color(accent())
        .child(words(
            "Drop on a pad to load it there \u{2014} several files fill the pads after it \
             \u{00B7} on Samples to add them to the folder \u{00B7} anywhere else loads the \
             selected pad",
            12.0,
            WHITE,
            s,
        ))
        .into_any_element()
}

// ── The well ───────────────────────────────────────────────────────────────

fn loaded_count(pads: &[Pad]) -> usize {
    pads.iter().filter(|pad| pad.sample_name.is_some()).count()
}

fn well_view(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    wave_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let loaded = loaded_count(&panel.params.pads);
    let on_view = cb.on_view.clone();
    let view_tabs = instrument_plate::tabs(
        "drum-view",
        &["Pads", "Mixer"],
        match panel.view {
            DrumView::Pads => 0,
            DrumView::Mixer => 1,
        },
        accent(),
        s,
        Rc::new(move |index, _window, cx| {
            on_view(
                if index == 0 {
                    DrumView::Pads
                } else {
                    DrumView::Mixer
                },
                cx,
            )
        }),
    );
    let middle = vec![view_tabs, bank_tabs(panel, cb, s)];
    let right = master_knobs(panel, cb, s);
    plate::well(s, WELL_TOP, WELL_H)
        .child(instrument_plate::header(
            "drum-header",
            s,
            "Drum Sampler",
            &format!("64-pad kit \u{00B7} {loaded} loaded"),
            accent(),
            middle,
            right,
            None,
        ))
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .px(px(8.0 * s))
                .pb(px(8.0 * s))
                .child(sample_display(panel, cb, wave_bounds, s)),
        )
        .into_any_element()
}

/// The four banks, each with how many of its pads hold a sample.
fn bank_tabs(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> AnyElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.0 * s))
        .child(
            div()
                .mr(px(4.0 * s))
                .child(caption("Bank", 10.0, 0.12, FAINT, s)),
        );
    for bank in 0..BANKS {
        let loaded = loaded_count(&panel.params.pads[bank * BANK_PADS..(bank + 1) * BANK_PADS]);
        let on_bank = cb.on_bank.clone();
        let label = if loaded > 0 {
            format!("{} \u{00B7} {loaded}", bank_letter(bank))
        } else {
            bank_letter(bank).to_string()
        };
        row = row.child(
            plate::pill(
                ("drum-bank", bank),
                label,
                panel.bank == bank,
                accent(),
                plate::PillLook::words(28.0, 0.0, 12.0),
                true,
                s,
                move |_, _, cx| on_bank(bank, cx),
            )
            .min_w(px(36.0 * s))
            .px(px(8.0 * s)),
        );
    }
    row.into_any_element()
}

/// The kit's master gain and tune, as compact knobs that fit the header.
fn master_knobs(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    s: f32,
) -> Vec<AnyElement> {
    let p = &panel.params;
    let (gain, tune) = (p.master_gain_db, p.master_tune);
    let on_gain = cb.on_set_master.clone();
    let on_tune = cb.on_set_master.clone();
    vec![
        header_knob(
            "drum-master-gain",
            Dial::new("masterGain", "Gain", -60.0, 12.0, 0.0, DialUnit::Db),
            gain,
            format_db(gain),
            s,
            Rc::new(move |value, cx| on_gain(((value * 10.0).round() / 10.0, tune), cx)),
        ),
        header_knob(
            "drum-master-tune",
            Dial::new("masterTune", "Tune", -24.0, 24.0, 0.0, DialUnit::Plain).bipolar(),
            tune,
            format_semis(tune),
            s,
            Rc::new(move |value, cx| on_tune((gain, value.round()), cx)),
        ),
    ]
}

/// A knob with its caption and value beside it, short enough for the
/// header row: drag it, wheel it, double-click it to reset — as every plate
/// knob.
fn header_knob(
    key: &'static str,
    dial: Dial,
    value: f32,
    readout: String,
    s: f32,
    set: KnobSet,
) -> AnyElement {
    let look = plate::KnobLook::small(30.0);
    // A plate knob cell with its label and value hidden: the knob alone,
    // with its drag, wheel and reset.
    let cell = plate::KnobCell::new(30.0, 0.1, 0.1).gap(0.0);
    let knob = div()
        .h(px(30.0 * s))
        .overflow_hidden()
        .child(plate::knob_with(
            key,
            Dial { label: "", ..dial },
            value,
            Some(String::new()),
            look,
            cell,
            accent(),
            true,
            s,
            set,
        ));
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(
            div()
                .flex()
                .flex_col()
                .items_end()
                .child(caption(dial.label, 9.5, 0.12, SOFT, s))
                .child(
                    div()
                        .min_w(px(58.0 * s))
                        .flex()
                        .justify_end()
                        .child(numbers(readout, 12.0, WHITE, s)),
                ),
        )
        .child(knob)
        .into_any_element()
}

/// The selected pad's sample: the region it plays, flagged S and E, and the
/// envelope over it in time.
fn sample_display(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
    s: f32,
) -> AnyElement {
    let index = panel.selected;
    let pad = panel.pad().clone();
    let waveform = panel.waveforms[index].clone();
    let colors = display_colors(accent());
    let (lo, hi) = effective_region(&pad);
    let seconds = waveform
        .as_ref()
        .filter(|w| w.sample_rate > 0)
        .map_or(0.0, |w| w.frames as f64 / w.sample_rate as f64);
    let plays = waveform
        .as_ref()
        .map_or(0.0, |w| region_seconds(&pad, w, panel.params.master_tune));
    let pairs: Option<Arc<Vec<(f32, f32)>>> = waveform.as_ref().map(|w| {
        Arc::new(
            w.peaks
                .iter()
                .map(|peak| {
                    let v = *peak as f32 / 255.0;
                    (-v, v)
                })
                .collect(),
        )
    });
    let has_sample = pairs.is_some();
    let press = cb.on_waveform_press.clone();
    let context = cb.on_context.clone();
    let envelope_pad = pad.clone();

    let name = if panel.loading[index] {
        "Loading\u{2026}".to_string()
    } else {
        pad.sample_name
            .clone()
            .unwrap_or_else(|| "No sample".into())
    };
    let meta = waveform
        .as_ref()
        .map(|w| {
            format!(
                "{} \u{00B7} {} \u{00B7} {:.1} kHz",
                format_seconds(seconds),
                if w.channels == 1 { "mono" } else { "stereo" },
                w.sample_rate as f32 / 1_000.0
            )
        })
        .unwrap_or_default();
    let reading = |label: &str, value: String| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(5.0 * s))
            .child(caption(label, 9.5, 0.12, FAINT, s))
            .child(numbers(value, 11.5, INK, s))
    };
    let mut head = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(DISPLAY_HEAD_H * s))
        .px(px(10.0 * s))
        .gap(px(10.0 * s))
        .border_b(px(1.0))
        .border_color(color(BOX_EDGE))
        .child(caption(
            &format!(
                "Pad {} \u{00B7} {}",
                pad_number(index),
                note_label(pad.note)
            ),
            10.5,
            0.12,
            ACCENT,
            s,
        ))
        .child(
            div()
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .overflow_hidden()
                .child(
                    words(name, 13.0, if has_sample { WHITE } else { FAINT }, s)
                        .font_weight(FontWeight::MEDIUM),
                ),
        );
    if let Some(error) = panel.errors[index].clone() {
        head = head.child(
            div()
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .overflow_hidden()
                .child(words(error, 11.0, ERROR, s)),
        );
    }
    head = head
        .child(div().flex_1())
        .child(numbers(meta, 10.5, GHOST, s))
        .child(reading("Start", format!("{:.1}%", lo * 100.0)))
        .child(reading("End", format!("{:.1}%", hi * 100.0)))
        .child(reading(
            "Plays",
            if plays > 0.0 {
                fmt_time(plays * 1_000.0)
            } else {
                "\u{2014}".into()
            },
        ));

    let mut wave = div()
        .id("drum-wave")
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .overflow_hidden()
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(gpui::MouseButton::Left, move |event, _, cx| {
                    press((f32::from(event.position.x), event.click_count), cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, _, cx| {
                    context(
                        (
                            DrumMenuTarget::Waveform,
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                        ),
                        cx,
                    )
                })
        })
        .child(
            canvas(
                move |bounds, _, _| bounds_out.set(Some(bounds)),
                move |bounds, _, window, _| {
                    let frame = paint_display_frame(window, bounds, seconds, &colors);
                    if !paint_waveform(window, &frame, pairs.as_deref(), &colors) {
                        return;
                    }
                    let (x0, y0, w, wave_h) = (frame.x0, frame.y0, frame.w, frame.wave_h);
                    let (start, end) = (frame.at(lo), frame.at(hi));
                    paint_rect(window, (x0, y0, start - x0, wave_h), colors.skipped);
                    paint_rect(window, (end, y0, x0 + w - end, wave_h), colors.skipped);
                    // The envelope, in time across the region, from the edge
                    // playback starts at, shaded under.
                    if plays > 0.0 {
                        let steps = ((end - start).max(2.0) as usize).min(512);
                        let mut line = Vec::with_capacity(steps + 1);
                        for i in 0..=steps {
                            let along = i as f32 / steps as f32;
                            let level = envelope_at(&envelope_pad, along * plays);
                            let fraction = if envelope_pad.reverse {
                                hi - along * (hi - lo)
                            } else {
                                lo + along * (hi - lo)
                            };
                            line.push((frame.at(fraction), frame.mid - level * frame.half));
                        }
                        let floor = frame.mid;
                        let mut area = line.clone();
                        area.push((line[line.len() - 1].0, floor));
                        area.push((line[0].0, floor));
                        paint_polygon(window, &area, color_a(0xffffff, 0.05));
                        plate::paint_polyline(window, &line, 1.5, color_a(0xffffff, 0.8), None);
                    }
                    for x in [start, end - 1.5] {
                        let x = x.clamp(x0, x0 + w - 1.5);
                        paint_rect(window, (x, y0, 1.5, wave_h), colors.marker);
                    }
                    window.paint_quad(
                        gpui::fill(plate::rect(start, y0, FLAG_W, FLAG_H), colors.marker)
                            .corner_radii(px(2.0)),
                    );
                    window.paint_quad(
                        gpui::fill(plate::rect(end - FLAG_W, y0, FLAG_W, FLAG_H), colors.marker)
                            .corner_radii(px(2.0)),
                    );
                },
            )
            .absolute()
            .inset_0(),
        );
    if has_sample {
        for (fraction, letter, inside) in [(lo, "S", false), (hi, "E", true)] {
            let flag = div()
                .absolute()
                .top(px(0.0))
                .left(relative(fraction))
                .w(px(FLAG_W))
                .h(px(FLAG_H))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .font_weight(FontWeight::BOLD)
                .text_color(color(WELL))
                .child(letter);
            wave = wave.child(if inside { flag.ml(px(-FLAG_W)) } else { flag });
        }
        for (fraction, label) in ruler_labels(seconds) {
            wave = wave.child(
                div()
                    .absolute()
                    .bottom(px(1.0))
                    .left(relative(fraction))
                    .pl(px(3.0))
                    .child(numbers(label, 9.5, GHOST, s)),
            );
        }
    } else {
        wave = wave.child(empty_display(panel.loading[index], s));
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .bg(color(BOX_GROUND))
        .border(px(1.0))
        .border_color(color(BOX_EDGE))
        .overflow_hidden()
        .child(head)
        .child(wave)
        .into_any_element()
}

/// What an empty pad's display says, over its blank frame.
fn empty_display(loading: bool, s: f32) -> AnyElement {
    let (title, hint) = if loading {
        ("Loading\u{2026}", "")
    } else {
        (
            "Drop an audio file here",
            "or click a file in Samples to load it onto this pad",
        )
    };
    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(4.0 * s))
        .child(words(title, 14.0, PALE, s).font_weight(FontWeight::MEDIUM))
        .when(!hint.is_empty(), |empty| {
            empty.child(words(hint, 11.5, FAINT, s))
        })
        .into_any_element()
}

// ── The deck: pads and the Samples folder ──────────────────────────────────

fn pads_deck(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    search_field: AnyElement,
    query: &str,
    s: f32,
) -> AnyElement {
    plate::deck(s, DECK_TOP, DECK_H)
        .p(px(DECK_PAD * s))
        .gap(px(DECK_GAP * s))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .w(px(PADS_W * s))
                .gap(px(DECK_GAP * s))
                .child(pad_grid(panel, cb, s))
                .child(library(panel, cb, search_field, query, s)),
        )
        .child(pad_cards(panel, cb, s))
        .into_any_element()
}

fn pad_grid(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks, s: f32) -> AnyElement {
    let mut grid = div().flex().flex_col().flex_shrink_0().gap(px(PAD_GAP * s));
    let first = panel.bank * BANK_PADS;
    for row in BANK_GRID.chunks(4) {
        let mut line = div().flex().flex_row().w_full().gap(px(PAD_GAP * s));
        for &offset in row {
            line = line.child(pad_cell(panel, first + offset, cb, s));
        }
        grid = grid.child(line);
    }
    grid.into_any_element()
}

/// A pad's small tag: its choke group, filter, output, mute or solo.
fn tag(text: String, ink: u32, s: f32) -> AnyElement {
    div()
        .flex_shrink_0()
        .px(px(3.0 * s))
        .rounded(px(2.0 * s))
        .bg(color_a(ink, 0.16))
        .child(
            numbers(text, 8.5, ink, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(12.0 * s)),
        )
        .into_any_element()
}

fn pad_cell(
    panel: &DrumSamplerPanelState,
    index: usize,
    cb: &DrumSamplerCallbacks,
    s: f32,
) -> AnyElement {
    let pad = &panel.params.pads[index];
    let empty = pad.sample_name.is_none();
    let selected = panel.selected == index;
    let loading = panel.loading[index];
    let failed = panel.errors[index].is_some();
    let meter = level_unit(panel.levels[index]);
    let pressed = panel.pressed == Some(index);
    let rest = if pressed {
        0x181818
    } else if selected {
        0x2a2a2a
    } else {
        0x232323
    };
    // The pad lights while it sounds, with how loud.
    let lit = Rgba {
        a: 0.10 + 0.28 * meter,
        ..accent()
    };
    let border = if failed {
        color(ERROR)
    } else if selected {
        accent()
    } else {
        color(0x3c3c3c)
    };
    let press = cb.on_pad_press.clone();
    let drop = cb.on_drop.clone();
    let context = cb.on_context.clone();

    let mut tags = div().flex().flex_row().gap(px(2.0 * s)).min_w(px(0.0));
    if pad.choke_group > 0 {
        // Pads in the selected pad's group are marked in the accent: the
        // ones it cuts, and that cut it.
        let linked = panel.pad().choke_group == pad.choke_group;
        tags = tags.child(tag(
            format!("C{}", pad.choke_group),
            if linked { ACCENT } else { PALE },
            s,
        ));
    }
    if pad.filter_mode != FilterMode::Off {
        tags = tags.child(tag("F".into(), PALE, s));
    }
    if pad.output > 0 {
        tags = tags.child(tag(format!("\u{2192}{}", pad.output + 1), PALE, s));
    }
    if pad.muted {
        tags = tags.child(tag("M".into(), ERROR, s));
    }
    if pad.solo {
        tags = tags.child(tag("S".into(), SOLO, s));
    }
    let name = if loading {
        "Loading\u{2026}".to_string()
    } else {
        pad.sample_name.clone().unwrap_or_else(|| "Empty".into())
    };
    let peaks = panel.waveforms[index]
        .as_ref()
        .map(|w| Arc::new(w.peaks.clone()));
    let region = effective_region(pad);
    let adjust = panel
        .adjusting
        .filter(|(pad_index, _)| *pad_index == index)
        .map(|(_, field)| match field {
            PadAdjust::Gain => ("Gain", format_db(pad.gain_db)),
            PadAdjust::Tune => ("Tune", format_semis(pad.tune_semitones)),
        });

    div()
        .id(("drum-pad", index))
        .relative()
        .flex()
        .flex_col()
        // Four equal columns whatever the pads hold: without a zero minimum,
        // a long file name would widen its pad and squeeze its row.
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .h(px(PAD_H * s))
        .px(px(7.0 * s))
        .py(px(6.0 * s))
        .gap(px(3.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(border)
        .bg(color(rest))
        .when(meter > 0.02, |cell| cell.bg(lit))
        .cursor(gpui::CursorStyle::PointingHand)
        .when(!selected && meter <= 0.02, |cell| {
            cell.hover(|style| style.bg(color(0x2c2c2c)))
        })
        .drag_over::<ExternalPaths>(move |style, _, _, _| {
            style.border_color(accent()).bg(color_a(ACCENT, 0.2))
        })
        .on_drop::<ExternalPaths>(move |paths, _, cx| {
            drop((Some(index), paths.paths().to_vec()), cx)
        })
        .on_mouse_down(gpui::MouseButton::Right, move |event, _, cx| {
            context(
                (
                    DrumMenuTarget::Pad(index),
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                ),
                cx,
            )
        })
        .on_mouse_down(gpui::MouseButton::Left, move |event, _, cx| {
            press(
                PadPress {
                    pad: index,
                    y: f32::from(event.position.y),
                    shift: event.modifiers.shift,
                },
                cx,
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0 * s))
                .h(px(14.0 * s))
                .child(
                    numbers(
                        pad_number(index),
                        11.0,
                        if selected { ACCENT } else { WHITE },
                        s,
                    )
                    .font_weight(FontWeight::SEMIBOLD),
                )
                .child(tags)
                .child(div().flex_1())
                .child(numbers(note_label(pad.note), 9.5, FAINT, s)),
        )
        .child(mini_wave(peaks, region, empty, s))
        .child(
            div().w_full().overflow_hidden().child(
                words(name, 10.5, if empty { GHOST } else { PALE }, s)
                    .w_full()
                    .truncate(),
            ),
        )
        .child(
            div()
                .w_full()
                .h(px(3.0 * s))
                .rounded(px(1.5 * s))
                .bg(color(0x161616))
                .child(
                    div()
                        .h_full()
                        .w(relative(meter))
                        .rounded(px(1.5 * s))
                        .bg(accent()),
                ),
        )
        .when(panel.hits[index] > 0, |cell| {
            cell.child(hit_flash(index, panel.hits[index], s))
        })
        .when_some(adjust, |cell, (label, value)| {
            cell.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0 * s))
                    .bg(color_a(0x141414, 0.9))
                    .child(caption(label, 9.5, 0.12, SOFT, s))
                    .child(numbers(value, 14.0, WHITE, s)),
            )
        })
        .into_any_element()
}

/// A hit's flash over its pad or strip: bright at once, fading out. Each hit
/// has its own id, so a retrigger starts it over; GPUI's animation element
/// drives the frames, and only while it runs.
pub(crate) fn hit_flash(index: usize, hit: u64, s: f32) -> AnyElement {
    let base = accent();
    div()
        .absolute()
        .inset_0()
        .rounded(px(4.0 * s))
        .border(px(2.0))
        .with_animation(
            ("drum-hit", index * 1_000_000 + (hit % 1_000_000) as usize),
            Animation::new(HIT_FLASH).with_easing(|t| 1.0 - (1.0 - t).powi(2)),
            move |flash, delta| {
                let left = 1.0 - delta;
                flash
                    .bg(Rgba {
                        a: 0.38 * left,
                        ..base
                    })
                    .border_color(Rgba { a: left, ..base })
            },
        )
        .into_any_element()
}

/// A pad's sample as a small mirrored silhouette, the played region bright
/// and the trimmed ends dim.
fn mini_wave(peaks: Option<Arc<Vec<u8>>>, (lo, hi): (f32, f32), empty: bool, s: f32) -> AnyElement {
    let shape = if empty {
        color(0x4a4a4a)
    } else {
        color_a(ACCENT, 0.75)
    };
    let trim = color_a(0x1d1d1d, 0.72);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (x0, y0) = plate::xy(bounds.origin);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let mid = y0 + h * 0.5;
            let Some(peaks) = peaks.as_ref().filter(|peaks| peaks.len() >= 2) else {
                paint_rect(window, (x0, mid, w, 1.0), shape);
                return;
            };
            let last = (peaks.len() - 1) as f32;
            let at = |i: usize| x0 + i as f32 / last * w;
            let mut outline: Vec<(f32, f32)> = Vec::with_capacity(peaks.len() * 2);
            for (i, peak) in peaks.iter().enumerate() {
                outline.push((at(i), mid - *peak as f32 / 255.0 * h * 0.5));
            }
            for (i, peak) in peaks.iter().enumerate().rev() {
                outline.push((at(i), mid + *peak as f32 / 255.0 * h * 0.5));
            }
            paint_polygon(window, &outline, shape);
            paint_rect(window, (x0, y0, lo * w, h), trim);
            paint_rect(window, (x0 + hi * w, y0, (1.0 - hi) * w, h), trim);
        },
    )
    .w_full()
    .h(px(MINI_WAVE_H * s))
    .flex_shrink_0()
    .into_any_element()
}

// ── Samples folder ─────────────────────────────────────────────────────────

fn library(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    search_field: AnyElement,
    query: &str,
    s: f32,
) -> AnyElement {
    let drop = cb.on_drop.clone();
    let refresh = cb.on_refresh.clone();
    let browse = cb.on_browse.clone();
    let current = panel.pad().sample_name.clone();
    let needle = query.trim().to_lowercase();
    let mut list = div()
        .id("drum-library-list")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .gap(px(1.0 * s));
    match &panel.files {
        None => list = list.child(library_note("Reading the Samples folder\u{2026}".into(), s)),
        Some(files) => {
            let shown: Vec<&BuiltinFileEntry> = files
                .iter()
                .filter(|file| needle.is_empty() || file.file_name.to_lowercase().contains(&needle))
                .collect();
            if shown.is_empty() {
                list = list.child(library_note(
                    if files.is_empty() {
                        "The Samples folder is empty \u{2014} drop audio files here, or use \
                         Browse\u{2026}"
                            .into()
                    } else {
                        format!("No sample matches \u{201C}{}\u{201D}.", query.trim())
                    },
                    s,
                ));
            }
            for (row, file) in shown.into_iter().enumerate() {
                let is_current = current.as_deref() == Some(file.file_name.as_str());
                let load = cb.on_load_file.clone();
                let name = file.file_name.clone();
                let context = cb.on_context.clone();
                let menu_name = file.file_name.clone();
                list = list.child(
                    div()
                        .id(("drum-library-file", row))
                        .flex()
                        .flex_row()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(8.0 * s))
                        .h(px(20.0 * s))
                        .px(px(8.0 * s))
                        .rounded(px(3.0 * s))
                        .when(is_current, |row| row.bg(color_a(ACCENT, 0.12)))
                        .on_mouse_down(gpui::MouseButton::Right, move |event, _, cx| {
                            context(
                                (
                                    DrumMenuTarget::File(menu_name.clone()),
                                    f32::from(event.position.x),
                                    f32::from(event.position.y),
                                ),
                                cx,
                            )
                        })
                        .when(panel.connected, |row| {
                            row.cursor(gpui::CursorStyle::PointingHand)
                                .hover(|style| style.bg(color(0x262626)))
                                .on_click(move |_, _, cx| load(name.clone(), cx))
                        })
                        .child(
                            div().flex_1().min_w(px(0.0)).overflow_hidden().child(
                                words(
                                    file.file_name.clone(),
                                    11.5,
                                    if is_current { ACCENT } else { PALE },
                                    s,
                                )
                                .w_full()
                                .truncate(),
                            ),
                        )
                        .child(numbers(
                            format!("{} KB", (file.size_bytes / 1_024).max(1)),
                            10.0,
                            GHOST,
                            s,
                        )),
                );
            }
        }
    }
    let target: SharedString = format!("\u{2192} Pad {}", pad_number(panel.selected)).into();
    div()
        .id("drum-library")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .gap(px(6.0 * s))
        .px(px(10.0 * s))
        .py(px(9.0 * s))
        .rounded(px(4.0 * s))
        .border(px(1.0))
        .border_color(color(CARD_EDGE))
        .bg(color(CARD))
        .drag_over::<ExternalPaths>(move |style, _, _, _| {
            style.border_color(accent()).bg(color_a(ACCENT, 0.1))
        })
        .on_drop::<ExternalPaths>(move |paths, _, cx| drop((None, paths.paths().to_vec()), cx))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(px(8.0 * s))
                .child(plate::card_title("Samples", s))
                .child(numbers(target, 10.5, FAINT, s))
                .child(div().flex_1())
                .child(plate::pill(
                    "drum-library-refresh",
                    "Refresh",
                    false,
                    accent(),
                    plate::PillLook::words(24.0, 10.0, 11.5),
                    true,
                    s,
                    move |_, _, cx| refresh((), cx),
                ))
                .child(plate::pill(
                    "drum-library-browse",
                    "Browse\u{2026}",
                    true,
                    accent(),
                    plate::PillLook::words(24.0, 10.0, 11.5),
                    panel.connected,
                    s,
                    move |_, _, cx| browse((), cx),
                )),
        )
        .child(div().flex_shrink_0().child(search_field))
        .child(list)
        .into_any_element()
}

fn library_note(text: String, s: f32) -> AnyElement {
    div()
        .py(px(8.0 * s))
        .px(px(8.0 * s))
        .text_size(type_px(11.0, s))
        .line_height(px(14.0 * s))
        .text_color(color(FAINT))
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad() -> Pad {
        drumsampler::default_pad(0)
    }

    #[test]
    fn the_grid_puts_pad_one_bottom_left() {
        assert_eq!(BANK_GRID[12], 0);
        assert_eq!(BANK_GRID[3], 15);
        let mut seen = BANK_GRID.to_vec();
        seen.sort_unstable();
        assert_eq!(seen, (0..BANK_PADS).collect::<Vec<_>>());
        assert_eq!(BANKS, 4);
        assert_eq!((bank_of(15), bank_of(16), bank_of(63)), (0, 1, 3));
        assert_eq!(bank_letter(2), 'C');
    }

    #[test]
    fn the_plate_regions_fit_the_artboard() {
        assert!(WELL_TOP + WELL_H < DECK_TOP);
        assert!(DECK_TOP + DECK_H <= PLATE_H);
        // The pads' column: four rows of pads and the Samples folder under
        // them, inside the deck.
        let grid = 4.0 * PAD_H + 3.0 * PAD_GAP;
        let inner = DECK_H - 2.0 * DECK_PAD;
        assert!(
            grid + DECK_GAP + 150.0 <= inner,
            "room for the Samples list"
        );
        assert!(FADER_H < inner);
    }

    #[test]
    fn region_edges_keep_their_order_and_minimum() {
        let mut p = pad();
        p.start = 0.2;
        p.end = 0.6;
        assert_eq!(
            with_edge(&p, RegionEdge::Start, 0.9).start,
            0.6 - MIN_REGION
        );
        assert_eq!(with_edge(&p, RegionEdge::End, 0.0).end, 0.2 + MIN_REGION);
        assert_eq!(edge_near(&p, 0.21, 0.02), Some(RegionEdge::Start));
        assert_eq!(edge_near(&p, 0.59, 0.02), Some(RegionEdge::End));
        assert_eq!(edge_near(&p, 0.4, 0.02), None);
        // A swapped pair reads as the DSP plays it.
        p.start = 0.7;
        p.end = 0.3;
        assert_eq!(effective_region(&p), (0.3, 0.7));
    }

    #[test]
    fn the_envelope_attacks_holds_and_decays_to_minus_sixty() {
        let mut p = pad();
        p.attack_ms = 10.0;
        p.hold_ms = 20.0;
        p.decay_ms = 100.0;
        assert!((envelope_at(&p, 0.005) - 0.5).abs() < 1.0e-4);
        assert_eq!(envelope_at(&p, 0.02), 1.0);
        assert!((envelope_at(&p, 0.08) - 0.001_f32.powf(0.5)).abs() < 1.0e-4);
        assert_eq!(envelope_at(&p, 0.2), 0.0);
        p.decay_ms = 0.0;
        assert_eq!(envelope_at(&p, 5.0), 1.0, "no decay plays to the end");
    }

    #[test]
    fn region_time_follows_the_tune() {
        let waveform = DrumPadWaveform {
            name: "kick.wav".into(),
            frames: 48_000,
            channels: 1,
            sample_rate: 48_000,
            peaks: vec![0; 4],
        };
        let mut p = pad();
        p.end = 0.5;
        assert!((region_seconds(&p, &waveform, 0.0) - 0.5).abs() < 1.0e-5);
        p.tune_semitones = 12.0;
        assert!((region_seconds(&p, &waveform, 0.0) - 0.25).abs() < 1.0e-5);
        assert!((region_seconds(&p, &waveform, -12.0) - 0.5).abs() < 1.0e-5);
    }

    #[test]
    fn the_filter_curve_matches_the_pad_filter() {
        let mut p = pad();
        p.filter_mode = FilterMode::LowPass;
        p.cutoff_hz = 1_000.0;
        // Butterworth at no resonance: −3 dB at the cutoff.
        assert!((filter_db(&p, 1_000.0) + 3.01).abs() < 0.05);
        assert!(filter_db(&p, 50.0).abs() < 0.1);
        p.resonance = 100.0;
        assert!((filter_db(&p, 1_000.0) - 20.0 * 12.0_f32.log10()).abs() < 0.05);
    }

    #[test]
    fn outputs_are_named_as_their_mixer_strips() {
        assert_eq!(output_label(0), "Out 1");
        assert_eq!(output_label(2), "Out 3");
        // One per pad, bank B: pads 17–32 take Out 1–16; bank A is left be.
        let mut outputs = [0; PADS];
        outputs[3] = 7;
        let spread = one_output_per_pad(outputs, 1);
        assert_eq!((spread[16], spread[17], spread[31]), (0, 1, 15));
        assert_eq!((spread[3], spread[32]), (7, 0));
    }

    #[test]
    fn faders_span_minus_sixty_to_plus_twelve() {
        assert_eq!(fader_position(FADER_MIN_DB), 0.0);
        assert_eq!(fader_position(FADER_MAX_DB), 1.0);
        assert!((fader_position(0.0) - 60.0 / 72.0).abs() < 1.0e-6);
        // The full height is the full span; a drag rounds to 0.1 dB.
        assert_eq!(fader_dragged(0.0, FADER_H), FADER_MAX_DB);
        assert_eq!(fader_dragged(0.0, -10.0), -3.0);
        assert_eq!(fader_dragged(-59.0, -500.0), FADER_MIN_DB);
    }

    #[test]
    fn choke_groups_list_their_pads() {
        let mut params = drumsampler::default_params();
        params.pads[2].choke_group = 1;
        params.pads[3].choke_group = 1;
        params.pads[9].choke_group = 2;
        assert_eq!(choke_members(&params, 1), vec![2, 3]);
        assert_eq!(choke_members(&params, 2), vec![9]);
        assert!(choke_members(&params, 0).is_empty(), "off is not a group");
    }

    #[test]
    fn pad_drags_step_and_clamp() {
        assert_eq!(adjusted(PadAdjust::Gain, 0.0, 10.0), 2.5);
        assert_eq!(adjusted(PadAdjust::Gain, 0.0, 1_000.0), 12.0);
        assert_eq!(adjusted(PadAdjust::Tune, 0.0, 34.0), 3.0);
        assert_eq!(adjusted(PadAdjust::Tune, 0.0, -1_000.0), -24.0);
        assert_eq!(level_unit(1.0), 1.0);
        assert_eq!(level_unit(0.001), 0.0);
        assert_eq!(level_unit(0.0), 0.0);
    }

    #[test]
    fn envelope_knobs_travel_square_root_and_round_back() {
        assert_eq!(
            time_from_position(
                time_position(400.0, drumsampler::MAX_DECAY_MS),
                drumsampler::MAX_DECAY_MS
            ),
            400.0
        );
        assert_eq!(time_from_position(1.0, ATTACK_MAX_MS), ATTACK_MAX_MS);
        assert_eq!(time_position(0.0, drumsampler::MAX_HOLD_MS), 0.0);
    }

    #[test]
    fn semitones_and_decibels_read_as_the_plates_print_them() {
        assert_eq!(format_semis(3.0), "+3 st");
        assert_eq!(format_semis(-12.0), "\u{2212}12 st");
        assert_eq!(format_semis(0.0), "0 st");
        assert_eq!(format_db(-6.0), "\u{2212}6.0 dB");
    }
}
