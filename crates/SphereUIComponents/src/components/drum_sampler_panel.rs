//! The built-in Drum Sampler's panel, drawn natively.
//!
//! Laid out the way a pad sampler is played and read:
//!
//! * a **top bar** — how many pads hold a sample, and the kit's master gain
//!   and tune;
//! * on the left, the **pads** — sixteen in a 4×4 grid, pad 1 bottom-left as
//!   on a hardware controller. Each shows its number, note, tags (choke
//!   group, filter, mute, solo), a silhouette of its sample with the played
//!   region bright, the file name and a level bar that lights the pad while
//!   it sounds — from whatever played it — and flashes on every hit. Press
//!   a pad to select and hear it, as fast as you like; drag up or down to
//!   set its gain (Shift: its tune). A file dropped on a pad loads there;
//!   several fill the pads after it;
//! * under them, the **Samples folder** — search it, click a file to load it
//!   onto the selected pad, Browse… to bring one in from anywhere; a file
//!   dropped here is only added to the folder;
//! * on the right, the **selected pad**: its waveform with the region it
//!   plays (drag either edge, double-click to reset it) and the envelope laid
//!   over it in time, then the voice, envelope, filter (with its response
//!   curve) and trigger modules.
//!
//! This file only renders; the state lives in
//! [`crate::components::drum_sampler_window::DrumSamplerEditorWindow`].

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use drumsampler::{
    BANK_PADS, FilterMode, MAX_CUTOFF_HZ, MAX_DECAY_MS, MAX_HOLD_MS, MIN_REGION, OUTPUTS, PADS, Pad,
};
use gpui::prelude::FluentBuilder;
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, ExternalPaths, InteractiveElement,
    IntoElement, ParentElement, PathBuilder, Pixels, StatefulInteractiveElement, Styled, Window,
    canvas, div, fill, point, px, relative,
};

use crate::components::builtin_plugin_editor::DrumPadWaveform;
use crate::components::builtin_plugin_files::BuiltinFileEntry;
use crate::components::controls::{
    FbButtonKind, FbSegment, fb_button, fb_checkbox, fb_segment, fb_segmented_track,
    fb_stepper_button,
};
use crate::components::drum_sampler_menu::DrumMenuTarget;
use crate::components::knob::{format_pan_label, knob_bipolar, knob_with_default};
use crate::components::quick_sampler_panel::{
    DisplayColors, FLAG_H, FLAG_W, GRAPH_H, GraphColors, KNOB_SIZE, SAMPLE_DROP_GROUP, VoidCb,
    controls_row, cutoff_from_position, cutoff_position, empty_sample_drop_zone, format_hz,
    format_ms, format_seconds, knob_cell, module, paint_curve, paint_display_frame,
    paint_graph_frame, paint_waveform, rect, ruler_labels, status_banner,
};
use crate::components::soundfont_player_mdi::note_label;
use crate::theme::{Colors, radius, space, typography};

/// Pad 1 bottom-left, as on a hardware pad controller: the grid's rows, top
/// to bottom.
const BANK_GRID: [usize; BANK_PADS] = [12, 13, 14, 15, 8, 9, 10, 11, 4, 5, 6, 7, 0, 1, 2, 3];
/// Banks of [`BANK_PADS`] pads, named A–D.
pub const BANKS: usize = PADS / BANK_PADS;
/// The mixer's faders: how tall, and the gain they span.
const FADER_H: f32 = 176.0;
const FADER_THUMB_H: f32 = 10.0;
pub const FADER_MIN_DB: f32 = -60.0;
pub const FADER_MAX_DB: f32 = 12.0;
const LEFT_W: f32 = 384.0;
const PAD_H: f32 = 86.0;
const MINI_WAVE_H: f32 = 24.0;
const WAVE_H: f32 = 172.0;
/// How near a press must land to a region edge to grab it, in pixels.
pub const EDGE_GRAB_PX: f32 = 10.0;
/// A pad drag's travel: dB and semitones per pixel.
const GAIN_PER_PX: f32 = 0.25;
const TUNE_PER_PX: f32 = 0.1;
/// Pixels a press must move before it is a drag rather than a hit.
pub const DRAG_SLOP_PX: f32 = 4.0;
const ATTACK_MAX_MS: f32 = 250.0;
/// `drumsampler`'s envelope floor: the decay reaches −60 dB at its time.
const DECAY_FLOOR: f32 = 0.001;
const MIN_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
const MAX_Q: f32 = 12.0;
/// How long a pad's flash lasts after a hit.
const HIT_FLASH: Duration = Duration::from_millis(260);

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

pub type PadPressCb = Arc<dyn Fn(&PadPress, &mut Window, &mut App) + 'static>;
/// A whole new pad value for one pad.
pub type PadEditCb = Arc<dyn Fn(&(usize, Pad), &mut Window, &mut App) + 'static>;
/// Dropped files: onto a pad (and the ones after it), or `None` for the
/// Samples folder only.
pub type DropCb = Arc<dyn Fn(&(Option<usize>, Vec<PathBuf>), &mut Window, &mut App) + 'static>;
/// Master gain (dB) and tune (semitones).
pub type MasterCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;
/// A press on the waveform: window-space x, and the click count.
pub type WavePressCb = Arc<dyn Fn(&(f32, usize), &mut Window, &mut App) + 'static>;
pub type FileCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
/// A press on a mixer fader.
#[derive(Clone, Copy, Debug)]
pub struct FaderPress {
    pub pad: usize,
    /// Window-space y, where the drag measures from.
    pub y: f32,
    pub clicks: usize,
}

pub type FaderCb = Arc<dyn Fn(&FaderPress, &mut Window, &mut App) + 'static>;
/// A right-click: on what, and where (window space).
pub type ContextCb = Arc<dyn Fn(&(DrumMenuTarget, f32, f32), &mut Window, &mut App) + 'static>;
pub type ViewCb = Arc<dyn Fn(&DrumView, &mut Window, &mut App) + 'static>;
/// A bank, or a pad, by index.
pub type IndexCb = Arc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;
/// Every pad's output at once (index = pad).
pub type OutputsCb = Arc<dyn Fn(&[u8; PADS], &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct DrumSamplerCallbacks {
    pub on_pad_press: PadPressCb,
    pub on_set_pad: PadEditCb,
    pub on_set_master: MasterCb,
    pub on_drop: DropCb,
    pub on_waveform_press: WavePressCb,
    pub on_load_file: FileCb,
    pub on_browse: VoidCb,
    pub on_refresh: VoidCb,
    pub on_set_outputs: OutputsCb,
    pub on_view: ViewCb,
    pub on_bank: IndexCb,
    pub on_select: IndexCb,
    pub on_fader_press: FaderCb,
    pub on_context: ContextCb,
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

/// The gain a fader dragged `rise` pixels up from `start` sets, to 0.1 dB.
pub fn fader_dragged(start: f32, rise: f32) -> f32 {
    let db = start + rise / FADER_H * (FADER_MAX_DB - FADER_MIN_DB);
    ((db * 10.0).round() / 10.0).clamp(FADER_MIN_DB, FADER_MAX_DB)
}

/// "01" … "16".
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
    if level <= DECAY_FLOOR { 0.0 } else { level }
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

/// The value a pad drag sets: `start` moved by `rise` pixels upward.
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
fn time_position(ms: f32, max: f32) -> f32 {
    (ms.max(0.0) / max).sqrt().clamp(0.0, 1.0)
}

fn time_from_position(position: f32, max: f32) -> f32 {
    let ms = position.clamp(0.0, 1.0).powi(2) * max;
    if ms < 10.0 {
        (ms * 10.0).round() / 10.0
    } else {
        ms.round()
    }
}

fn format_db(db: f32) -> String {
    format!("{db:+.1} dB")
}

fn format_semis(semis: f32) -> String {
    format!("{:+} st", semis.round() as i32)
}

// ── Panel ──────────────────────────────────────────────────────────────────

/// The whole panel. `search_field` is the library's search box (the window
/// owns its text state), `query` what it holds; `wave_bounds` is filled in
/// by the waveform's paint, so the window can turn a pointer x into a
/// position in the sample.
pub fn drum_sampler_panel(
    panel: &DrumSamplerPanelState,
    cb: DrumSamplerCallbacks,
    search_field: AnyElement,
    query: &str,
    wave_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let anywhere = cb.on_drop.clone();
    let selected = panel.selected;
    div()
        .relative()
        .group(SAMPLE_DROP_GROUP)
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        // A drop that lands on neither a pad nor the library loads the
        // selected pad.
        .on_drop::<ExternalPaths>(move |paths, w, cx| {
            anywhere(&(Some(selected), paths.paths().to_vec()), w, cx)
        })
        .child(top_bar(panel, &cb))
        .when_some(panel.note.clone(), |root, note| {
            root.child(status_banner(note))
        })
        .when(panel.view == DrumView::Mixer, |root| {
            root.child(mixer_view(panel, &cb))
        })
        .when(panel.view == DrumView::Pads, |root| {
            root.child(pads_view(panel, &cb, search_field, query, wave_bounds))
        })
        .child(drop_hint())
        .into_any_element()
}

/// The pads of the bank, the Samples folder, and the selected pad's editor.
fn pads_view(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    search_field: AnyElement,
    query: &str,
    wave_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let cb = cb.clone();
    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h(px(0.0))
        .gap(px(space::SECTION))
        .px(px(space::SECTION))
        .pt(px(space::LOOSE))
        .pb(px(space::SECTION))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .w(px(LEFT_W))
                .min_h(px(0.0))
                .gap(px(space::BASE))
                .child(pad_grid(panel, &cb))
                .child(library(panel, &cb, search_field, query)),
        )
        .child(
            div()
                .id("drum-sampler-inspector")
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .overflow_y_scroll()
                .gap(px(space::BASE))
                .child(inspector_head(panel))
                .child(waveform_editor(panel, &cb, wave_bounds))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::BASE))
                        .mt(px(space::TIGHT))
                        .child(voice_module(panel, &cb))
                        .child(envelope_module(panel, &cb))
                        .child(filter_module(panel, &cb))
                        .child(trigger_module(panel, &cb))
                        .child(output_module(panel, &cb)),
                ),
        )
        .into_any_element()
}

fn caption(text: &'static str) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(text)
        .into_any_element()
}

fn top_bar(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let p = &panel.params;
    let loaded = p
        .pads
        .iter()
        .filter(|pad| pad.sample_name.is_some())
        .count();
    let (gain, tune) = (p.master_gain_db, p.master_tune);
    let on_gain = cb.on_set_master.clone();
    let on_tune = cb.on_set_master.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::SECTION))
        .py(px(space::SNUG))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::HAIR))
                .child(
                    div()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child("64-pad one-shot kit"),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(format!("{loaded} / {PADS} pads loaded")),
                ),
        )
        .child(view_tabs(panel, cb))
        .child(bank_tabs(panel, cb))
        .child(div().flex_1())
        .child(caption("MASTER"))
        .child(knob_cell(
            "Gain",
            format_db(gain),
            knob_with_default(
                "drum-master-gain",
                gain,
                -60.0,
                12.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                0.0,
                move |value, w, cx| on_gain(&(((*value) * 10.0).round() / 10.0, tune), w, cx),
            ),
        ))
        .child(knob_cell(
            "Tune",
            format_semis(tune),
            knob_bipolar(
                "drum-master-tune",
                tune,
                -24.0,
                24.0,
                KNOB_SIZE,
                Colors::accent_primary(),
                None,
                0.0,
                move |value, w, cx| on_tune(&(gain, value.round()), w, cx),
            ),
        ))
        .into_any_element()
}

fn view_tabs(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let mut track = fb_segmented_track();
    for (index, (view, label)) in [(DrumView::Pads, "Pads"), (DrumView::Mixer, "Mixer")]
        .into_iter()
        .enumerate()
    {
        let on_view = cb.on_view.clone();
        track = track.child(fb_segment(
            ("drum-view", index),
            label,
            panel.view == view,
            if index == 0 {
                FbSegment::First
            } else {
                FbSegment::Last
            },
            move |_, w, cx| on_view(&view, w, cx),
        ));
    }
    track.w(px(150.0)).into_any_element()
}

/// The four banks, each with how many of its pads hold a sample.
fn bank_tabs(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let mut track = fb_segmented_track();
    for bank in 0..BANKS {
        let loaded = panel.params.pads[bank * BANK_PADS..(bank + 1) * BANK_PADS]
            .iter()
            .filter(|pad| pad.sample_name.is_some())
            .count();
        let on_bank = cb.on_bank.clone();
        let label = if loaded > 0 {
            format!("{} · {loaded}", bank_letter(bank))
        } else {
            bank_letter(bank).to_string()
        };
        track = track.child(fb_segment(
            ("drum-bank", bank),
            label,
            panel.bank == bank,
            match bank {
                0 => FbSegment::First,
                b if b + 1 == BANKS => FbSegment::Last,
                _ => FbSegment::Middle,
            },
            move |_, w, cx| on_bank(&bank, w, cx),
        ));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(caption("BANK"))
        .child(track.w(px(220.0)))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(format!(
                    "Pads {}–{}",
                    pad_number(panel.bank * BANK_PADS),
                    pad_number((panel.bank + 1) * BANK_PADS - 1)
                )),
        )
        .into_any_element()
}

/// Shown along the bottom while a file is dragged over the window.
fn drop_hint() -> AnyElement {
    div()
        .absolute()
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .px(px(space::SECTION))
        .py(px(space::SNUG))
        .opacity(0.0)
        .group_drag_over::<ExternalPaths>(SAMPLE_DROP_GROUP, |style| style.opacity(1.0))
        .bg(Colors::with_alpha(Colors::accent_primary(), 0.16))
        .border_t(px(1.0))
        .border_color(Colors::accent_primary())
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_primary())
        .child(
            "Drop on a pad to load it there — several files fill the pads after it · on Samples \
             to add them to the folder · anywhere else loads the selected pad",
        )
        .into_any_element()
}

// ── Pads ───────────────────────────────────────────────────────────────────

fn pad_grid(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let mut grid = div().flex().flex_col().gap(px(space::SNUG));
    let first = panel.bank * BANK_PADS;
    for row in BANK_GRID.chunks(4) {
        let mut line = div().flex().flex_row().w_full().gap(px(space::SNUG));
        for &offset in row {
            line = line.child(pad_cell(panel, first + offset, cb));
        }
        grid = grid.child(line);
    }
    grid.into_any_element()
}

fn tag(text: String, color: gpui::Rgba) -> AnyElement {
    div()
        .px(px(3.0))
        .rounded(px(2.0))
        .bg(Colors::with_alpha(color, 0.18))
        .text_size(px(8.0))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn pad_cell(panel: &DrumSamplerPanelState, index: usize, cb: &DrumSamplerCallbacks) -> AnyElement {
    let pad = &panel.params.pads[index];
    let empty = pad.sample_name.is_none();
    let selected = panel.selected == index;
    let loading = panel.loading[index];
    let failed = panel.errors[index].is_some();
    let meter = level_unit(panel.levels[index]);
    let accent = Colors::accent_primary();
    let pressed = panel.pressed == Some(index);
    let rest = if pressed {
        Colors::surface_pressed()
    } else if selected {
        Colors::surface_card_selected()
    } else {
        Colors::surface_card()
    };
    // The pad lights while it sounds, with how loud.
    let bg = if meter > 0.02 {
        Colors::composite(rest, Colors::with_alpha(accent, 0.12 + 0.3 * meter))
    } else {
        rest
    };
    let border = if failed {
        Colors::status_error()
    } else if selected {
        accent
    } else {
        Colors::border_subtle()
    };
    let press = cb.on_pad_press.clone();
    let drop = cb.on_drop.clone();
    let context = cb.on_context.clone();

    let mut tags = div().flex().flex_row().gap(px(2.0));
    if pad.choke_group > 0 {
        // Pads in the selected pad's group are marked in the accent: the
        // ones it cuts, and that cut it.
        let linked = panel.pad().choke_group == pad.choke_group;
        tags = tags.child(tag(
            format!("C{}", pad.choke_group),
            if linked {
                accent
            } else {
                Colors::text_secondary()
            },
        ));
    }
    if pad.filter_mode != FilterMode::Off {
        tags = tags.child(tag("F".into(), Colors::text_secondary()));
    }
    if pad.output > 0 {
        tags = tags.child(tag(
            format!("→{}", pad.output + 1),
            Colors::text_secondary(),
        ));
    }
    if pad.muted {
        tags = tags.child(tag("M".into(), Colors::status_error()));
    }
    if pad.solo {
        tags = tags.child(tag("S".into(), Colors::status_warning()));
    }
    let name = if loading {
        "Loading…".to_string()
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
        .h(px(PAD_H))
        .p(px(space::SNUG))
        .gap(px(3.0))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .cursor(gpui::CursorStyle::PointingHand)
        .when(!selected && meter <= 0.02, |cell| {
            cell.hover(|style| style.bg(Colors::surface_card_hover()))
        })
        .drag_over::<ExternalPaths>(move |style, _, _, _| {
            style
                .border_color(accent)
                .bg(Colors::with_alpha(accent, 0.2))
        })
        .on_drop::<ExternalPaths>(move |paths, w, cx| {
            drop(&(Some(index), paths.paths().to_vec()), w, cx)
        })
        .on_mouse_down(gpui::MouseButton::Right, move |event, w, cx| {
            context(
                &(
                    DrumMenuTarget::Pad(index),
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                ),
                w,
                cx,
            )
        })
        .on_mouse_down(gpui::MouseButton::Left, move |event, w, cx| {
            press(
                &PadPress {
                    pad: index,
                    y: f32::from(event.position.y),
                    shift: event.modifiers.shift,
                },
                w,
                cx,
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(if selected {
                            accent
                        } else {
                            Colors::text_primary()
                        })
                        .child(pad_number(index)),
                )
                .child(tags)
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(note_label(pad.note)),
                ),
        )
        .child(mini_wave(peaks, region, empty))
        .child(
            div()
                .truncate()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(if empty {
                    Colors::text_faint()
                } else {
                    Colors::text_secondary()
                })
                .child(name),
        )
        .child(
            div()
                .w_full()
                .h(px(3.0))
                .rounded(px(1.5))
                .bg(Colors::surface_muted())
                .child(
                    div()
                        .h_full()
                        .w(relative(meter))
                        .rounded(px(1.5))
                        .bg(accent),
                ),
        )
        .when(selected && !failed, |cell| {
            cell.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(radius::CONTROL))
                    .border(px(1.0))
                    .border_color(accent),
            )
        })
        .when(panel.hits[index] > 0, |cell| {
            cell.child(hit_flash(index, panel.hits[index]))
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
                    .rounded(px(radius::CONTROL))
                    .bg(Colors::with_alpha(Colors::surface_window(), 0.86))
                    .child(caption(label))
                    .child(
                        div()
                            .text_size(px(typography::UI_MD))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(value),
                    ),
            )
        })
        .into_any_element()
}

/// A hit's flash over its pad: bright at once, fading out. Each hit has its
/// own id, so a retrigger starts it over; GPUI's animation element drives
/// the frames, and only while it runs.
fn hit_flash(index: usize, hit: u64) -> AnyElement {
    let accent = Colors::accent_primary();
    div()
        .absolute()
        .inset_0()
        .rounded(px(radius::CONTROL))
        .border(px(2.0))
        .with_animation(
            ("drum-hit", index * 1_000_000 + (hit % 1_000_000) as usize),
            Animation::new(HIT_FLASH).with_easing(|t| 1.0 - (1.0 - t).powi(2)),
            move |flash, delta| {
                let left = 1.0 - delta;
                flash
                    .bg(Colors::with_alpha(accent, 0.38 * left))
                    .border_color(Colors::with_alpha(accent, left))
            },
        )
        .into_any_element()
}

/// A pad's sample as a small mirrored silhouette, the played region bright
/// and the trimmed ends dim.
fn mini_wave(peaks: Option<Arc<Vec<u8>>>, (lo, hi): (f32, f32), empty: bool) -> AnyElement {
    let shape = if empty {
        Colors::text_faint()
    } else {
        Colors::accent_primary()
    };
    let shape = Colors::with_alpha(shape, 0.7);
    let trim = Colors::with_alpha(Colors::surface_card(), 0.7);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let mid = y0 + h * 0.5;
            let Some(peaks) = peaks.as_ref().filter(|peaks| peaks.len() >= 2) else {
                window.paint_quad(fill(rect(x0, mid, w, 1.0), shape));
                return;
            };
            let last = (peaks.len() - 1) as f32;
            let at = |i: usize| x0 + i as f32 / last * w;
            let mut path = PathBuilder::fill();
            path.move_to(point(px(x0), px(mid)));
            for (i, peak) in peaks.iter().enumerate() {
                path.line_to(point(px(at(i)), px(mid - *peak as f32 / 255.0 * h * 0.5)));
            }
            for (i, peak) in peaks.iter().enumerate().rev() {
                path.line_to(point(px(at(i)), px(mid + *peak as f32 / 255.0 * h * 0.5)));
            }
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, shape);
            }
            window.paint_quad(fill(rect(x0, y0, lo * w, h), trim));
            window.paint_quad(fill(rect(x0 + hi * w, y0, (1.0 - hi) * w, h), trim));
        },
    )
    .w_full()
    .h(px(MINI_WAVE_H))
    .into_any_element()
}

// ── Samples folder ─────────────────────────────────────────────────────────

fn library(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    search_field: AnyElement,
    query: &str,
) -> AnyElement {
    let accent = Colors::accent_primary();
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
        .overflow_y_scroll();
    match &panel.files {
        None => list = list.child(library_note("Reading the Samples folder…".into())),
        Some(files) => {
            let shown: Vec<&BuiltinFileEntry> = files
                .iter()
                .filter(|file| needle.is_empty() || file.file_name.to_lowercase().contains(&needle))
                .collect();
            if shown.is_empty() {
                list = list.child(library_note(if files.is_empty() {
                    "The Samples folder is empty — drop audio files here, or use Browse…".into()
                } else {
                    format!("No sample matches “{}”.", query.trim())
                }));
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
                        .items_center()
                        .gap(px(space::SNUG))
                        .px(px(space::SNUG))
                        .py(px(3.0))
                        .rounded(px(radius::CONTROL_SM))
                        .when(is_current, |row| row.bg(Colors::with_alpha(accent, 0.14)))
                        .on_mouse_down(gpui::MouseButton::Right, move |event, w, cx| {
                            context(
                                &(
                                    DrumMenuTarget::File(menu_name.clone()),
                                    f32::from(event.position.x),
                                    f32::from(event.position.y),
                                ),
                                w,
                                cx,
                            )
                        })
                        .when(panel.connected, |row| {
                            row.cursor(gpui::CursorStyle::PointingHand)
                                .hover(|style| style.bg(Colors::surface_hover()))
                                .on_click(move |_, w, cx| load(&name, w, cx))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(typography::UI_XS))
                                .text_color(if is_current {
                                    accent
                                } else {
                                    Colors::text_secondary()
                                })
                                .child(file.file_name.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(typography::DENSE_CAPTION))
                                .text_color(Colors::text_faint())
                                .child(format!("{} KB", (file.size_bytes / 1_024).max(1))),
                        ),
                );
            }
        }
    }
    div()
        .id("drum-library")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(140.0))
        .gap(px(space::SNUG))
        .p(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .drag_over::<ExternalPaths>(move |style, _, _, _| {
            style
                .border_color(accent)
                .bg(Colors::with_alpha(accent, 0.12))
        })
        .on_drop::<ExternalPaths>(move |paths, w, cx| drop(&(None, paths.paths().to_vec()), w, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(caption("SAMPLES"))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(format!("→ Pad {}", pad_number(panel.selected))),
                )
                .child(fb_button(
                    "drum-library-refresh",
                    "Refresh",
                    FbButtonKind::Ghost,
                    true,
                    move |_, w, cx| refresh(w, cx),
                ))
                .child(fb_button(
                    "drum-library-browse",
                    "Browse…",
                    FbButtonKind::Primary,
                    panel.connected,
                    move |_, w, cx| browse(w, cx),
                )),
        )
        .child(search_field)
        .child(list)
        .into_any_element()
}

fn library_note(text: String) -> AnyElement {
    div()
        .py(px(space::BASE))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::text_faint())
        .child(text)
        .into_any_element()
}

// ── Selected pad ───────────────────────────────────────────────────────────

fn inspector_head(panel: &DrumSamplerPanelState) -> AnyElement {
    let index = panel.selected;
    let pad = panel.pad();
    let name = if panel.loading[index] {
        "Loading…".to_string()
    } else {
        pad.sample_name
            .clone()
            .unwrap_or_else(|| "No sample".into())
    };
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(space::BASE))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::accent_primary())
                .child(format!(
                    "PAD {} · {}",
                    pad_number(index),
                    note_label(pad.note)
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_MD))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(name),
        )
        .when_some(panel.errors[index].clone(), |head, error| {
            head.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::status_error())
                    .child(error),
            )
        })
        .into_any_element()
}

/// The selected pad's sample, the region it plays, and the envelope over it.
fn waveform_editor(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    bounds_out: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> AnyElement {
    let pad = panel.pad().clone();
    let waveform = panel.waveforms[panel.selected].clone();
    let colors = DisplayColors::resolve();
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

    let mut display = div()
        .id("drum-wave")
        .relative()
        .flex_shrink_0()
        .h(px(WAVE_H))
        .w_full()
        .rounded(px(radius::SURFACE))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .when(has_sample, |display| {
            display
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(gpui::MouseButton::Left, move |event, w, cx| {
                    press(&(f32::from(event.position.x), event.click_count), w, cx)
                })
                .on_mouse_down(gpui::MouseButton::Right, move |event, w, cx| {
                    context(
                        &(
                            DrumMenuTarget::Waveform,
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                        ),
                        w,
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
                    window.paint_quad(fill(rect(x0, y0, start - x0, wave_h), colors.skipped));
                    window.paint_quad(fill(rect(end, y0, x0 + w - end, wave_h), colors.skipped));
                    // The envelope, in time across the region, from the edge
                    // playback starts at.
                    if plays > 0.0 {
                        let steps = ((end - start).max(2.0) as usize).min(512);
                        let mut line = PathBuilder::stroke(px(1.5));
                        for i in 0..=steps {
                            let along = i as f32 / steps as f32;
                            let level = envelope_at(&envelope_pad, along * plays);
                            let fraction = if envelope_pad.reverse {
                                hi - along * (hi - lo)
                            } else {
                                lo + along * (hi - lo)
                            };
                            let at =
                                point(px(frame.at(fraction)), px(frame.mid - level * frame.half));
                            if i == 0 {
                                line.move_to(at);
                            } else {
                                line.line_to(at);
                            }
                        }
                        if let Ok(path) = line.build() {
                            window.paint_path(path, colors.loop_marker);
                        }
                    }
                    for x in [start, end - 1.5] {
                        let x = x.clamp(x0, x0 + w - 1.5);
                        window.paint_quad(fill(rect(x, y0, 1.5, wave_h), colors.marker));
                    }
                    window.paint_quad(
                        fill(rect(start, y0, FLAG_W, FLAG_H), colors.marker).corner_radii(px(2.0)),
                    );
                    window.paint_quad(
                        fill(rect(end - FLAG_W, y0, FLAG_W, FLAG_H), colors.marker)
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
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(Colors::text_inverse())
                .child(letter);
            display = display.child(if inside { flag.ml(px(-FLAG_W)) } else { flag });
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
            pad.sample_name.is_some(),
            "or click a file in Samples to load it onto this pad",
        ));
    }

    let readout = |label: &'static str, value: String| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(space::TIGHT))
            .child(caption(label))
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_secondary())
                    .child(value),
            )
    };
    let meta = waveform
        .as_ref()
        .map(|w| {
            format!(
                "{} · {} · {:.1} kHz",
                format_seconds(seconds),
                if w.channels == 1 { "mono" } else { "stereo" },
                w.sample_rate as f32 / 1_000.0
            )
        })
        .unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .child(display)
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(space::LOOSE))
                .child(readout("START", format!("{:.1}%", lo * 100.0)))
                .child(readout("END", format!("{:.1}%", hi * 100.0)))
                .child(readout(
                    "PLAYS",
                    if plays > 0.0 {
                        format_ms(plays * 1_000.0)
                    } else {
                        "—".into()
                    },
                ))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_faint())
                        .child(meta),
                ),
        )
        .into_any_element()
}

// ── Modules ────────────────────────────────────────────────────────────────

/// Sends the selected pad edited by `apply`.
fn edit(
    panel: &DrumSamplerPanelState,
    cb: &DrumSamplerCallbacks,
    apply: impl Fn(&mut Pad, f32) + 'static,
) -> impl Fn(&f32, &mut Window, &mut App) + 'static {
    let index = panel.selected;
    let pad = panel.pad().clone();
    let set = cb.on_set_pad.clone();
    move |value, w, cx| {
        let mut next = pad.clone();
        apply(&mut next, *value);
        set(&(index, next), w, cx)
    }
}

fn knob_id(name: &'static str, pad: usize) -> String {
    format!("drum-{name}-{pad}")
}

fn voice_module(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let pad = panel.pad();
    let i = panel.selected;
    let accent = Colors::accent_primary();
    module(
        "VOICE",
        None,
        controls_row()
            .child(knob_cell(
                "Tune",
                format_semis(pad.tune_semitones),
                knob_bipolar(
                    knob_id("tune", i),
                    pad.tune_semitones,
                    -24.0,
                    24.0,
                    KNOB_SIZE,
                    accent,
                    None,
                    0.0,
                    edit(panel, cb, |pad, v| pad.tune_semitones = v.round()),
                ),
            ))
            .child(knob_cell(
                "Gain",
                format_db(pad.gain_db),
                knob_with_default(
                    knob_id("gain", i),
                    pad.gain_db,
                    -60.0,
                    12.0,
                    KNOB_SIZE,
                    accent,
                    0.0,
                    edit(panel, cb, |pad, v| pad.gain_db = (v * 10.0).round() / 10.0),
                ),
            ))
            .child(knob_cell(
                "Pan",
                format_pan_label(pad.pan),
                knob_bipolar(
                    knob_id("pan", i),
                    pad.pan,
                    -1.0,
                    1.0,
                    KNOB_SIZE,
                    accent,
                    None,
                    0.0,
                    edit(panel, cb, |pad, v| pad.pan = v),
                ),
            ))
            .child(knob_cell(
                "Velocity",
                format!("{:.0}%", pad.velocity_sensitivity),
                knob_with_default(
                    knob_id("velocity", i),
                    pad.velocity_sensitivity,
                    0.0,
                    100.0,
                    KNOB_SIZE,
                    accent,
                    100.0,
                    edit(panel, cb, |pad, v| pad.velocity_sensitivity = v.round()),
                ),
            )),
    )
}

fn envelope_module(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let pad = panel.pad();
    let i = panel.selected;
    let accent = Colors::accent_primary();
    let decays = pad.decay_ms > 0.0;
    module(
        if decays {
            "ENVELOPE · ATTACK HOLD DECAY"
        } else {
            "ENVELOPE · PLAYS TO THE END"
        },
        None,
        controls_row()
            .child(knob_cell(
                "Attack",
                format_ms(pad.attack_ms),
                knob_with_default(
                    knob_id("attack", i),
                    time_position(pad.attack_ms, ATTACK_MAX_MS),
                    0.0,
                    1.0,
                    KNOB_SIZE,
                    accent,
                    time_position(1.0, ATTACK_MAX_MS),
                    edit(panel, cb, |pad, v| {
                        pad.attack_ms = time_from_position(v, ATTACK_MAX_MS)
                    }),
                ),
            ))
            .child(
                div()
                    .opacity(if decays { 1.0 } else { 0.48 })
                    .child(knob_cell(
                        "Hold",
                        format_ms(pad.hold_ms),
                        knob_with_default(
                            knob_id("hold", i),
                            time_position(pad.hold_ms, MAX_HOLD_MS),
                            0.0,
                            1.0,
                            KNOB_SIZE,
                            accent,
                            0.0,
                            edit(panel, cb, |pad, v| {
                                pad.hold_ms = time_from_position(v, MAX_HOLD_MS)
                            }),
                        ),
                    )),
            )
            .child(knob_cell(
                "Decay",
                if decays {
                    format_ms(pad.decay_ms)
                } else {
                    "Off".into()
                },
                knob_with_default(
                    knob_id("decay", i),
                    time_position(pad.decay_ms, MAX_DECAY_MS),
                    0.0,
                    1.0,
                    KNOB_SIZE,
                    accent,
                    0.0,
                    edit(panel, cb, |pad, v| {
                        pad.decay_ms = time_from_position(v, MAX_DECAY_MS)
                    }),
                ),
            )),
    )
}

fn filter_graph(pad: Pad) -> AnyElement {
    let active = pad.filter_mode != FilterMode::Off;
    let colors = GraphColors::resolve(active);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            paint_graph_frame(window, bounds, &colors);
            let x0 = f32::from(bounds.origin.x);
            let y0 = f32::from(bounds.origin.y);
            let w = f32::from(bounds.size.width).max(1.0);
            let h = f32::from(bounds.size.height).max(1.0);
            let y_for = |db: f32| y0 + ((18.0 - db.clamp(-36.0, 18.0)) / 54.0) * h;
            let columns = (w as usize).max(2);
            let curve: Vec<(f32, f32)> = (0..columns)
                .map(|i| {
                    let t = i as f32 / (columns - 1) as f32;
                    let hz = 20.0 * 1_000.0_f32.powf(t);
                    (x0 + t * w, y_for(filter_db(&pad, hz)))
                })
                .collect();
            paint_curve(window, &curve, y0 + h, &colors);
            if active {
                let x = x0 + cutoff_position(pad.cutoff_hz) * w;
                window.paint_quad(fill(rect(x, y0, 1.0, h), colors.guide));
            }
        },
    )
    .w_full()
    .h(px(GRAPH_H))
    .into_any_element()
}

fn filter_module(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let pad = panel.pad();
    let i = panel.selected;
    let accent = Colors::accent_primary();
    let mut modes = fb_segmented_track();
    let last = FilterMode::ALL.len() - 1;
    for (index, mode) in FilterMode::ALL.into_iter().enumerate() {
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        next.filter_mode = mode;
        modes = modes.child(fb_segment(
            ("drum-filter", index),
            match mode {
                FilterMode::Off => "Off",
                FilterMode::LowPass => "LP",
                FilterMode::HighPass => "HP",
                FilterMode::BandPass => "BP",
            },
            pad.filter_mode == mode,
            match index {
                0 => FbSegment::First,
                i if i == last => FbSegment::Last,
                _ => FbSegment::Middle,
            },
            move |_, w, cx| set(&(i, next.clone()), w, cx),
        ));
    }
    let active = pad.filter_mode != FilterMode::Off;
    module(
        "FILTER",
        Some(filter_graph(pad.clone())),
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(modes.w_full())
            .child(
                controls_row()
                    .justify_around()
                    .opacity(if active { 1.0 } else { 0.48 })
                    .child(knob_cell(
                        "Cutoff",
                        format_hz(pad.cutoff_hz),
                        knob_with_default(
                            knob_id("cutoff", i),
                            cutoff_position(pad.cutoff_hz),
                            0.0,
                            1.0,
                            KNOB_SIZE,
                            accent,
                            cutoff_position(MAX_CUTOFF_HZ),
                            edit(panel, cb, |pad, v| {
                                pad.cutoff_hz = cutoff_from_position(v).round()
                            }),
                        ),
                    ))
                    .child(knob_cell(
                        "Resonance",
                        format!("{:.0}%", pad.resonance),
                        knob_with_default(
                            knob_id("resonance", i),
                            pad.resonance,
                            0.0,
                            100.0,
                            KNOB_SIZE,
                            accent,
                            0.0,
                            edit(panel, cb, |pad, v| pad.resonance = v.round()),
                        ),
                    )),
            ),
    )
}

/// A `− value +` stepper over one pad field.
fn pad_stepper(
    id: (&'static str, usize),
    caption_text: &'static str,
    readout: String,
    (index, dec, inc): (usize, Pad, Pad),
    cb: &DrumSamplerCallbacks,
) -> AnyElement {
    let on_dec = cb.on_set_pad.clone();
    let on_inc = cb.on_set_pad.clone();
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::HAIR))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::HAIR))
                .child(fb_stepper_button(
                    (id.0, id.1 * 2),
                    "−",
                    move |_, w, cx| on_dec(&(index, dec.clone()), w, cx),
                ))
                .child(
                    div()
                        .w(px(58.0))
                        .flex()
                        .justify_center()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(readout),
                )
                .child(fb_stepper_button(
                    (id.0, id.1 * 2 + 1),
                    "+",
                    move |_, w, cx| on_inc(&(index, inc.clone()), w, cx),
                )),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption_text),
        )
        .into_any_element()
}

/// Where the selected pad plays: Main, on the instrument's own channel, or
/// one of fifteen more outputs, each heard on a mixer channel of its own that
/// Studio adds as soon as a pad uses it (and drops when none does) — nothing
/// to set up first. Two shortcuts send the whole kit at once.
fn output_module(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let i = panel.selected;
    let pad = panel.pad().clone();
    let accent = Colors::accent_primary();
    let mut chips = div().flex().flex_row().flex_wrap().gap(px(3.0));
    for output in 0..OUTPUTS as u8 {
        let on = pad.output == output;
        let used = panel.params.pads.iter().any(|p| p.output == output);
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        next.output = output;
        chips = chips.child(
            div()
                .id(("drum-output", output as usize))
                .flex()
                .items_center()
                .justify_center()
                .min_w(px(26.0))
                .h(px(24.0))
                .px(px(4.0))
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(if on { accent } else { Colors::border_subtle() })
                .bg(if on {
                    Colors::with_alpha(accent, 0.22)
                } else if used {
                    Colors::surface_card()
                } else {
                    Colors::surface_input()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(Colors::surface_hover()))
                .on_click(move |_, w, cx| set(&(i, next.clone()), w, cx))
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if on {
                            accent
                        } else if used {
                            Colors::text_primary()
                        } else {
                            Colors::text_muted()
                        })
                        .child((output + 1).to_string()),
                ),
        );
    }
    let mut extra: Vec<u8> = panel
        .params
        .pads
        .iter()
        .map(|p| p.output)
        .filter(|output| *output > 0)
        .collect();
    extra.sort_unstable();
    extra.dedup();
    let summary = if pad.output == 0 {
        "Out 1 — plays on this instrument's own channel".to_string()
    } else {
        format!(
            "{} — plays on its own mixer channel, “Out Ch {}”, added for you",
            output_label(pad.output),
            pad.output + 1
        )
    };
    let channels = if extra.is_empty() {
        "Every pad is on Out 1".to_string()
    } else {
        let names: Vec<String> = extra.iter().map(|o| output_label(*o)).collect();
        format!("Own channels: {}", names.join(", "))
    };
    let spread = cb.on_set_outputs.clone();
    let gather = cb.on_set_outputs.clone();
    let outputs = pad_outputs(&panel.params);
    let bank = panel.bank;
    module(
        "OUTPUT",
        None,
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(chips)
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .text_color(Colors::text_muted())
                    .child(summary),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(space::SNUG))
                    .child(fb_button(
                        "drum-outputs-spread",
                        "One per pad (this bank)",
                        FbButtonKind::Default,
                        true,
                        move |_, w, cx| spread(&one_output_per_pad(outputs, bank), w, cx),
                    ))
                    .child(fb_button(
                        "drum-outputs-gather",
                        "All to Out 1",
                        FbButtonKind::Ghost,
                        !extra.is_empty(),
                        move |_, w, cx| gather(&[0; PADS], w, cx),
                    ))
                    .child(
                        div()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(channels),
                    ),
            ),
    )
}

// ── Mixer ──────────────────────────────────────────────────────────────────

/// A strip per pad of the bank: what each pad plays, where it goes, and how
/// loud — the kit's own mixer, before the outputs reach Studio's.
fn mixer_view(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let first = panel.bank * BANK_PADS;
    let mut strips = div().flex().flex_row().w_full().gap(px(space::TIGHT));
    for index in first..first + BANK_PADS {
        strips = strips.child(mixer_strip(panel, index, cb));
    }
    div()
        .id("drum-mixer")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .gap(px(space::BASE))
        .px(px(space::SECTION))
        .pt(px(space::LOOSE))
        .pb(px(space::SECTION))
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(space::BASE))
                .child(caption("MIXER"))
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(
                            "Drag a fader for the pad's level — double-click it for 0 dB · Out \
                             picks the mixer channel the pad plays on",
                        ),
                ),
        )
        .child(strips)
        .into_any_element()
}

fn strip_toggle(
    id: (&'static str, usize),
    label: &'static str,
    on: bool,
    tone: gpui::Rgba,
    set: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_1()
        .h(px(20.0))
        .rounded(px(radius::CONTROL_SM))
        .border(px(1.0))
        .border_color(if on { tone } else { Colors::border_subtle() })
        .bg(if on {
            Colors::with_alpha(tone, 0.28)
        } else {
            Colors::surface_input()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|style| style.bg(Colors::surface_hover()))
        .on_click(move |_, w, cx| set(w, cx))
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(if on { tone } else { Colors::text_muted() })
        .child(label)
        .into_any_element()
}

fn mixer_strip(
    panel: &DrumSamplerPanelState,
    index: usize,
    cb: &DrumSamplerCallbacks,
) -> AnyElement {
    let pad = panel.params.pads[index].clone();
    let selected = panel.selected == index;
    let empty = pad.sample_name.is_none();
    let meter = level_unit(panel.levels[index]);
    let accent = Colors::accent_primary();
    let position = fader_position(pad.gain_db);
    let select = cb.on_select.clone();
    let press = cb.on_fader_press.clone();
    let context = cb.on_context.clone();
    let set = |apply: fn(&mut Pad)| {
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        apply(&mut next);
        move |w: &mut Window, cx: &mut App| set(&(index, next.clone()), w, cx)
    };
    let pan_set = cb.on_set_pad.clone();
    let pan_pad = pad.clone();
    let out_dec = {
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        next.output = pad.output.saturating_sub(1);
        move |_: &gpui::ClickEvent, w: &mut Window, cx: &mut App| set(&(index, next.clone()), w, cx)
    };
    let out_inc = {
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        next.output = (pad.output + 1).min(OUTPUTS as u8 - 1);
        move |_: &gpui::ClickEvent, w: &mut Window, cx: &mut App| set(&(index, next.clone()), w, cx)
    };

    let fader = div()
        .id(("drum-fader", index))
        .relative()
        .w(px(20.0))
        .h(px(FADER_H))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .on_mouse_down(gpui::MouseButton::Left, move |event, w, cx| {
            press(
                &FaderPress {
                    pad: index,
                    y: f32::from(event.position.y),
                    clicks: event.click_count,
                },
                w,
                cx,
            )
        })
        .child(
            div()
                .absolute()
                .left(px(8.0))
                .w(px(4.0))
                .top(px(0.0))
                .bottom(px(0.0))
                .rounded(px(2.0))
                .bg(Colors::surface_input()),
        )
        .child(
            div()
                .absolute()
                .left(px(8.0))
                .w(px(4.0))
                .bottom(px(0.0))
                .h(px(FADER_H * position))
                .rounded(px(2.0))
                .bg(Colors::with_alpha(accent, 0.55)),
        )
        // 0 dB, marked.
        .child(
            div()
                .absolute()
                .left(px(2.0))
                .w(px(16.0))
                .h(px(1.0))
                .bottom(px(FADER_H * fader_position(0.0)))
                .bg(Colors::text_faint()),
        )
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .w(px(20.0))
                .h(px(FADER_THUMB_H))
                .bottom(px((FADER_H - FADER_THUMB_H) * position))
                .rounded(px(2.0))
                .border(px(1.0))
                .border_color(Colors::border_strong())
                .bg(if selected {
                    accent
                } else {
                    Colors::text_secondary()
                }),
        );
    let level = div()
        .relative()
        .w(px(6.0))
        .h(px(FADER_H))
        .rounded(px(2.0))
        .bg(Colors::surface_muted())
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .h(px(FADER_H * meter))
                .rounded(px(2.0))
                .bg(accent),
        );

    div()
        .id(("drum-strip", index))
        .relative()
        .flex()
        .flex_col()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .gap(px(space::SNUG))
        .p(px(space::TIGHT))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(if selected {
            accent
        } else {
            Colors::border_subtle()
        })
        .bg(if meter > 0.02 {
            Colors::composite(
                Colors::surface_panel(),
                Colors::with_alpha(accent, 0.08 + 0.2 * meter),
            )
        } else {
            Colors::surface_panel()
        })
        .opacity(if empty { 0.6 } else { 1.0 })
        .on_mouse_down(gpui::MouseButton::Right, move |event, w, cx| {
            context(
                &(
                    DrumMenuTarget::Pad(index),
                    f32::from(event.position.x),
                    f32::from(event.position.y),
                ),
                w,
                cx,
            )
        })
        .child(
            div()
                .id(("drum-strip-head", index))
                .flex()
                .flex_col()
                .items_center()
                .w_full()
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, w, cx| select(&index, w, cx))
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(if selected {
                            accent
                        } else {
                            Colors::text_primary()
                        })
                        .child(pad_number(index)),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_center()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(Colors::text_muted())
                        .child(pad.sample_name.clone().unwrap_or_else(|| "Empty".into())),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0))
                .child(caption("OUT"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(fb_stepper_button(
                            ("drum-strip-out", index * 2),
                            "−",
                            out_dec,
                        ))
                        .child(
                            div()
                                .w(px(18.0))
                                .flex()
                                .justify_center()
                                .text_size(px(typography::UI_XS))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(if pad.output > 0 {
                                    accent
                                } else {
                                    Colors::text_primary()
                                })
                                .child((pad.output + 1).to_string()),
                        )
                        .child(fb_stepper_button(
                            ("drum-strip-out", index * 2 + 1),
                            "+",
                            out_inc,
                        )),
                ),
        )
        .child(knob_cell(
            "Pan",
            format_pan_label(pad.pan),
            knob_bipolar(
                knob_id("strip-pan", index),
                pad.pan,
                -1.0,
                1.0,
                24.0,
                accent,
                None,
                0.0,
                move |value, w, cx| {
                    let mut next = pan_pad.clone();
                    next.pan = *value;
                    pan_set(&(index, next), w, cx)
                },
            ),
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(space::TIGHT))
                .child(fader)
                .child(level),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(format!("{:+.1}", pad.gain_db)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .gap(px(2.0))
                .child(strip_toggle(
                    ("drum-strip-mute", index),
                    "M",
                    pad.muted,
                    Colors::status_error(),
                    set(|p| p.muted = !p.muted),
                ))
                .child(strip_toggle(
                    ("drum-strip-solo", index),
                    "S",
                    pad.solo,
                    Colors::status_warning(),
                    set(|p| p.solo = !p.solo),
                )),
        )
        .when(panel.hits[index] > 0, |strip| {
            strip.child(hit_flash(index, panel.hits[index]))
        })
        .into_any_element()
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

/// The selected pad's choke group, as one row of chips — Off, then 1–8 —
/// each group showing how many pads are in it, and which pads share the
/// selected one's.
fn choke_chips(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let i = panel.selected;
    let pad = panel.pad().clone();
    let accent = Colors::accent_primary();
    let mut chips = div().flex().flex_row().flex_wrap().gap(px(3.0));
    for group in 0..=8_u8 {
        let on = pad.choke_group == group;
        let count = choke_members(&panel.params, group).len();
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        next.choke_group = group;
        chips = chips.child(
            div()
                .id(("drum-choke-group", group as usize))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .min_w(px(if group == 0 { 34.0 } else { 24.0 }))
                .h(px(28.0))
                .px(px(4.0))
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(if on { accent } else { Colors::border_subtle() })
                .bg(if on {
                    Colors::with_alpha(accent, 0.22)
                } else {
                    Colors::surface_input()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|style| style.bg(Colors::surface_hover()))
                .on_click(move |_, w, cx| set(&(i, next.clone()), w, cx))
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if on { accent } else { Colors::text_secondary() })
                        .child(if group == 0 {
                            "Off".to_string()
                        } else {
                            group.to_string()
                        }),
                )
                .when(group > 0 && count > 0, |chip| {
                    chip.child(
                        div()
                            .text_size(px(8.0))
                            .text_color(Colors::text_faint())
                            .child(format!("{count} pad{}", if count == 1 { "" } else { "s" })),
                    )
                }),
        );
    }
    let members = choke_members(&panel.params, pad.choke_group);
    let summary = if pad.choke_group == 0 {
        "Off — this pad never cuts another, and is never cut".to_string()
    } else if members.len() <= 1 {
        format!(
            "Group {} — put another pad in it to cut each other (open and closed hi-hat)",
            pad.choke_group
        )
    } else {
        let names: Vec<String> = members.iter().map(|m| pad_number(*m)).collect();
        format!(
            "Group {} — pads {} cut each other",
            pad.choke_group,
            names.join(", ")
        )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(caption("CHOKE GROUP"))
        .child(chips)
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(summary),
        )
        .into_any_element()
}

fn trigger_module(panel: &DrumSamplerPanelState, cb: &DrumSamplerCallbacks) -> AnyElement {
    let pad = panel.pad().clone();
    let i = panel.selected;
    let with = |apply: &dyn Fn(&mut Pad)| {
        let mut next = pad.clone();
        apply(&mut next);
        next
    };
    let toggle = |id: &'static str, label: &'static str, on: bool, apply: fn(&mut Pad)| {
        let set = cb.on_set_pad.clone();
        let mut next = pad.clone();
        apply(&mut next);
        fb_checkbox((id, i), label, on, true, move |_, w, cx| {
            set(&(i, next.clone()), w, cx)
        })
    };
    module(
        "TRIGGER",
        None,
        div()
            .flex()
            .flex_col()
            .gap(px(space::BASE))
            .child(controls_row().child(pad_stepper(
                ("drum-note", i),
                "Note",
                note_label(pad.note),
                (
                    i,
                    with(&|p| p.note = p.note.saturating_sub(1)),
                    with(&|p| p.note = p.note.saturating_add(1).min(127)),
                ),
                cb,
            )))
            .child(choke_chips(panel, cb))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(space::BASE))
                    .child(toggle("drum-reverse", "Reverse", pad.reverse, |p| {
                        p.reverse = !p.reverse
                    }))
                    .child(toggle("drum-mute", "Mute", pad.muted, |p| {
                        p.muted = !p.muted
                    }))
                    .child(toggle("drum-solo", "Solo", pad.solo, |p| p.solo = !p.solo)),
            ),
    )
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
        assert_eq!(fader_dragged(0.0, -10.0), -4.1);
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
            time_from_position(time_position(400.0, MAX_DECAY_MS), MAX_DECAY_MS),
            400.0
        );
        assert_eq!(time_from_position(1.0, ATTACK_MAX_MS), ATTACK_MAX_MS);
        assert_eq!(time_position(0.0, MAX_HOLD_MS), 0.0);
    }
}
