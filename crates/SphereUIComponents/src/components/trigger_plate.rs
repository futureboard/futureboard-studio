//! Trigger's editor: the detector's level history with the threshold and
//! every hit in the well, In / Out meters and the hit lamp beside it; Sample,
//! Detect and Output on the deck.
//!
//! Everything moving is real. The history is the host's meter frames (the
//! detector rides slot 0's input peak, a hit lights slot 0's output), and the
//! lamp, the hit count, the last hit's level and the sounding voices come from
//! `trigger::telemetry`. The sample is a file in the plug-in's Samples folder,
//! sent to the host with `load_pad_sample` (slot 0) like Quick Sampler's; the
//! built-in sounds are rendered by `trigger::synth` into that folder and load
//! the same way, so they save and reopen like any other sample. The waveform
//! is decoded here, off the UI thread, only to draw it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    canvas, div, px, AnyElement, App, AppContext, ExternalPaths, FontWeight, InteractiveElement,
    IntoElement, MouseButton, MouseMoveEvent, ParentElement, Rgba, SharedString, Styled, Window,
};
use trigger::synth::Sound;
use trigger::{telemetry, Mode, Params, LENGTH_FULL_MS, OFFSET_MAX_MS, OFFSET_MIN_MS};

use crate::components::builtin_plugin_editor::builtin_trigger_params;
use crate::components::builtin_plugin_editor_window::{
    read_dropped_sample, BuiltinDrumSampleLoadRequest, BuiltinEditorHostOps, PluginInstanceKey,
};
use crate::components::builtin_plugin_files::{self as files, BuiltinFileKind};
use crate::components::fx_model::KnobSpec;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate::{
    self, color, color_a, deck, icon, live_slot, numbers, paint_dot, paint_polygon, paint_polyline,
    paint_text, plate_frame, plate_scale, square, well, words, Align, Dial, DialUnit, KnobCell,
    KnobLook, PillLook, GROUND, ICON, OFF, SOFT, WHITE,
};
use crate::components::plugin_kit::{
    bank, open_kit_editor, Cx, KitEditor, KitModel, KitPreset, KitWindow, LiveBounds, LiveSlot,
};
use crate::components::plugin_live::{HistoryPoint, Live};
use crate::components::quick_sampler_window::{list_samples, step_in, SAMPLE_EXTENSIONS};

/// Trigger's accent: hits and what they set off.
pub const ACCENT: u32 = 0xfb923c;

const HELP: u32 = 0xa6a6a6;
const LEGEND: u32 = 0x9a9a9a;
const SCREEN: u32 = 0x0d0d0d;
const SCREEN_EDGE: u32 = 0x242424;
const DIVIDER: u32 = 0x2a2a2a;

/// The display's view box.
const GW: f32 = 820.0;
const GH: f32 = 244.0;
/// History frames across the display: about four seconds at the telemetry
/// rate.
const SHOWN: usize = 120;
/// Columns of waveform overview.
const WAVE_BUCKETS: usize = 300;

const SLOT_GRAPH: usize = 0;
/// In and Out bars, then their readings.
const SLOT_BAR: usize = 1;
const SLOT_VALUE: usize = 3;
const SLOT_LAMP: usize = 5;
const SLOT_LAST: usize = 6;
const SLOT_STATUS: usize = 7;

const KNOB: KnobLook = KnobLook::small(48.0);
const CELL: KnobCell = KnobCell::new(80.0, 11.0, 12.0).plain_label();

fn accent() -> Rgba {
    color(ACCENT)
}

// ── Readouts ───────────────────────────────────────────────────────────────

fn f_hz(v: f32) -> String {
    if v < 1_000.0 {
        format!("{} Hz", v.round())
    } else if v < 10_000.0 {
        format!("{:.1} kHz", v / 1_000.0)
    } else {
        format!("{:.0} kHz", v / 1_000.0)
    }
}

fn sign(v: f32) -> &'static str {
    if v > 0.0 {
        "+"
    } else if v < 0.0 {
        "\u{2212}"
    } else {
        ""
    }
}

fn f_offset(v: f32) -> String {
    let v = (v * 10.0).round() / 10.0;
    if v == 0.0 {
        "0 ms".into()
    } else {
        format!("{}{:.1} ms", sign(v), v.abs())
    }
}

fn f_pitch(v: f32) -> String {
    let v = (v * 10.0).round() / 10.0;
    if v.fract() == 0.0 {
        format!("{}{} st", sign(v), v.abs())
    } else {
        format!("{}{:.1} st", sign(v), v.abs())
    }
}

fn f_length(v: f32) -> String {
    if v >= LENGTH_FULL_MS - 0.5 {
        "Full".into()
    } else {
        DialUnit::Time.format(v)
    }
}

fn f_voices(v: f32) -> String {
    let n = v.round().max(1.0) as u32;
    if n == 1 {
        "1 \u{00B7} cut".into()
    } else {
        n.to_string()
    }
}

/// The detector filters print "Off" at the end of their range that turns
/// them off.
fn f_hpf(v: f32) -> String {
    if v <= 20.5 {
        "Off".into()
    } else {
        f_hz(v)
    }
}

fn f_lpf(v: f32) -> String {
    if v >= 19_950.0 {
        "Off".into()
    } else {
        f_hz(v)
    }
}

/// A deck knob: its param and how its value reads.
#[derive(Clone, Copy)]
struct Knob {
    dial: Dial,
    text: fn(f32) -> String,
}

const fn knob_def(dial: Dial, text: fn(f32) -> String) -> Knob {
    Knob { dial, text }
}

const THRESHOLD: Knob = knob_def(
    Dial::new("thresholdDb", "Threshold", -60.0, 0.0, -20.0, DialUnit::Db),
    plate::fmt_db,
);
const RETRIGGER: Knob = knob_def(
    Dial::new("retriggerMs", "Retrigger", 5.0, 500.0, 60.0, DialUnit::Ms).log(),
    plate::fmt_ms,
);
const DYNAMICS: Knob = knob_def(
    Dial::new("dynamics", "Dynamics", 0.0, 100.0, 75.0, DialUnit::Percent),
    |v| format!("{}%", v.round()),
);
const HPF: Knob = knob_def(
    Dial::new("hpfHz", "High-pass", 20.0, 2_000.0, 20.0, DialUnit::HzRound).log(),
    f_hpf,
);
const LPF: Knob = knob_def(
    Dial::new(
        "lpfHz",
        "Low-pass",
        100.0,
        20_000.0,
        20_000.0,
        DialUnit::HzRound,
    )
    .log(),
    f_lpf,
);
const OFFSET: Knob = knob_def(
    Dial::new(
        "offsetMs",
        "Offset",
        OFFSET_MIN_MS,
        OFFSET_MAX_MS,
        0.0,
        DialUnit::Ms,
    ),
    f_offset,
);
const PITCH: Knob = knob_def(
    Dial::new("pitch", "Pitch", -24.0, 24.0, 0.0, DialUnit::Plain).bipolar(),
    f_pitch,
);
const LENGTH: Knob = knob_def(
    Dial::new(
        "lengthMs",
        "Length",
        10.0,
        LENGTH_FULL_MS,
        LENGTH_FULL_MS,
        DialUnit::Time,
    )
    .log(),
    f_length,
);
const FADE: Knob = knob_def(
    Dial::new("fadeMs", "Fade", 1.0, 500.0, 30.0, DialUnit::Ms).log(),
    plate::fmt_ms,
);
const SAMPLE_LEVEL: Knob = knob_def(
    Dial::new("sampleDb", "Sample", -24.0, 12.0, 0.0, DialUnit::Db),
    plate::fmt_db,
);
const DRY: Knob = knob_def(
    Dial::new("dryDb", "Dry", -48.0, 6.0, 0.0, DialUnit::Db),
    plate::fmt_db,
);
const VOICES: Knob = knob_def(
    Dial::new("voices", "Voices", 1.0, 8.0, 4.0, DialUnit::Plain),
    f_voices,
);

/// The display's level scale: 0 dB near the top, −60 at the foot.
fn db_y(db: f32) -> f32 {
    6.0 + (-db / 60.0).clamp(0.0, 1.0) * (GH - 12.0)
}

// ── The sample ─────────────────────────────────────────────────────────────

/// The loaded sample, as the display draws it.
#[derive(Debug, Clone)]
pub struct SampleView {
    pub name: String,
    pub seconds: f64,
    pub channels: usize,
    pub sample_rate: u32,
    pub peaks: Arc<Vec<(f32, f32)>>,
}

/// Decodes `bytes` (a file named `name`) for the display. Background thread
/// only.
fn describe(name: &str, bytes: &[u8]) -> Result<SampleView, String> {
    let ext = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let buffer = DirectAudio::load_audio_bytes(bytes, ext)?;
    let sample = quicksampler::SampleData::from_interleaved(
        &buffer.samples,
        buffer.channels,
        buffer.sample_rate,
    )
    .ok_or_else(|| "the file holds no audio".to_string())?;
    Ok(SampleView {
        name: name.to_string(),
        seconds: sample.seconds(),
        channels: sample.channels(),
        sample_rate: sample.sample_rate(),
        peaks: Arc::new(sample.peaks(WAVE_BUCKETS)),
    })
}

fn samples_dir() -> PathBuf {
    files::plugin_files_root(trigger::PLUGIN_NAME).join(BuiltinFileKind::Samples.dir_name())
}

/// A built-in sound, written into the Samples folder unless a file of that
/// name is already there, and read back. Background thread only.
fn builtin_sound_file(dir: &Path, sound: Sound) -> Result<(String, Vec<u8>), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("could not create the Samples folder: {error}"))?;
    let path = dir.join(sound.file_name());
    if !path.exists() {
        std::fs::write(&path, sound.wav_bytes())
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok((sound.file_name().to_string(), bytes))
}

// ── The model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TriggerPlate;

/// A Trigger insert's editor window.
pub type TriggerWindow = KitWindow<TriggerPlate>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    Threshold,
}

#[derive(Default)]
pub struct TriggerUi {
    drag: Option<Drag>,
    /// The sample the display shows, once read.
    sample: Option<SampleView>,
    /// The file the display shows, or is reading.
    shown: Option<String>,
    loading: bool,
    /// A load was sent to the host and it has not answered yet: until it
    /// does, the mirror still names the sample before it.
    awaiting: bool,
    status: Option<String>,
    /// Bumped per read, so a slow read for a file since replaced cannot land
    /// over the current one.
    generation: u64,
    /// The Samples folder, by name: what the stepper steps through.
    files: Vec<String>,
    /// Whether the folder has been listed since the editor opened.
    listed: bool,
}

#[derive(Clone)]
pub struct TriggerView {
    params: Params,
}

impl KitModel for TriggerPlate {
    type Params = Params;
    type Ui = TriggerUi;
    type LiveView = TriggerView;

    fn title(self) -> &'static str {
        "Trigger"
    }

    fn subtitle(self, params: &Params) -> String {
        params.mode.label().to_uppercase()
    }

    fn key(self) -> &'static str {
        "trigger-plate"
    }

    fn defaults(self) -> Params {
        trigger::default_params()
    }

    fn from_mirror(self, insert_id: &str) -> Option<Params> {
        builtin_trigger_params(insert_id)
    }

    fn wire_values(self, params: &Params) -> Vec<(u32, f32)> {
        trigger::ipc::ui_values(params)
            .into_iter()
            .filter_map(|(id, value)| trigger::ui_param_index(id).map(|index| (index, value)))
            .collect()
    }

    fn with(self, params: &Params, id: &str, value: f32) -> Params {
        let mut next = params.clone();
        trigger::ipc::apply_ui_param(&mut next, id, value);
        next
    }

    fn value(self, params: &Params, id: &str) -> f32 {
        trigger::ipc::ui_values(params)
            .into_iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0.0, |(_, value)| value)
    }

    fn presets(self) -> Arc<Vec<KitPreset<Params>>> {
        static BANK: std::sync::OnceLock<Arc<Vec<KitPreset<Params>>>> = std::sync::OnceLock::new();
        BANK.get_or_init(|| bank(trigger::factory_presets(), |p| (p.name, p.params)))
            .clone()
    }

    /// A preset sets how hits are found and played: the sample, power and
    /// Listen stay as they are.
    fn preset_applied(self, current: &Params, preset: &Params) -> Params {
        let mut next = preset.clone();
        next.power = current.power;
        next.listen = current.listen;
        next.sample_name = current.sample_name.clone();
        next
    }

    fn knob(self, _id: &str) -> Option<KnobSpec> {
        None
    }

    fn panel(e: &KitEditor<Self>, cx: &mut Cx<Self>) -> AnyElement {
        let s = plate_scale(e.viewport);
        plate_frame(s, vec![display(e, cx, s), controls(e, cx, s)])
    }

    fn live_view(e: &KitEditor<Self>) -> TriggerView {
        TriggerView {
            params: e.params.clone(),
        }
    }

    fn paint_live(
        view: &TriggerView,
        live: &mut Live,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = window.viewport_size();
        let s = plate_scale((f32::from(viewport.width), f32::from(viewport.height)));
        let p = &view.params;
        let history = recent_history(live);
        let pads = live.pads().copied();
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_GRAPH)) {
            paint_graph(window, p, &history, b);
        }
        let mono = plate::mono(FontWeight::NORMAL);
        if let Some(frame) = live.frame {
            let bars = [db20(frame.in_peak), db20(frame.out_peak)];
            for (index, db) in bars.into_iter().enumerate() {
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_BAR + index)) {
                    paint_bar(
                        window,
                        b,
                        db,
                        if index == 0 { 0x9a9a9a } else { 0xe6e6e6 },
                        s,
                    );
                }
                if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_VALUE + index)) {
                    let (x, y) = plate::xy(b.center());
                    paint_text(
                        window,
                        cx,
                        &plate::signed(db.max(-99.9)),
                        mono.clone(),
                        10.5 * s,
                        color(plate::INK),
                        (x, y - 7.0 * s),
                        Align::Center,
                    );
                }
            }
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_LAMP)) {
            let flash = pads
                .map_or(0.0, |block| block[telemetry::HIT])
                .clamp(0.0, 1.0);
            let (x, y) = plate::xy(b.center());
            let r = f32::from(b.size.width) * 0.5;
            paint_dot(window, x, y, r, color(0x2a2a2a));
            if flash > 0.02 {
                paint_dot(window, x, y, r, color_a(ACCENT, flash));
            }
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_LAST)) {
            let text = pads
                .filter(|block| block[telemetry::HITS] > 0.0)
                .map_or_else(
                    || "\u{2013}".to_string(),
                    |block| plate::fmt_db(block[telemetry::LAST_GAIN_DB]),
                );
            let (x, y) = plate::xy(b.center());
            paint_text(
                window,
                cx,
                &text,
                mono.clone(),
                10.5 * s,
                accent(),
                (x, y - 7.0 * s),
                Align::Center,
            );
        }
        if let Some(b) = bounds.get(LiveSlot::Plate(SLOT_STATUS)) {
            if let Some(text) = status_text(p, pads.as_ref()) {
                let (x, y) = plate::xy(b.origin);
                paint_text(
                    window,
                    cx,
                    &text,
                    mono,
                    11.0 * s,
                    color(LEGEND),
                    (x + f32::from(b.size.width), y),
                    Align::Right,
                );
            }
        }
    }

    fn window_size(self) -> (f32, f32) {
        plate::WINDOW_SIZE
    }

    fn min_size(self) -> (f32, f32) {
        plate::MIN_WINDOW_SIZE
    }

    fn synced(e: &mut KitEditor<Self>, cx: &mut Cx<Self>) {
        if !e.ui.listed {
            e.ui.listed = true;
            list_files(cx);
        }
        // A load the host has not answered yet: the mirror still names the
        // sample before it.
        if e.ui.awaiting {
            return;
        }
        let wanted = e.params.sample_name.clone();
        if wanted != e.ui.shown {
            show_stored(e, wanted, cx);
        }
    }

    fn mouse_move(e: &mut KitEditor<Self>, event: &MouseMoveEvent, cx: &mut Cx<Self>) {
        let Some(Drag::Threshold) = e.ui.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            e.ui.drag = None;
            return;
        }
        let Some(b) = e.live_bounds.get(LiveSlot::Plate(SLOT_GRAPH)) else {
            return;
        };
        let (_, y) = plate::xy(event.position);
        let (_, by) = plate::xy(b.origin);
        let h = f32::from(b.size.height).max(1.0);
        let units = ((y - by) / h).clamp(0.0, 1.0) * GH;
        let db = -((units - 6.0) / (GH - 12.0)) * 60.0;
        let db = DialUnit::Db.round(db).clamp(-60.0, 0.0);
        e.set_value("thresholdDb", db, cx);
    }

    fn mouse_up(e: &mut KitEditor<Self>, _cx: &mut Cx<Self>) {
        e.ui.drag = None;
    }
}

fn db20(v: f32) -> f32 {
    20.0 * v.max(1.0e-9).log10()
}

/// The newest history frames, oldest first.
fn recent_history(live: &Live) -> Vec<HistoryPoint> {
    let count = live.history_len().min(SHOWN);
    (0..count).rev().map(|age| live.history_at(age)).collect()
}

/// The line under the display.
fn status_text(p: &Params, pads: Option<&[f32; telemetry::SLOTS]>) -> Option<String> {
    let block = pads?;
    if !p.power {
        return Some("Bypassed".into());
    }
    let hits = block[telemetry::HITS] as u32;
    let hits = match hits {
        1 => "1 hit".to_string(),
        n => format!("{n} hits"),
    };
    if block[telemetry::LOADED] < 0.5 {
        return Some(format!("{hits} \u{00B7} no sample: hits play nothing"));
    }
    let voices = block[telemetry::VOICES] as u32;
    Some(format!("{hits} \u{00B7} {voices} sounding"))
}

/// A hit's marker sits where the history's hit flag comes on.
fn hit_marks(history: &[HistoryPoint]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(i, point)| point.slot_lit && (*i == 0 || !history[i - 1].slot_lit))
        .map(|(i, _)| i)
        .collect()
}

// ── Painting the live layers ───────────────────────────────────────────────

fn graph_at(b: gpui::Bounds<gpui::Pixels>) -> impl Fn(f32, f32) -> (f32, f32) {
    let (bx, by) = plate::xy(b.origin);
    let kx = f32::from(b.size.width) / GW;
    let ky = f32::from(b.size.height) / GH;
    move |x, y| (bx + x * kx, by + y * ky)
}

fn paint_graph(
    window: &mut Window,
    p: &Params,
    history: &[HistoryPoint],
    b: gpui::Bounds<gpui::Pixels>,
) {
    let at = graph_at(b);
    for i in 1..8 {
        let x = i as f32 * GW / 8.0;
        let ink = color(if i % 4 == 0 { 0x262626 } else { 0x181818 });
        paint_polyline(window, &[at(x, 0.0), at(x, GH)], 1.0, ink, None);
    }
    for db in [-12.0, -24.0, -36.0, -48.0] {
        let y = db_y(db);
        paint_polyline(window, &[at(0.0, y), at(GW, y)], 1.0, color(0x262626), None);
    }
    if history.len() >= 2 {
        let step = GW / (SHOWN - 1) as f32;
        let n = history.len();
        let x_of = |i: usize| GW - (n - 1 - i) as f32 * step;
        let line = |read: fn(&HistoryPoint) -> f32| -> Vec<(f32, f32)> {
            history
                .iter()
                .enumerate()
                .map(|(i, point)| at(x_of(i), db_y(read(point))))
                .collect()
        };
        let out = line(|point| point.out_db);
        let detector = line(|point| point.slot_in_db);
        let (first, last) = (x_of(0), x_of(n - 1));
        let mut fill = out.clone();
        fill.push(at(last, GH));
        fill.push(at(first, GH));
        paint_polygon(window, &fill, color_a(0xe6e6e6, 0.07));
        paint_polyline(window, &out, 1.2, color_a(0xd6d6d6, 0.55), None);
        paint_polyline(
            window,
            &detector,
            if p.listen { 1.8 } else { 1.4 },
            color(0xe6e6e6),
            None,
        );
        for i in hit_marks(history) {
            let x = x_of(i);
            paint_polyline(
                window,
                &[at(x, 0.0), at(x, GH)],
                1.5,
                color_a(ACCENT, 0.75),
                None,
            );
            let (dx, dy) = at(x, 8.0);
            let k = f32::from(b.size.width) / GW;
            paint_dot(window, dx, dy, 4.0 * k, accent());
        }
    }
    let y = db_y(p.threshold_db);
    paint_polyline(
        window,
        &[at(0.0, y), at(GW, y)],
        1.0,
        color_a(ACCENT, 0.8),
        Some((5.0, 4.0)),
    );
}

/// A vertical meter from the foot up over −60…0 dB.
fn paint_bar(window: &mut Window, b: gpui::Bounds<gpui::Pixels>, db: f32, ink: u32, s: f32) {
    let (x, y) = plate::xy(b.origin);
    let w = f32::from(b.size.width);
    let h = f32::from(b.size.height);
    window.paint_quad(gpui::fill(plate::rect(x, y, w, h), color(GROUND)).corner_radii(px(2.0 * s)));
    let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0) * h;
    plate::paint_rect(window, (x, y + h - fill, w, fill), color(ink));
}

// ── Sample actions ─────────────────────────────────────────────────────────

/// Re-reads the Samples folder, off the UI thread.
fn list_files(cx: &mut Cx<TriggerPlate>) {
    let dir = samples_dir();
    cx.spawn(async move |this, cx| {
        let mut names = cx.background_spawn(async move { list_samples(&dir) }).await;
        let _ = this.update(cx, |e, cx| {
            // A sample loaded while the folder was being read stays listed.
            for name in e.ui.files.drain(..) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            names.sort_by_key(|name| name.to_lowercase());
            e.ui.files = names;
            cx.notify();
        });
    })
    .detach();
}

/// Reads a sample already in the Samples folder, only to draw it: the host
/// gets its audio from the studio's own reload.
fn show_stored(e: &mut KitEditor<TriggerPlate>, name: Option<String>, cx: &mut Cx<TriggerPlate>) {
    e.ui.generation = e.ui.generation.wrapping_add(1);
    e.ui.shown = name.clone();
    e.ui.status = None;
    let Some(name) = name else {
        e.ui.sample = None;
        e.ui.loading = false;
        return;
    };
    e.ui.loading = true;
    let generation = e.ui.generation;
    let path = samples_dir().join(&name);
    cx.spawn(async move |this, cx| {
        let result = cx
            .background_spawn(async move {
                let bytes = std::fs::read(&path)
                    .map_err(|error| format!("{} is missing: {error}", path.display()))?;
                describe(&name, &bytes)
            })
            .await;
        let _ = this.update(cx, |e, cx| {
            if e.ui.generation == generation {
                e.ui.loading = false;
                match result {
                    Ok(view) => e.ui.sample = Some(view),
                    Err(error) => {
                        e.ui.sample = None;
                        e.ui.status = Some(format!("Could not read the sample: {error}"));
                    }
                }
                cx.notify();
            }
        });
    })
    .detach();
}

/// Reads a sample with `read` off the UI thread, then sends it to the host
/// and draws it.
fn load_with(
    e: &mut KitEditor<TriggerPlate>,
    cx: &mut Cx<TriggerPlate>,
    read: impl FnOnce(&Path) -> Result<(String, Vec<u8>), String> + Send + 'static,
) {
    e.ui.loading = true;
    e.ui.status = None;
    e.ui.generation = e.ui.generation.wrapping_add(1);
    let generation = e.ui.generation;
    let dir = samples_dir();
    cx.notify();
    cx.spawn(async move |this, cx| {
        let result = cx
            .background_spawn(async move {
                let (name, bytes) = read(&dir)?;
                let view = describe(&name, &bytes)?;
                Ok::<_, String>((name, bytes, view))
            })
            .await;
        let _ = this.update(cx, |e, cx| {
            if e.ui.generation != generation {
                return;
            }
            match result {
                Ok((name, bytes, view)) => adopt(e, name, bytes, view, cx),
                Err(error) => {
                    e.ui.loading = false;
                    e.ui.status = Some(error);
                }
            }
            cx.notify();
        });
    })
    .detach();
}

/// Copies `source` into the Samples folder and loads it.
fn import(e: &mut KitEditor<TriggerPlate>, source: PathBuf, cx: &mut Cx<TriggerPlate>) {
    load_with(e, cx, move |dir| read_dropped_sample(dir, &source));
}

fn load_sound(e: &mut KitEditor<TriggerPlate>, sound: Sound, cx: &mut Cx<TriggerPlate>) {
    load_with(e, cx, move |dir| builtin_sound_file(dir, sound));
}

/// The previous (−1) or next (+1) file in the Samples folder.
fn step_sample(e: &mut KitEditor<TriggerPlate>, delta: i32, cx: &mut Cx<TriggerPlate>) {
    let Some(name) = step_in(&e.ui.files, e.ui.shown.as_deref(), delta) else {
        return;
    };
    if Some(name.as_str()) == e.ui.shown.as_deref() {
        return;
    }
    load_with(e, cx, move |dir| {
        let path = dir.join(&name);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("{} is missing: {error}", path.display()))?;
        Ok((name, bytes))
    });
}

/// "Load…": the sample is picked in the studio's Browser.
fn browse(cx: &mut Cx<TriggerPlate>) {
    let editor = cx.weak_entity();
    crate::browser_pick::request(cx, "Trigger", &SAMPLE_EXTENSIONS, move |path, cx| {
        let _ = editor.update(cx, |e, cx| import(e, path, cx));
    });
}

/// Sends a read sample to the host and shows it; the mirror records it when
/// the host answers that it loaded.
fn adopt(
    e: &mut KitEditor<TriggerPlate>,
    name: String,
    bytes: Vec<u8>,
    view: SampleView,
    cx: &mut Cx<TriggerPlate>,
) {
    e.ui.loading = false;
    let Some(load) = e.host_ops().load_pad_sample.clone() else {
        e.ui.status = Some("The plug-in host is not running yet.".into());
        return;
    };
    load(
        e.insert(),
        BuiltinDrumSampleLoadRequest {
            pad_index: 0,
            name: name.clone(),
            bytes,
        },
    );
    e.ui.awaiting = true;
    if !e.ui.files.contains(&name) {
        e.ui.files.push(name.clone());
        e.ui.files.sort_by_key(|name| name.to_lowercase());
    }
    e.ui.shown = Some(name);
    e.ui.sample = Some(view);
    cx.notify();
}

/// Empties the slot: the host answers with an empty name, which the mirror
/// records as no sample.
fn clear_sample(e: &mut KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>) {
    let Some(load) = e.host_ops().load_pad_sample.clone() else {
        return;
    };
    load(
        e.insert(),
        BuiltinDrumSampleLoadRequest {
            pad_index: 0,
            name: String::new(),
            bytes: Vec::new(),
        },
    );
    e.ui.generation = e.ui.generation.wrapping_add(1);
    e.ui.awaiting = true;
    e.ui.shown = None;
    e.ui.sample = None;
    e.ui.loading = false;
    e.ui.status = None;
    cx.notify();
}

/// The host's answer to a sample load for this insert. The mirror has
/// recorded a successful one by now, so the editor catches up with it.
pub fn notify_sample_result(
    e: &mut KitEditor<TriggerPlate>,
    ok: bool,
    name: &str,
    error: Option<&str>,
    cx: &mut Cx<TriggerPlate>,
) {
    e.ui.awaiting = false;
    // A failed load leaves the mirror on the sample before it, so this goes
    // back to that one; the reason is put up after it.
    e.sync_from_mirror(cx);
    if !ok {
        let error = error.unwrap_or("unknown error");
        e.ui.status = Some(if name.is_empty() {
            format!("The plug-in could not clear its sample: {error}")
        } else {
            format!("The plug-in could not load {name}: {error}")
        });
    }
    cx.notify();
}

// ── The well ───────────────────────────────────────────────────────────────

fn display(e: &KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>, s: f32) -> AnyElement {
    let p = &e.params;
    let inner = |y: f32| y * (GH - 2.0) / GH;
    let mut scale = div().absolute().left(px(6.0 * s)).top_0().h_full();
    for db in [0.0f32, -12.0, -24.0, -36.0, -48.0] {
        let text = if db == 0.0 {
            "0".to_string()
        } else {
            format!("\u{2212}{}", -db)
        };
        let y = inner(db_y(db)) + if db == 0.0 { 2.0 } else { -13.0 };
        scale = scale.child(
            div()
                .absolute()
                .left_0()
                .top(px(y * s))
                .child(numbers(text, 10.0, 0x8a8a8a, s)),
        );
    }
    let top = inner(db_y(p.threshold_db)) - 10.0;
    let handle = div()
        .id("trigger-threshold")
        .absolute()
        .right(px(6.0 * s))
        .top(px(top * s))
        .h(px(20.0 * s))
        .px(px(7.0 * s))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(accent())
        .bg(color(GROUND))
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .child(words("Thr", 10.5, SOFT, s))
        .child(numbers(plate::fmt_db(p.threshold_db), 11.0, WHITE, s))
        .on_mouse_down(
            MouseButton::Left,
            e.press_cb(cx, |this, _, event, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    this.ui.drag = None;
                    this.set_value("thresholdDb", THRESHOLD.dial.default, cx);
                } else {
                    this.ui.drag = Some(Drag::Threshold);
                }
            }),
        )
        .on_scroll_wheel(e.wheel_cb(cx, |this, dy, fine, cx| {
            let next = plate::wheel_turn(THRESHOLD.dial, this.params.threshold_db, dy, fine);
            this.set_value("thresholdDb", next, cx);
        }));
    let screen = div()
        .absolute()
        .left(px(14.0 * s))
        .top(px(2.0 * s))
        .w(px(GW * s))
        .h(px(GH * s))
        .bg(color(SCREEN))
        .border(px(1.0))
        .border_color(color(SCREEN_EDGE))
        .overflow_hidden()
        .child(live_slot(&e.live_bounds, SLOT_GRAPH).absolute().size_full())
        .child(plate::over_live(scale))
        .child(plate::over_live(handle));
    well(s, 7.0, 327.0)
        .child(plate::header(
            e,
            cx,
            s,
            &TriggerPlate.subtitle(p),
            accent(),
            Vec::new(),
        ))
        .child(
            div()
                .relative()
                .flex_1()
                .child(screen)
                .child(
                    div()
                        .absolute()
                        .left(px(14.0 * s))
                        .top(px(252.0 * s))
                        .w(px(GW * s))
                        .h(px(24.0 * s))
                        .child(legend(e, s)),
                )
                .child(meters(e, s)),
        )
        .into_any_element()
}

fn legend_item(swatch: gpui::Div, text: &str, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0 * s))
        .child(swatch)
        .child(words(text.to_string(), 11.0, LEGEND, s))
}

fn legend(e: &KitEditor<TriggerPlate>, s: f32) -> gpui::Div {
    let rule = |ink: Rgba, h: f32| div().w(px(14.0 * s)).h(px(h)).bg(ink);
    div()
        .flex()
        .flex_row()
        .items_center()
        .size_full()
        .gap(px(18.0 * s))
        .child(legend_item(
            rule(color(0xe6e6e6), 2.0 * s),
            if e.params.listen {
                "Detector (listening)"
            } else {
                "Detector"
            },
            s,
        ))
        .child(legend_item(rule(color_a(0xd6d6d6, 0.55), 1.0), "Output", s))
        .child(legend_item(rule(color_a(ACCENT, 0.8), 1.0), "Threshold", s))
        .child(legend_item(
            div().size(px(7.0 * s)).rounded_full().bg(accent()),
            "Hit",
            s,
        ))
        .child(div().flex_1())
        .child(
            div()
                .w(px(300.0 * s))
                .h(px(15.0 * s))
                .child(live_slot(&e.live_bounds, SLOT_STATUS).size_full()),
        )
}

/// In and Out bars, and the hit lamp with the last hit's level.
fn meters(e: &KitEditor<TriggerPlate>, s: f32) -> gpui::Div {
    let label = |text: &str| {
        words(text.to_string(), 10.5, LEGEND, s)
            .font_weight(FontWeight::MEDIUM)
            .line_height(px(12.0 * s))
    };
    let column = |index: usize, text: &str| {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0 * s))
            .w(px(40.0 * s))
            .child(
                div()
                    .w(px(14.0 * s))
                    .h(px(GH * s))
                    .child(live_slot(&e.live_bounds, SLOT_BAR + index).size_full()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(label(text))
                    .child(
                        div()
                            .w(px(40.0 * s))
                            .h(px(12.0 * s))
                            .child(live_slot(&e.live_bounds, SLOT_VALUE + index).size_full()),
                    ),
            )
    };
    let hit = div()
        .flex()
        .flex_col()
        .items_center()
        .justify_between()
        .w(px(44.0 * s))
        .h(px(GH * s))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0 * s))
                .child(
                    div()
                        .size(px(22.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_LAMP).size_full()),
                )
                .child(label("Hit")),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0 * s))
                .child(label("Last"))
                .child(
                    div()
                        .w(px(44.0 * s))
                        .h(px(12.0 * s))
                        .child(live_slot(&e.live_bounds, SLOT_LAST).size_full()),
                ),
        );
    div()
        .absolute()
        .left(px(852.0 * s))
        .top(px(2.0 * s))
        .w(px(140.0 * s))
        .h(px(274.0 * s))
        .flex()
        .flex_row()
        .justify_between()
        .child(column(0, "In"))
        .child(hit)
        .child(column(1, "Out"))
}

// ── The deck ───────────────────────────────────────────────────────────────

fn section(left: f32, width: f32, divider: bool, s: f32) -> gpui::Div {
    let base = div()
        .absolute()
        .left(px(left * s))
        .top(px(22.0 * s))
        .w(px(width * s))
        .h(px(376.0 * s))
        .px(px(22.0 * s))
        .flex()
        .flex_col()
        .gap(px(16.0 * s));
    if divider {
        base.border_l(px(1.0)).border_color(color(DIVIDER))
    } else {
        base
    }
}

fn section_head(title: &str, right: Vec<AnyElement>, s: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(24.0 * s))
        .child(
            words(title.to_string(), 14.0, WHITE, s)
                .font_weight(FontWeight::SEMIBOLD)
                .line_height(px(22.0 * s)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0 * s))
                .children(right),
        )
}

fn help(text: impl Into<SharedString>, s: f32) -> gpui::Div {
    words(text, 11.0, HELP, s)
}

fn dial(cx: &mut Cx<TriggerPlate>, knob: Knob, value: f32, enabled: bool, s: f32) -> AnyElement {
    let id = knob.dial.id;
    plate::knob_reading(
        cx,
        format!("trigger-{id}"),
        knob.dial,
        value,
        Some((knob.text)(value)),
        KNOB,
        CELL,
        accent(),
        enabled,
        s,
        move |this: &mut KitEditor<TriggerPlate>, value, cx| this.set_value(id, value, cx),
    )
}

const PILL: PillLook = PillLook::words(26.0, 10.0, 12.0);

fn controls(e: &KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>, s: f32) -> AnyElement {
    deck(s, 341.0, 420.0)
        .child(sample_section(e, cx, s))
        .child(detect_section(e, cx, s))
        .child(output_section(e, cx, s))
        .into_any_element()
}

fn sample_section(e: &KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let ui = &e.ui;
    let key = e.model.key();
    let connected = e.host_ops().load_pad_sample.is_some();
    let chevron = |d: &'static str| icon(d, (12.0, 12.0), (12.0, 12.0), 1.5, color(ICON), s);
    let can_step = connected && !ui.files.is_empty();
    let head_buttons = vec![
        square(
            (key, 950usize),
            26.0,
            chevron(plate::CHEVRON_LEFT),
            can_step,
            s,
            e.click_cb(cx, |this, cx| step_sample(this, -1, cx)),
        )
        .into_any_element(),
        square(
            (key, 951usize),
            26.0,
            chevron(plate::CHEVRON_RIGHT),
            can_step,
            s,
            e.click_cb(cx, |this, cx| step_sample(this, 1, cx)),
        )
        .into_any_element(),
        plate::pill(
            (key, 952usize),
            "Load\u{2026}",
            false,
            accent(),
            PILL,
            connected,
            s,
            e.click_cb(cx, |_, cx| browse(cx)),
        )
        .into_any_element(),
        plate::pill(
            (key, 953usize),
            "Clear",
            false,
            accent(),
            PILL,
            connected && ui.shown.is_some(),
            s,
            e.click_cb(cx, |this, cx| clear_sample(this, cx)),
        )
        .into_any_element(),
    ];

    let sounds: Vec<AnyElement> = Sound::ALL
        .iter()
        .enumerate()
        .map(|(index, &sound)| {
            let on = ui.shown.as_deref() == Some(sound.file_name());
            plate::pill(
                (key, 960 + index),
                sound.label(),
                on,
                accent(),
                PILL,
                connected,
                s,
                e.click_cb(cx, move |this, cx| load_sound(this, sound, cx)),
            )
            .into_any_element()
        })
        .collect();

    let info = if let Some(status) = ui.status.clone() {
        help(status, s).text_color(color(0xf87171))
    } else if !connected {
        help("Starting the plug-in host\u{2026}", s)
    } else if ui.loading {
        help("Reading the sample\u{2026}", s)
    } else if let Some(sample) = ui.sample.as_ref() {
        let channels = if sample.channels >= 2 {
            "stereo"
        } else {
            "mono"
        };
        help(
            format!(
                "{:.1} kHz \u{00B7} {channels} \u{00B7} {:.2} s",
                sample.sample_rate as f32 / 1_000.0,
                sample.seconds
            ),
            s,
        )
    } else {
        help(
            "Drop a sample on the waveform, load one, or pick a sound",
            s,
        )
    };

    let knobs = vec![
        dial(cx, PITCH, p.pitch, true, s),
        dial(cx, LENGTH, p.length_ms, true, s),
        dial(cx, FADE, p.fade_ms, !p.full_length(), s),
    ];

    section(0.0, 352.0, false, s)
        .child(section_head("Sample", head_buttons, s))
        .child(waveform(e, cx, s))
        .child(plate::columns(4.0, s, sounds))
        .child(div().h(px(14.0 * s)).child(info))
        .child(plate::columns(0.0, s, knobs))
}

/// The sample's waveform, its name, and where Length cuts it. A file dropped
/// here becomes the sample.
fn waveform(e: &KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>, s: f32) -> AnyElement {
    let ui = &e.ui;
    let peaks = ui.sample.as_ref().map(|sample| sample.peaks.clone());
    let cut = ui.sample.as_ref().and_then(|sample| {
        (!e.params.full_length() && sample.seconds > 0.0)
            .then(|| (e.params.length_ms as f64 / 1_000.0 / sample.seconds).min(1.0) as f32)
    });
    let ink = accent();
    let drawing = canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x0, y0) = plate::xy(b.origin);
            let w = f32::from(b.size.width);
            let h = f32::from(b.size.height);
            let mid = y0 + h * 0.5;
            paint_polyline(
                window,
                &[(x0, mid), (x0 + w, mid)],
                1.0,
                color(0x262626),
                None,
            );
            let Some(peaks) = peaks.as_ref() else {
                return;
            };
            let n = peaks.len().max(1);
            let column = w / n as f32;
            for (i, (low, high)) in peaks.iter().enumerate() {
                let x = x0 + i as f32 * column;
                let top = mid - high.clamp(-1.0, 1.0) * h * 0.46;
                let bottom = mid - low.clamp(-1.0, 1.0) * h * 0.46;
                let played = cut.is_none_or(|cut| (i as f32 + 0.5) / n as f32 <= cut);
                let ink = if played { ink } else { color(0x4a4a4a) };
                plate::paint_rect(
                    window,
                    (x, top, column.max(1.0), (bottom - top).max(1.0)),
                    ink,
                );
            }
            if let Some(cut) = cut {
                let x = x0 + cut * w;
                paint_polyline(
                    window,
                    &[(x, y0), (x, y0 + h)],
                    1.0,
                    color(WHITE),
                    Some((3.0, 3.0)),
                );
            }
        },
    )
    .absolute()
    .size_full();
    let name = ui
        .sample
        .as_ref()
        .map(|sample| sample.name.clone())
        .or_else(|| ui.shown.clone());
    let caption = div()
        .absolute()
        .left(px(6.0 * s))
        .top(px(5.0 * s))
        .max_w(px(280.0 * s))
        .px(px(4.0 * s))
        .rounded(px(2.0 * s))
        .bg(color_a(SCREEN, 0.8))
        .truncate()
        .child(match name {
            Some(name) => words(name, 11.5, WHITE, s),
            None => words("No sample", 11.5, OFF, s),
        });
    div()
        .id("trigger-waveform")
        .relative()
        .h(px(92.0 * s))
        .rounded(px(3.0 * s))
        .border(px(1.0))
        .border_color(color(SCREEN_EDGE))
        .bg(color(SCREEN))
        .overflow_hidden()
        .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(color(ACCENT)))
        .on_drop::<ExternalPaths>({
            let entity = cx.entity().clone();
            move |paths, _window, app| {
                if let Some(path) = paths.paths().first().cloned() {
                    let _ = entity.update(app, |this, cx| import(this, path, cx));
                }
            }
        })
        .child(drawing)
        .child(caption)
        .into_any_element()
}

fn detect_section(e: &KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let key = e.model.key();
    let listen = plate::pill(
        (key, 970usize),
        "Listen",
        p.listen,
        accent(),
        PILL,
        true,
        s,
        e.click_cb(cx, |this, cx| this.toggle("listen", cx)),
    )
    .into_any_element();
    let band = match (p.hpf_hz > 20.5, p.lpf_hz < 19_950.0) {
        (false, false) => "the whole input".to_string(),
        (true, false) => format!("above {}", f_hz(p.hpf_hz)),
        (false, true) => format!("below {}", f_hz(p.lpf_hz)),
        (true, true) => format!("{} to {}", f_hz(p.hpf_hz), f_hz(p.lpf_hz)),
    };
    let text = if p.listen {
        format!("Hearing the detector: {band}")
    } else {
        format!("Hits are found in {band}")
    };
    let first = vec![
        dial(cx, THRESHOLD, p.threshold_db, true, s),
        dial(cx, RETRIGGER, p.retrigger_ms, true, s),
        dial(cx, DYNAMICS, p.dynamics, true, s),
    ];
    let second = vec![
        dial(cx, HPF, p.hpf_hz, true, s),
        dial(cx, LPF, p.lpf_hz, true, s),
        dial(cx, OFFSET, p.offset_ms, true, s),
    ];
    section(352.0, 372.0, true, s)
        .child(section_head("Detect", vec![listen], s))
        .child(div().h(px(14.0 * s)).child(help(text, s)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(20.0 * s))
                .child(plate::columns(0.0, s, first))
                .child(plate::columns(0.0, s, second)),
        )
}

fn output_section(e: &KitEditor<TriggerPlate>, cx: &mut Cx<TriggerPlate>, s: f32) -> gpui::Div {
    let p = &e.params;
    let key = e.model.key();
    let modes: Vec<AnyElement> = Mode::ALL
        .iter()
        .enumerate()
        .map(|(index, &mode)| {
            let wire = mode.to_wire();
            plate::pill(
                (key, 980 + index),
                mode.label(),
                p.mode == mode,
                accent(),
                PILL,
                true,
                s,
                e.click_cb(cx, move |this, cx| this.set_value("mode", wire, cx)),
            )
            .into_any_element()
        })
        .collect();
    let text = match p.mode {
        Mode::Replace => "The sample alone: the track is muted",
        Mode::Blend => "The sample over the track",
    };
    let knobs = vec![
        dial(cx, SAMPLE_LEVEL, p.sample_db, true, s),
        dial(cx, DRY, p.dry_db, p.mode == Mode::Blend, s),
    ];
    let voices = vec![
        dial(cx, VOICES, p.voices as f32, true, s),
        div().into_any_element(),
    ];
    section(724.0, 287.0, true, s)
        .child(section_head("Output", Vec::new(), s))
        .child(plate::columns(4.0, s, modes))
        .child(div().h(px(14.0 * s)).child(help(text, s)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(20.0 * s))
                .child(plate::columns(0.0, s, knobs))
                .child(plate::columns(0.0, s, voices)),
        )
}

/// Opens with a track's Trigger insert.
pub fn open_trigger_editor(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<TriggerWindow>, String> {
    plate::ensure_fonts(cx);
    open_kit_editor(
        TriggerPlate,
        owner_bounds,
        key,
        identity,
        host_ops,
        on_close,
        cx,
    )
}

/// The UI preview's scenes: a sample shown without a host.
#[cfg(feature = "ui-preview")]
pub fn show_sample_for_preview(e: &mut KitEditor<TriggerPlate>, sound: Sound) {
    let bytes = sound.wav_bytes();
    if let Ok(view) = describe(sound.file_name(), &bytes) {
        // Whatever read the editor started on open is not this one.
        e.ui.generation = e.ui.generation.wrapping_add(1);
        e.ui.loading = false;
        e.ui.status = None;
        e.ui.shown = Some(view.name.clone());
        e.ui.sample = Some(view);
        e.ui.files = Sound::ALL
            .iter()
            .map(|s| s.file_name().to_string())
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readouts_print_their_units() {
        assert_eq!(f_offset(0.0), "0 ms");
        assert_eq!(f_offset(-2.0), "\u{2212}2.0 ms");
        assert_eq!(f_offset(12.34), "+12.3 ms");
        assert_eq!(f_pitch(0.0), "0 st");
        assert_eq!(f_pitch(-12.0), "\u{2212}12 st");
        assert_eq!(f_pitch(3.5), "+3.5 st");
        assert_eq!(f_length(LENGTH_FULL_MS), "Full");
        assert_eq!(f_length(250.0), "250 ms");
        assert_eq!(f_voices(1.0), "1 \u{00B7} cut");
        assert_eq!(f_voices(4.0), "4");
        assert_eq!(f_hpf(20.0), "Off");
        assert_eq!(f_hpf(80.0), "80 Hz");
        assert_eq!(f_lpf(20_000.0), "Off");
        assert_eq!(f_lpf(4_500.0), "4.5 kHz");
    }

    #[test]
    fn the_dials_match_the_wire_ranges() {
        for knob in [
            THRESHOLD,
            RETRIGGER,
            DYNAMICS,
            HPF,
            LPF,
            OFFSET,
            PITCH,
            LENGTH,
            FADE,
            SAMPLE_LEVEL,
            DRY,
            VOICES,
        ] {
            let index = trigger::ui_param_index(knob.dial.id).expect("a wire param");
            let (min, max) = trigger::ipc::RANGES[index as usize];
            assert_eq!(
                (knob.dial.min, knob.dial.max),
                (min, max),
                "{}",
                knob.dial.id
            );
            let defaults = trigger::ipc::ui_values(&trigger::default_params());
            assert_eq!(
                knob.dial.default, defaults[index as usize].1,
                "{} resets to the default",
                knob.dial.id
            );
        }
    }

    #[test]
    fn hits_are_marked_where_the_flag_comes_on() {
        let point = |lit: bool| HistoryPoint {
            slot_lit: lit,
            ..HistoryPoint::default()
        };
        let history = [
            point(false),
            point(true),
            point(true),
            point(false),
            point(true),
        ];
        assert_eq!(hit_marks(&history), vec![1, 4]);
        assert_eq!(hit_marks(&[point(true)]), vec![0]);
    }

    #[test]
    fn the_status_line_says_what_hits_do() {
        let mut block = [0.0; telemetry::SLOTS];
        let p = trigger::default_params();
        assert_eq!(status_text(&p, None), None);
        assert_eq!(
            status_text(&p, Some(&block)).as_deref(),
            Some("0 hits \u{00B7} no sample: hits play nothing")
        );
        block[telemetry::LOADED] = 1.0;
        block[telemetry::HITS] = 1.0;
        block[telemetry::VOICES] = 2.0;
        assert_eq!(
            status_text(&p, Some(&block)).as_deref(),
            Some("1 hit \u{00B7} 2 sounding")
        );
        let off = Params { power: false, ..p };
        assert_eq!(status_text(&off, Some(&block)).as_deref(), Some("Bypassed"));
    }

    #[test]
    fn a_built_in_sound_is_written_once_and_reads_as_audio() {
        let dir = std::env::temp_dir().join(format!(
            "trigger-sounds-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let (name, bytes) = builtin_sound_file(&dir, Sound::Snare).expect("written");
        assert_eq!(name, "Trigger Snare.wav");
        // A file of that name already there is used as it is.
        std::fs::write(dir.join(&name), &bytes[..]).unwrap();
        let (_, again) = builtin_sound_file(&dir, Sound::Snare).expect("read back");
        assert_eq!(again, bytes);
        let view = describe(&name, &bytes).expect("decodes");
        assert_eq!(view.channels, 1);
        assert_eq!(view.sample_rate, trigger::synth::SAMPLE_RATE);
        assert!(view.seconds > 0.3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn presets_keep_the_sample() {
        let current = Params {
            sample_name: Some("mine.wav".into()),
            listen: true,
            ..trigger::default_params()
        };
        let bank = TriggerPlate.presets();
        let applied = TriggerPlate.preset_applied(&current, &bank[1].params);
        assert_eq!(applied.sample_name.as_deref(), Some("mine.wav"));
        assert!(applied.listen);
        assert_eq!(applied.lpf_hz, bank[1].params.lpf_hz);
    }
}
