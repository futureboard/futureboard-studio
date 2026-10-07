//! The Slicer's native editor window.
//!
//! The editor of one Slicer insert, drawn natively with GPUI inside the
//! shared [`native_plugin_shell`]. It talks to the plug-in exactly as Quick
//! Sampler's does (see [`crate::components::quick_sampler_window`]): edits
//! are wire params sent with `forward_param`, a sample is copied into the
//! plug-in's Samples folder and sent with `load_pad_sample` (slot 0), and the
//! keyboard plays through `preview_note`.
//!
//! What it adds is the cut. The window keeps the decoded sample's mono mix
//! and works out slice points from it — at its hits
//! ([`SphereAudioProcessor::detect_transients`]), on its tempo's beat grid
//! (the same tempo detector as the clip inspector), or into equal lengths —
//! on a background thread, then sends them as slice-point params. The DSP
//! only ever reads points.
//!
//! What is sounding comes back the other way: the plug-in publishes its
//! playheads ([`slicer::telemetry`]) through the insert's pad-level block, and
//! the window polls it with the meter, so a slice played from the track or
//! the virtual keyboard lights up as one played here does.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Bounds, Context, ExternalPaths, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, div, px, size,
};

use crate::components::builtin_plugin_editor_window::{
    BuiltinDrumSampleLoadRequest, BuiltinEditorHostOps, PluginInstanceKey, read_dropped_sample,
};
use crate::components::builtin_plugin_files::{self as files, BuiltinFileKind};
use crate::components::context_menu::{ContextMenuEntry, context_menu_overlay};
use crate::components::native_plugin_shell::{
    NativeBuiltinEditor, SHELL_METER_INTERVAL, ShellIdentity, ShellMeter, native_plugin_shell,
};
use crate::components::quick_sampler_panel::SampleSummary;
use crate::components::quick_sampler_window::SAMPLE_EXTENSIONS;
use crate::components::slicer_panel::{
    SlicerCallbacks, SlicerPanelState, SoundingSlice, point_under, slicer_panel,
};
use crate::components::timeline::timeline_state::detect_tempo_from_mono;
use slicer::{SliceMode, SlicerParams, slicing};

pub const SLICER_WINDOW_WIDTH: f32 = 1_040.0;
pub const SLICER_WINDOW_HEIGHT: f32 = 860.0;
pub const SLICER_WINDOW_MIN_WIDTH: f32 = 720.0;
pub const SLICER_WINDOW_MIN_HEIGHT: f32 = 580.0;

const PREVIEW_VELOCITY: u8 = 100;
const WAVEFORM_BUCKETS: usize = 1_600;
/// Shortest slice the editor cuts or lets a drag make.
const MIN_SLICE_SECONDS: f64 = 0.01;
/// Shortest slice a transient cut makes: closer hits are one hit (a flam,
/// or the detector seeing one attack twice).
const MIN_TRANSIENT_SECONDS: f64 = 0.03;
/// The tempo range a loop is read in for the beat grid.
const TEMPO_MIN_BPM: f32 = 70.0;
const TEMPO_MAX_BPM: f32 = 180.0;
/// How long a slice's region flashes as it starts.
const FLASH: Duration = Duration::from_millis(250);
/// Telemetry not republished for this long is stale.
const TELEMETRY_STALE: Duration = Duration::from_millis(250);

/// A right-click on the display: where, and what was under it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SlicerMenu {
    x: f32,
    y: f32,
    /// Position in the sample.
    fraction: f32,
    /// The slice playing there.
    slice: Option<usize>,
    /// The slice point whose flag or line was clicked.
    flag: Option<usize>,
}

mod menu_command {
    pub const PLAY: &str = "play";
    pub const SPLIT: &str = "split";
    pub const JOIN: &str = "join";
    pub const AGAIN: &str = "again";
    pub const ONE: &str = "one";
}

/// The slice point a right-click would remove: the clicked flag, or the
/// start of the slice under the pointer (never the first).
fn menu_join_point(menu: &SlicerMenu) -> Option<usize> {
    menu.flag.or(menu.slice.filter(|slice| *slice > 0))
}

/// The display's right-click menu: hear the slice, cut it here, join it to
/// the one before, or cut the whole sample again.
fn slicer_menu_entries(p: &SlicerParams, menu: &SlicerMenu, min_gap: f32) -> Vec<ContextMenuEntry> {
    let item = |enabled: bool, label: String, command: &str| {
        if enabled {
            ContextMenuEntry::item(label, command)
        } else {
            ContextMenuEntry::disabled_item(label, command)
        }
    };
    let can_split = slicing::insert_point(*p, menu.fraction, min_gap).is_some();
    let join = menu_join_point(menu).filter(|_| p.slice_count > 1);
    vec![
        ContextMenuEntry::Header(match menu.slice {
            Some(slice) => format!("Slice {}", slice + 1),
            None => "Before the first slice".into(),
        }),
        item(
            menu.slice.is_some(),
            match menu.slice {
                Some(slice) => format!("Play Slice {}", slice + 1),
                None => "Play Slice".into(),
            },
            menu_command::PLAY,
        ),
        item(can_split, "Split Here".into(), menu_command::SPLIT),
        item(
            join.is_some(),
            match join {
                Some(point) if point > 0 => {
                    format!("Join Slice {} to Slice {}", point + 1, point)
                }
                Some(_) => "Remove First Marker".into(),
                None => "Join to the Slice Before".into(),
            },
            menu_command::JOIN,
        ),
        ContextMenuEntry::Separator,
        ContextMenuEntry::item("Slice Again", menu_command::AGAIN),
        item(
            p.slice_count > 1,
            "Reset to One Slice".into(),
            menu_command::ONE,
        ),
    ]
}

/// What the cut is worked out from: the decoded sample, mixed to mono.
#[derive(Clone)]
struct SliceSource {
    mono: Arc<Vec<f32>>,
    sample_rate: u32,
    seconds: f64,
}

/// Decodes `bytes` (a file named `name`), describes it for the display and
/// keeps its mono mix for slicing. Background thread only.
fn describe_sample(name: &str, bytes: &[u8]) -> Result<(SampleSummary, SliceSource), String> {
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
    .ok_or_else(|| "The file holds no audio.".to_string())?;
    Ok((
        SampleSummary {
            file_name: name.to_string(),
            sample_rate: sample.sample_rate(),
            channels: sample.channels(),
            seconds: sample.seconds(),
            peaks: Arc::new(sample.peaks(WAVEFORM_BUCKETS)),
        },
        SliceSource {
            mono: Arc::new(sample.mono()),
            sample_rate: sample.sample_rate(),
            seconds: sample.seconds(),
        },
    ))
}

/// The shortest slice, as a fraction of a sample `seconds` long.
fn min_gap(seconds: f64) -> f32 {
    gap_of(MIN_SLICE_SECONDS, seconds)
}

/// `gap` seconds as a fraction of a sample `seconds` long.
fn gap_of(gap: f64, seconds: f64) -> f32 {
    if seconds > 0.0 {
        (gap / seconds).min(1.0) as f32
    } else {
        0.0
    }
}

/// Where the attack a transient was detected at really starts. The detector
/// names the start of its analysis frame, up to a frame (tens of
/// milliseconds) before the hit — a cut there would hand the previous
/// slice's tail to this one. This looks a short way on for the hit's peak,
/// takes the first sample to reach half of it, walks back while the audio
/// before is not quiet, and leaves a millisecond of lead-in.
fn attack_onset(mono: &[f32], frame: usize, sample_rate: u32) -> usize {
    const BLOCK: usize = 32;
    let rate = sample_rate.max(1) as usize;
    let end = (frame + rate / 20).min(mono.len());
    if frame >= end {
        return frame;
    }
    let window = &mono[frame..end];
    let peak = window.iter().fold(0.0_f32, |peak, v| peak.max(v.abs()));
    if peak <= 1.0e-6 {
        return frame;
    }
    let Some(rise) = window.iter().position(|v| v.abs() >= 0.5 * peak) else {
        return frame;
    };
    let mut at = rise;
    while at >= BLOCK && window[at - BLOCK..at].iter().any(|v| v.abs() >= 0.1 * peak) {
        at -= BLOCK;
    }
    frame + at.saturating_sub(rate / 1_000)
}

/// The cut `p`'s slicing settings make of `source`: its slice points, and —
/// in beat mode with no tempo yet — the tempo it measured (`Some(0.0)` when
/// it found none). Background thread only.
fn cut_points(p: &SlicerParams, source: &SliceSource) -> (Vec<f32>, Option<f32>) {
    match p.slice_mode {
        SliceMode::Equal => (slicing::equal_points(p.equal_slices as usize), None),
        SliceMode::Beat => {
            let (bpm, measured) = if p.sample_bpm > 0.0 {
                (p.sample_bpm, None)
            } else {
                let bpm = detect_tempo_from_mono(
                    &source.mono,
                    source.sample_rate as f32,
                    TEMPO_MIN_BPM,
                    TEMPO_MAX_BPM,
                    None,
                )
                .map(|detection| slicer::sanitize_bpm(detection.bpm))
                .unwrap_or(0.0);
                (bpm, Some(bpm))
            };
            (
                slicing::beat_points(source.seconds, bpm, p.beat_division),
                measured,
            )
        }
        SliceMode::Transient => {
            let frames = source.mono.len().max(1) as f32;
            let mono = source.mono.as_slice();
            let hits: Vec<(f32, f32)> = SphereAudioProcessor::detect_transients(
                &source.mono,
                source.sample_rate,
                SphereAudioProcessor::TransientDetectParams {
                    sensitivity: p.sensitivity,
                    min_gap_ms: (MIN_TRANSIENT_SECONDS * 1_000.0) as f32,
                    ..Default::default()
                },
            )
            .iter()
            .map(|marker| {
                let onset = attack_onset(mono, marker.source_frame as usize, source.sample_rate);
                (onset as f32 / frames, marker.strength)
            })
            .collect();
            (
                slicing::transient_points(&hits, gap_of(MIN_TRANSIENT_SECONDS, source.seconds)),
                None,
            )
        }
    }
}

pub struct SlicerEditorWindow {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    panel: SlicerPanelState,
    /// The sample file the panel shows (or is reading).
    shown_sample: Option<String>,
    /// The shown sample's audio, for cutting it again.
    source: Option<SliceSource>,
    /// Bumped per read, so a slow read for a file since replaced cannot land
    /// over the current one.
    load_generation: u64,
    /// Bumped per cut, so only the latest one lands.
    slice_generation: u64,
    /// The key a press on the display is sounding, until the button lifts.
    audition: Option<u8>,
    /// When each sounding slice (by note and start number) was first seen,
    /// for its flash.
    sound_since: Vec<(u8, u16, Instant)>,
    /// The telemetry's last publish number, and when it last moved: a
    /// host that stopped publishing shows nothing playing, not a frozen
    /// playhead.
    telemetry_seq: Option<(u32, Instant)>,
    display_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The open right-click menu.
    menu: Option<SlicerMenu>,
    meter: ShellMeter,
    /// Cleared when the window closes, ending the meter timer.
    alive: Rc<Cell<bool>>,
}

impl SlicerEditorWindow {
    pub fn new(
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut window = Self {
            key,
            identity,
            host_ops,
            on_close,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            panel: SlicerPanelState::default(),
            shown_sample: None,
            source: None,
            load_generation: 0,
            slice_generation: 0,
            audition: None,
            sound_since: Vec::new(),
            telemetry_seq: None,
            display_bounds: Rc::new(Cell::new(None)),
            menu: None,
            meter: ShellMeter::default(),
            alive: Rc::new(Cell::new(true)),
        };
        window.sync_from_mirror(cx);
        window.start_meter(cx);
        window
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    /// Re-reads the insert's params from Studio's state mirror — after an
    /// undo, a preset, or a project reload — and the sample it names.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        let params =
            crate::components::builtin_plugin_editor::builtin_slicer_params(&self.key.insert_id)
                .unwrap_or_default();
        if self.panel.dragging.is_none() {
            self.panel.params = params.slicer;
            let count = self.panel.params.slice_count as usize;
            self.panel.selected = self.panel.selected.filter(|index| *index < count);
        }
        if params.sample_name != self.shown_sample {
            self.show_stored_sample(params.sample_name, cx);
        }
        cx.notify();
    }

    /// Rebinds the window to another insert, keeping the OS window.
    pub fn rebind(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        if key != self.key {
            self.release_all(cx);
            self.key = key;
            self.shown_sample = None;
            self.source = None;
            self.panel.dragging = None;
            self.panel.selected = None;
            self.panel.analysing = false;
            self.slice_generation = self.slice_generation.wrapping_add(1);
            self.panel.sounding.clear();
            self.sound_since.clear();
            self.telemetry_seq = None;
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
    }

    fn samples_dir(&self) -> PathBuf {
        files::plugin_files_root(slicer::PLUGIN_NAME).join(BuiltinFileKind::Samples.dir_name())
    }

    /// Reads a sample already in the Samples folder, to draw it and to have
    /// it at hand for cutting again. Its slices are already in the params.
    fn show_stored_sample(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        self.shown_sample = name.clone();
        self.source = None;
        self.panel.status = None;
        let Some(name) = name else {
            self.panel.sample = None;
            self.panel.loading = false;
            return;
        };
        self.panel.loading = true;
        let generation = self.load_generation;
        let path = self.samples_dir().join(&name);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let bytes = std::fs::read(&path)
                        .map_err(|error| format!("{} is missing: {error}", path.display()))?;
                    describe_sample(&name, &bytes)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation == generation {
                    this.apply_summary(result);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_summary(&mut self, result: Result<(SampleSummary, SliceSource), String>) {
        self.panel.loading = false;
        match result {
            Ok((sample, source)) => {
                self.panel.sample = Some(sample);
                self.source = Some(source);
                self.panel.status = None;
            }
            Err(error) => {
                self.panel.sample = None;
                self.source = None;
                self.panel.status = Some(format!("Could not read the sample: {error}"));
            }
        }
    }

    fn browse_sample(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            cx.spawn(async move |this, cx| {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Load Sample")
                    .add_filter("Audio", &SAMPLE_EXTENSIONS)
                    .pick_file()
                    .await
                else {
                    return;
                };
                let path = handle.path().to_path_buf();
                let _ = this.update(cx, |this, cx| this.import_sample(path, cx));
            })
            .detach();
        }
        #[cfg(not(feature = "native-dialogs"))]
        {
            let _ = SAMPLE_EXTENSIONS;
            self.panel.status = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    /// Copies `source` into the plug-in's Samples folder, sends it to the
    /// host, draws it, and cuts it with the current slicing settings.
    fn import_sample(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        self.panel.loading = true;
        self.panel.status = None;
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let samples_dir = self.samples_dir();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let (name, bytes) = read_dropped_sample(&samples_dir, &source)?;
                    let described = describe_sample(&name, &bytes)?;
                    Ok::<_, String>((name, bytes, described))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                match result {
                    Ok((name, bytes, described)) => this.adopt_sample(name, bytes, described, cx),
                    Err(error) => this.apply_summary(Err(error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn adopt_sample(
        &mut self,
        name: String,
        bytes: Vec<u8>,
        described: (SampleSummary, SliceSource),
        cx: &mut Context<Self>,
    ) {
        self.release_all(cx);
        let Some(load) = self.host_ops.load_pad_sample.clone() else {
            self.panel.loading = false;
            self.panel.status = Some("The plug-in host is not running yet.".into());
            return;
        };
        load(
            &self.key,
            BuiltinDrumSampleLoadRequest {
                pad_index: 0,
                name: name.clone(),
                bytes,
            },
        );
        self.shown_sample = Some(name);
        self.apply_summary(Ok(described));
        self.panel.selected = None;
        // A new file has a tempo of its own, measured when the beat grid
        // needs it.
        let mut next = self.panel.params;
        next.sample_bpm = 0.0;
        self.reslice(next, cx);
    }

    /// The host's answer to a sample load for this insert.
    pub fn notify_sample_result(&mut self, ok: bool, name: &str, error: Option<&str>) {
        if !ok && self.shown_sample.as_deref() == Some(name) {
            self.panel.status = Some(format!(
                "The plug-in could not load {name}: {}",
                error.unwrap_or("unknown error")
            ));
        }
    }

    /// Sends what changed between the panel's params and `next` as wire
    /// edits. Each one reaches the host DSP, the state mirror, and the
    /// project's dirty flag through `forward_param`.
    fn set_params(&mut self, next: SlicerParams, cx: &mut Context<Self>) {
        let next = next.sanitized();
        let edits = slicer::ipc::wire_diff(&self.panel.params, &next);
        self.panel.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        cx.notify();
    }

    /// Takes `next`'s slicing settings and cuts the sample with them, off the
    /// UI thread. The settings are sent at once; the slices when the cut
    /// lands — unless a newer cut or another sample has replaced it.
    fn reslice(&mut self, next: SlicerParams, cx: &mut Context<Self>) {
        self.set_params(next, cx);
        let Some(source) = self.source.clone() else {
            return;
        };
        self.slice_generation = self.slice_generation.wrapping_add(1);
        let generation = self.slice_generation;
        let settings = self.panel.params;
        self.panel.analysing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let gap = min_gap(source.seconds);
            let (points, measured) = cx
                .background_spawn(async move { cut_points(&settings, &source) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.slice_generation != generation {
                    return;
                }
                this.panel.analysing = false;
                let mut next = this.panel.params;
                if let Some(bpm) = measured {
                    next.sample_bpm = bpm;
                    if bpm <= 0.0 {
                        this.panel.status = Some(
                            "No steady tempo found. Set it with the TEMPO buttons, or slice by \
                             transients."
                                .into(),
                        );
                    }
                }
                let next = slicing::with_points(next, &points, gap);
                this.panel.selected = None;
                this.set_params(next, cx);
            });
        })
        .detach();
    }

    fn seconds(&self) -> f64 {
        self.panel.sample.as_ref().map_or(0.0, |s| s.seconds)
    }

    fn fraction_at(&self, x: f32) -> Option<f32> {
        let bounds = self.display_bounds.get()?;
        let left = f32::from(bounds.origin.x);
        let width = f32::from(bounds.size.width).max(1.0);
        Some(((x - left) / width).clamp(0.0, 1.0))
    }

    /// The slice point under a window-space pointer: on its flag, or near
    /// its line.
    fn point_at(&self, x: f32, y: f32) -> Option<usize> {
        let bounds = self.display_bounds.get()?;
        point_under(
            &self.panel.params,
            x - f32::from(bounds.origin.x),
            y - f32::from(bounds.origin.y),
            f32::from(bounds.size.width).max(1.0),
        )
    }

    /// A press on the display: on a flag, grab it; a double-click elsewhere,
    /// cut there; otherwise play the slice under the pointer.
    fn press_display(&mut self, x: f32, y: f32, clicks: usize, cx: &mut Context<Self>) {
        let Some(fraction) = self.fraction_at(x) else {
            return;
        };
        let p = self.panel.params;
        let on_flag = self.point_at(x, y);
        if clicks >= 2 {
            self.stop_audition(cx);
            let inserted = on_flag
                .is_none()
                .then(|| slicing::insert_point(p, fraction, min_gap(self.seconds())))
                .flatten();
            if let Some(next) = inserted {
                self.panel.selected = slicing::slice_at(&next, fraction);
                self.set_params(next, cx);
            }
            return;
        }
        if let Some(index) = on_flag {
            self.panel.dragging = Some(index);
            self.panel.selected = Some(index);
            cx.notify();
            return;
        }
        if let Some(index) = slicing::slice_at(&p, fraction) {
            self.panel.selected = Some(index);
            self.stop_audition(cx);
            if let Some(pitch) = p.note_for_slice(index) {
                self.note_on(pitch, cx);
                self.audition = Some(pitch);
            }
            cx.notify();
        }
    }

    /// A right-click joins a slice to the one before: on a flag, that flag's
    /// slice (the first one too); anywhere else, the slice under the pointer.
    /// A right-click opens the display's menu on what is under the pointer.
    fn right_press_display(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some(fraction) = self.fraction_at(x) else {
            return;
        };
        let slice = slicing::slice_at(&self.panel.params, fraction);
        if slice.is_some() {
            self.panel.selected = slice;
        }
        self.menu = Some(SlicerMenu {
            x,
            y,
            fraction,
            slice,
            flag: self.point_at(x, y),
        });
        cx.notify();
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let p = self.panel.params;
        match command {
            menu_command::PLAY => {
                if let Some(pitch) = menu.slice.and_then(|slice| p.note_for_slice(slice)) {
                    self.note_on(pitch, cx);
                    self.note_off(pitch, cx);
                }
            }
            menu_command::SPLIT => {
                if let Some(next) = slicing::insert_point(p, menu.fraction, min_gap(self.seconds()))
                {
                    self.panel.selected = slicing::slice_at(&next, menu.fraction);
                    self.set_params(next, cx);
                }
            }
            menu_command::JOIN => {
                if let Some(point) = menu_join_point(&menu) {
                    self.remove_slice_point(point, cx);
                }
            }
            menu_command::AGAIN => self.reslice(p, cx),
            menu_command::ONE => {
                self.panel.selected = Some(0);
                self.set_params(slicing::with_points(p, &[0.0], 0.0), cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Removes slice point `index`, joining its slice to the one before.
    fn remove_slice_point(&mut self, index: usize, cx: &mut Context<Self>) {
        self.stop_audition(cx);
        if let Some(next) = slicing::remove_point(self.panel.params, index) {
            self.panel.selected = index.checked_sub(1);
            self.panel.dragging = None;
            self.set_params(next, cx);
        }
    }

    /// Delete or Backspace removes the picked slice's point.
    fn on_key_down(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.menu.take().is_some() {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if !matches!(event.keystroke.key.as_str(), "delete" | "backspace") {
            return;
        }
        if let Some(index) = self.panel.selected {
            cx.stop_propagation();
            self.remove_slice_point(index, cx);
        }
    }

    fn drag_point_to(&mut self, x: f32, cx: &mut Context<Self>) {
        let (Some(index), Some(fraction)) = (self.panel.dragging, self.fraction_at(x)) else {
            return;
        };
        let next = slicing::move_point(self.panel.params, index, fraction, min_gap(self.seconds()));
        if next != self.panel.params {
            self.set_params(next, cx);
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.stop_audition(cx);
        if self.panel.dragging.take().is_some() {
            cx.notify();
        }
    }

    fn stop_audition(&mut self, cx: &mut Context<Self>) {
        if let Some(pitch) = self.audition.take() {
            self.note_off(pitch, cx);
        }
    }

    fn preview(&self, pitch: u8, velocity: Option<u8>, cx: &mut App) {
        if let Some(preview) = self.host_ops.preview_note.as_ref() {
            preview(&self.key, 0, pitch, velocity, cx);
        }
    }

    fn note_on(&mut self, pitch: u8, cx: &mut Context<Self>) {
        if !self.panel.is_playable() || self.panel.active_notes.contains(&pitch) {
            return;
        }
        self.panel.active_notes.push(pitch);
        if let Some(index) = self.panel.params.slice_for_note(pitch) {
            self.panel.selected = Some(index);
        }
        self.preview(pitch, Some(PREVIEW_VELOCITY), cx);
        cx.notify();
    }

    fn note_off(&mut self, pitch: u8, cx: &mut Context<Self>) {
        if !self.panel.active_notes.contains(&pitch) {
            return;
        }
        self.panel.active_notes.retain(|held| *held != pitch);
        self.preview(pitch, None, cx);
        cx.notify();
    }

    fn release_all(&mut self, cx: &mut App) {
        self.audition = None;
        for pitch in std::mem::take(&mut self.panel.active_notes) {
            self.preview(pitch, None, cx);
        }
    }

    /// Polls the insert's output meter at the CEF editors' telemetry rate,
    /// redrawing only when the reading moves.
    fn start_meter(&mut self, cx: &mut Context<Self>) {
        let alive = self.alive.clone();
        cx.spawn(async move |this, cx| {
            while alive.get() {
                cx.background_executor().timer(SHELL_METER_INTERVAL).await;
                let keep = this.update(cx, |this, cx| {
                    let key = this.key.clone();
                    let meter = this.meter.poll(this.host_ops.meter_source.as_ref(), &key);
                    if this.poll_sounding() || meter {
                        cx.notify();
                    }
                });
                if keep.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reads which slices the plug-in is sounding, and where, from its
    /// telemetry. `true` when the drawing changes.
    fn poll_sounding(&mut self) -> bool {
        let now = Instant::now();
        let published = self
            .host_ops
            .pad_level_source
            .as_ref()
            .and_then(|source| source(&self.key));
        let heads = match published {
            Some((seq, slots)) => {
                let moved = match self.telemetry_seq {
                    Some((last, at)) if last == seq => at,
                    _ => now,
                };
                self.telemetry_seq = Some((seq, moved));
                if now.duration_since(moved) > TELEMETRY_STALE {
                    Vec::new()
                } else {
                    slots
                        .first_chunk::<{ slicer::telemetry::SLOTS }>()
                        .map(slicer::telemetry::decode)
                        .unwrap_or_default()
                }
            }
            None => Vec::new(),
        };
        let since = std::mem::take(&mut self.sound_since);
        let mut sounding = Vec::with_capacity(heads.len());
        for head in &heads {
            let started = since
                .iter()
                .find(|(note, start, _)| *note == head.note && *start == head.start)
                .map_or(now, |(_, _, at)| *at);
            self.sound_since.push((head.note, head.start, started));
            let age = now.duration_since(started).as_secs_f32();
            sounding.push(SoundingSlice {
                note: head.note,
                position: head.position,
                flash: (1.0 - age / FLASH.as_secs_f32()).max(0.0),
            });
        }
        if sounding == self.panel.sounding {
            return false;
        }
        self.panel.sounding = sounding;
        true
    }

    fn callbacks(&self, cx: &mut Context<Self>) -> SlicerCallbacks {
        let entity = cx.entity().clone();
        fn with<T: Clone + 'static>(
            entity: &gpui::Entity<SlicerEditorWindow>,
            f: impl Fn(&mut SlicerEditorWindow, T, &mut Context<SlicerEditorWindow>) + 'static,
        ) -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |value: &T, _window, app: &mut App| {
                let value = value.clone();
                let _ = entity.update(app, |this, cx| f(this, value, cx));
            })
        }
        let browse = entity.clone();
        SlicerCallbacks {
            on_browse: Arc::new(move |_window, app: &mut App| {
                let _ = browse.update(app, |this, cx| this.browse_sample(cx));
            }),
            on_set_params: with(&entity, |this, params: SlicerParams, cx| {
                this.set_params(params, cx)
            }),
            on_reslice: with(&entity, |this, params: SlicerParams, cx| {
                this.reslice(params, cx)
            }),
            on_display_press: with(&entity, |this, (x, y, clicks): (f32, f32, usize), cx| {
                this.press_display(x, y, clicks, cx)
            }),
            on_display_right_press: with(&entity, |this, (x, y): (f32, f32), cx| {
                this.right_press_display(x, y, cx)
            }),
            on_note_on: with(&entity, |this, pitch: u8, cx| this.note_on(pitch, cx)),
            on_note_off: with(&entity, |this, pitch: u8, cx| this.note_off(pitch, cx)),
            on_shift_octave: with(&entity, |this, delta: i32, cx| {
                this.release_all(cx);
                this.panel.shift_keyboard_octave(delta);
                cx.notify();
            }),
        }
    }
}

impl NativeBuiltinEditor for SlicerEditorWindow {
    fn rebind_insert(
        &mut self,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        cx: &mut Context<Self>,
    ) {
        self.rebind(key, identity, host_ops, cx);
    }
}

impl Render for SlicerEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let alive = self.alive.clone();
        let content = slicer_panel(&self.panel, self.callbacks(cx), self.display_bounds.clone());

        let menu = self.menu.map(|menu| {
            let viewport = window.viewport_size();
            let command_target = entity.clone();
            let close_target = entity.clone();
            context_menu_overlay(
                slicer_menu_entries(&self.panel.params, &menu, min_gap(self.seconds())),
                menu.x,
                menu.y,
                viewport.width.into(),
                viewport.height.into(),
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ =
                        command_target.update(cx, |this, cx| this.run_menu_command(&command, cx));
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.menu = None;
                        cx.notify();
                    });
                }),
            )
        });

        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down({
                let entity = entity.clone();
                move |event: &gpui::KeyDownEvent, _window, cx| {
                    let _ = entity.update(cx, |this, cx| this.on_key_down(event, cx));
                }
            })
            // A flag drag follows the pointer anywhere in the window and ends
            // wherever the button comes up — as does a slice being heard.
            .on_mouse_move({
                let entity = entity.clone();
                move |event: &gpui::MouseMoveEvent, _window, cx| {
                    if event.pressed_button == Some(gpui::MouseButton::Left) {
                        let x = f32::from(event.position.x);
                        let _ = entity.update(cx, |this, cx| this.drag_point_to(x, cx));
                    }
                }
            })
            .on_mouse_up(gpui::MouseButton::Left, {
                let entity = entity.clone();
                move |_, _window, cx| {
                    let _ = entity.update(cx, |this, cx| this.end_drag(cx));
                }
            })
            .on_mouse_up_out(gpui::MouseButton::Left, {
                let entity = entity.clone();
                move |_, _window, cx| {
                    let _ = entity.update(cx, |this, cx| this.end_drag(cx));
                }
            })
            // A file dropped anywhere on the editor becomes its sample.
            .on_drop::<ExternalPaths>({
                let entity = entity.clone();
                move |paths, _window, cx| {
                    if let Some(path) = paths.paths().first().cloned() {
                        let _ = entity.update(cx, |this, cx| this.import_sample(path, cx));
                    }
                }
            })
            .child(native_plugin_shell(
                "slicer-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // Closing must not leave an auditioned slice held.
                    let _ = entity.update(cx, |this, cx| this.release_all(cx));
                    alive.set(false);
                    on_close(window, cx);
                    window.remove_window();
                },
                content,
            ))
            // The right-click menu, over everything.
            .children(menu)
    }
}

pub fn open_slicer_editor(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<SlicerEditorWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(SLICER_WINDOW_WIDTH), px(SLICER_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(SLICER_WINDOW_MIN_WIDTH),
        px(SLICER_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| SlicerEditorWindow::new(key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click track: a short burst every quarter second.
    fn clicks(seconds: f64, rate: u32) -> SliceSource {
        let frames = (seconds * rate as f64) as usize;
        let every = rate as usize / 4;
        let mono: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i % every;
                if t < 200 {
                    (t as f32 * 0.7).sin() * (1.0 - t as f32 / 200.0)
                } else {
                    0.0
                }
            })
            .collect();
        SliceSource {
            mono: Arc::new(mono),
            sample_rate: rate,
            seconds,
        }
    }

    #[test]
    fn transient_slices_land_on_the_hits() {
        let source = clicks(2.0, 22_050);
        let (points, measured) = cut_points(&SlicerParams::default(), &source);
        assert!(measured.is_none());
        assert_eq!(points.len(), 8, "{points:?}");
        // On each hit, a millisecond early at most — never a frame early.
        for (index, point) in points.iter().enumerate() {
            let hit = index as f32 / 8.0;
            assert!(*point <= hit && hit - point < 0.001, "{points:?}");
        }
    }

    #[test]
    fn an_onset_is_found_where_the_attack_starts() {
        let rate = 22_050;
        let mut mono = vec![0.0_f32; rate as usize];
        for (t, sample) in mono[10_000..10_400].iter_mut().enumerate() {
            *sample = (t as f32 * 0.7).sin() * 0.8;
        }
        let onset = attack_onset(&mono, 9_100, rate);
        assert!((10_000 - 22..=10_000).contains(&onset), "{onset}");
        // Nothing to find: the detector's frame stands.
        assert_eq!(attack_onset(&vec![0.0; 4_000], 1_000, rate), 1_000);
    }

    #[test]
    fn equal_slices_need_no_analysis() {
        let p = SlicerParams {
            slice_mode: SliceMode::Equal,
            equal_slices: 5,
            ..SlicerParams::default()
        };
        let (points, _) = cut_points(&p, &clicks(1.0, 22_050));
        assert_eq!(points.len(), 5);
    }

    #[test]
    fn a_beat_grid_uses_a_known_tempo_as_is() {
        let p = SlicerParams {
            slice_mode: SliceMode::Beat,
            sample_bpm: 120.0,
            ..SlicerParams::default()
        };
        let (points, measured) = cut_points(&p, &clicks(2.0, 22_050));
        assert_eq!(measured, None);
        // Two seconds at 120 BPM are four beats: eight eighths.
        assert_eq!(points.len(), 8);
    }

    #[test]
    fn the_menu_offers_what_the_spot_allows() {
        let p = slicing::with_points(SlicerParams::default(), &[0.0, 0.5], 0.0);
        let labels = |menu: &SlicerMenu| -> Vec<(String, bool)> {
            slicer_menu_entries(&p, menu, 0.01)
                .into_iter()
                .filter_map(|entry| match entry {
                    ContextMenuEntry::Item {
                        label, disabled, ..
                    } => Some((label, disabled)),
                    _ => None,
                })
                .collect()
        };
        // Inside slice 2, away from its flag.
        let inside = SlicerMenu {
            x: 0.0,
            y: 0.0,
            fraction: 0.7,
            slice: Some(1),
            flag: None,
        };
        let items = labels(&inside);
        assert!(items.contains(&("Play Slice 2".into(), false)));
        assert!(items.contains(&("Split Here".into(), false)));
        assert!(items.contains(&("Join Slice 2 to Slice 1".into(), false)));
        // On a marker: no cut on top of it.
        let on_marker = SlicerMenu {
            fraction: 0.5,
            flag: Some(1),
            ..inside
        };
        assert!(labels(&on_marker).contains(&("Split Here".into(), true)));
        // In the first slice there is nothing before to join to.
        let first = SlicerMenu {
            fraction: 0.2,
            slice: Some(0),
            ..inside
        };
        assert!(labels(&first).contains(&("Join to the Slice Before".into(), true)));
    }

    #[test]
    fn a_missing_or_empty_file_is_reported_not_drawn() {
        assert!(describe_sample("nothing.wav", &[]).is_err());
    }
}
