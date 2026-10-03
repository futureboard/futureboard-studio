//! The Drum Sampler's native editor window.
//!
//! The editor of one Drum Sampler insert, drawn natively with GPUI inside the
//! shared [`native_plugin_shell`] — it replaces the plug-in's web editor. It
//! talks to the plug-in through the same host ops that editor used:
//!
//! * every edit is a wire param, sent with `forward_param` — which folds it
//!   into Studio's state mirror (what the project saves and a restarted host
//!   replays) and marks the project edited;
//! * a sample is stored in the plug-in's Samples folder and sent as bytes to
//!   one pad with `load_pad_sample`; the host decodes it, hands it to the
//!   audio thread and answers with the pad's waveform overview, which
//!   `plugin_ops.rs` caches and passes on through
//!   [`DrumSamplerEditorWindow::notify_pad_sample_result`];
//! * a pad pressed here plays through `preview_note`;
//! * the pads' levels come back through `pad_level_source`, polled with the
//!   meter, so a pad lights whatever played it — the track, a MIDI
//!   keyboard, the virtual keyboard, or a press here.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drumsampler::{PADS, Pad, Params};
use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, div, px, size,
};

use crate::components::builtin_plugin_editor::{DrumPadWaveform, drum_waveforms};
use crate::components::builtin_plugin_editor_window::{
    BuiltinDrumSampleLoadRequest, BuiltinEditorHostOps, PluginInstanceKey, read_dropped_sample,
};
use crate::components::builtin_plugin_files::{self as files, BuiltinFileKind};
use crate::components::context_menu::context_menu_overlay;
use crate::components::drum_sampler_menu::{
    DrumMenuTarget, command, free_output, menu_entries, next_empty_pad, paste_settings, reset_pad,
};
use crate::components::drum_sampler_panel::{
    DRAG_SLOP_PX, DrumSamplerCallbacks, DrumSamplerPanelState, DrumView, EDGE_GRAB_PX, FaderPress,
    PadAdjust, PadPress, RegionEdge, adjusted, bank_of, drum_sampler_panel, edge_near,
    fader_dragged, with_edge,
};
use crate::components::native_plugin_shell::{
    NativeBuiltinEditor, SHELL_METER_INTERVAL, ShellIdentity, ShellMeter, native_plugin_shell,
};
use crate::components::quick_sampler_window::SAMPLE_EXTENSIONS;
use crate::components::text_input::{
    TextInputState, bind_mouse_selection, text_field_with_callbacks_and_ime,
};

pub const DRUM_SAMPLER_WINDOW_WIDTH: f32 = 1_180.0;
pub const DRUM_SAMPLER_WINDOW_HEIGHT: f32 = 840.0;
pub const DRUM_SAMPLER_WINDOW_MIN_WIDTH: f32 = 920.0;
pub const DRUM_SAMPLER_WINDOW_MIN_HEIGHT: f32 = 620.0;

const PREVIEW_VELOCITY: u8 = 110;
/// Pad levels not republished for this long are stale: nothing is playing.
const LEVELS_STALE: Duration = Duration::from_millis(250);
/// A rise in a pad's level within this long of a press here is that
/// press's own sound, not a new hit.
const LOCAL_HIT_WINDOW: Duration = Duration::from_millis(120);
/// How long a note about a drop stays up.
const NOTE_FOR: Duration = Duration::from_secs(4);

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SAMPLE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The pads a drop of `count` files onto `first` loads, in order: the ones
/// from `first` on, as far as the last pad. The rest are only stored.
pub fn drop_pads(first: usize, count: usize) -> Vec<usize> {
    (first..PADS.min(first.saturating_add(count))).collect()
}

/// `next` as wire edits against `current`: every value that differs.
fn wire_diff(current: &Params, next: &Params) -> Vec<(u32, f32)> {
    drumsampler::ipc::ui_values(current)
        .into_iter()
        .zip(drumsampler::ipc::ui_values(next))
        .enumerate()
        .filter(|(_, ((_, a), (_, b)))| a != b)
        .map(|(index, (_, (_, value)))| (index as u32, value))
        .collect()
}

/// Whether a pad's level going from `before` to `now` is a new attack: a
/// clear jump, not the held peak's slow release or its small wobble.
pub fn is_new_hit(before: f32, now: f32) -> bool {
    now > 0.01 && now > before * 1.4 + 0.01
}

/// A mixer fader being dragged.
#[derive(Clone, Copy, Debug)]
struct FaderDrag {
    pad: usize,
    start_y: f32,
    start_db: f32,
}

/// A pad being dragged to set its gain or tune.
#[derive(Clone, Copy, Debug)]
struct PadDrag {
    pad: usize,
    field: PadAdjust,
    start_y: f32,
    start_value: f32,
    dragging: bool,
}

pub struct DrumSamplerEditorWindow {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    panel: DrumSamplerPanelState,
    search: TextInputState,
    wave_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pad_drag: Option<PadDrag>,
    fader_drag: Option<FaderDrag>,
    /// The open right-click menu: what it is for, and where.
    menu: Option<(DrumMenuTarget, f32, f32)>,
    /// A pad's settings, copied to paste onto another.
    copied: Option<Pad>,
    /// The note a press on a pad is sounding, until the button lifts.
    audition: Option<u8>,
    /// When each pad was last pressed here.
    local_hits: [Option<Instant>; PADS],
    /// The pad levels' last publish number, and when it last moved.
    levels_seq: Option<(u32, Instant)>,
    /// Bumped per note, so an old note's timer cannot clear a newer one.
    note_generation: u64,
    meter: ShellMeter,
    /// Cleared when the window closes, ending the meter timer.
    alive: Rc<Cell<bool>>,
}

impl DrumSamplerEditorWindow {
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
            panel: DrumSamplerPanelState::default(),
            search: TextInputState::new("drum-library-search", cx.focus_handle())
                .with_placeholder("Search samples")
                .with_accessible_label("Search samples"),
            wave_bounds: Rc::new(Cell::new(None)),
            pad_drag: None,
            fader_drag: None,
            menu: None,
            copied: None,
            audition: None,
            local_hits: [None; PADS],
            levels_seq: None,
            note_generation: 0,
            meter: ShellMeter::default(),
            alive: Rc::new(Cell::new(true)),
        };
        window.sync_from_mirror(cx);
        window.refresh_files(cx);
        window.start_meter(cx);
        window
    }

    pub fn key(&self) -> &PluginInstanceKey {
        &self.key
    }

    /// Re-reads the insert's kit from Studio's state mirror — after an undo,
    /// a preset or a project reload — and each pad's waveform from the cache
    /// the host's load answers fill.
    pub fn sync_from_mirror(&mut self, cx: &mut Context<Self>) {
        let params = crate::components::builtin_plugin_editor::builtin_drum_sampler_params(
            &self.key.insert_id,
        )
        .unwrap_or_else(drumsampler::default_params);
        if self.pad_drag.is_none() && self.fader_drag.is_none() && self.panel.region_drag.is_none()
        {
            self.panel.params = params;
        }
        let mut waveforms: Vec<Option<DrumPadWaveform>> = vec![None; PADS];
        for (pad, waveform) in drum_waveforms(&self.key.insert_id) {
            let Some(slot) = waveforms.get_mut(pad as usize) else {
                continue;
            };
            // Only the file the pad still names: an undo may have moved on.
            if self.panel.params.pads[pad as usize].sample_name.as_deref()
                == Some(waveform.name.as_str())
            {
                *slot = Some(waveform);
            }
        }
        self.panel.waveforms = waveforms;
        self.panel.connected = self.host_ops.load_pad_sample.is_some();
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
            self.release_audition(cx);
            self.key = key;
            self.pad_drag = None;
            self.fader_drag = None;
            self.panel.region_drag = None;
            self.panel.adjusting = None;
            self.panel.loading = [false; PADS];
            self.panel.errors = vec![None; PADS];
            self.panel.levels = [0.0; PADS];
            self.levels_seq = None;
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
    }

    fn samples_dir(&self) -> PathBuf {
        files::plugin_files_root(drumsampler::PLUGIN_NAME).join(BuiltinFileKind::Samples.dir_name())
    }

    /// Re-reads the Samples folder, off the UI thread.
    fn refresh_files(&mut self, cx: &mut Context<Self>) {
        let root = files::plugin_files_root(drumsampler::PLUGIN_NAME);
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_spawn(async move { files::list_files(&root, BuiltinFileKind::Samples) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.panel.files = Some(listed);
                cx.notify();
            });
        })
        .detach();
    }

    fn show_note(&mut self, note: String, cx: &mut Context<Self>) {
        self.note_generation = self.note_generation.wrapping_add(1);
        let generation = self.note_generation;
        self.panel.note = Some(note);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.note_generation == generation {
                    this.panel.note = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Sends what changed between the panel's kit and `next` as wire edits.
    /// Each one reaches the host DSP, the state mirror, and the project's
    /// dirty flag through `forward_param`.
    fn set_params(&mut self, mut next: Params, cx: &mut Context<Self>) {
        drumsampler::ipc::sanitize_params(&mut next);
        let edits = wire_diff(&self.panel.params, &next);
        self.panel.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        cx.notify();
    }

    /// One pad's new value. Its sample stays whatever the host last loaded:
    /// that is not a param.
    fn set_pad(&mut self, index: usize, mut pad: Pad, cx: &mut Context<Self>) {
        if index >= PADS {
            return;
        }
        let mut next = self.panel.params.clone();
        pad.sample_name = next.pads[index].sample_name.clone();
        next.pads[index] = pad;
        self.set_params(next, cx);
    }

    /// Every pad's output at once, sent as one set of edits.
    fn set_outputs(&mut self, outputs: [u8; PADS], cx: &mut Context<Self>) {
        let mut next = self.panel.params.clone();
        for (pad, output) in next.pads.iter_mut().zip(outputs) {
            pad.output = output;
        }
        self.set_params(next, cx);
    }

    fn set_master(&mut self, gain_db: f32, tune: f32, cx: &mut Context<Self>) {
        let mut next = self.panel.params.clone();
        next.master_gain_db = gain_db;
        next.master_tune = tune;
        self.set_params(next, cx);
    }

    /// Sends `bytes` (a file already in the Samples folder, named `name`) to
    /// pad `index`.
    fn send_to_pad(&mut self, index: usize, name: String, bytes: Vec<u8>) {
        let Some(load) = self.host_ops.load_pad_sample.clone() else {
            self.panel.loading[index] = false;
            self.panel.errors[index] = Some("The plug-in host is not running yet.".into());
            return;
        };
        self.panel.loading[index] = true;
        self.panel.errors[index] = None;
        load(
            &self.key,
            BuiltinDrumSampleLoadRequest {
                pad_index: index as u32,
                name,
                bytes,
            },
        );
    }

    /// Loads a file from the Samples folder onto the selected pad.
    fn load_file(&mut self, name: String, cx: &mut Context<Self>) {
        let index = self.panel.selected;
        let root = files::plugin_files_root(drumsampler::PLUGIN_NAME);
        self.panel.loading[index] = true;
        self.panel.errors[index] = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let read_name = name.clone();
            let read = cx
                .background_spawn(async move {
                    files::read_file_bytes(&root, BuiltinFileKind::Samples, &read_name)
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match read {
                    Ok(bytes) => this.send_to_pad(index, name, bytes),
                    Err(error) => {
                        this.panel.loading[index] = false;
                        this.panel.errors[index] = Some(format!("{name}: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let index = self.panel.selected;
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
                let _ = this.update(cx, |this, cx| this.place_files(Some(index), vec![path], cx));
            })
            .detach();
        }
        #[cfg(not(feature = "native-dialogs"))]
        {
            self.show_note(
                "Native file dialogs are unavailable in this build.".into(),
                cx,
            );
        }
    }

    /// Files dropped (or browsed): stored in the Samples folder, and loaded
    /// onto `first` and the pads after it — or, with `None`, only stored.
    fn place_files(&mut self, first: Option<usize>, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let (audio, rejected): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|path| is_audio_file(path));
        if audio.is_empty() {
            self.show_note(
                "Only WAV, FLAC, MP3, OGG, AIFF or M4A files can be loaded.".into(),
                cx,
            );
            return;
        }
        if !rejected.is_empty() {
            self.show_note(
                format!("Skipped {} file(s) that are not audio.", rejected.len()),
                cx,
            );
        }
        let targets: Vec<(Option<usize>, PathBuf)> = match first {
            Some(first) => {
                let pads = drop_pads(first, audio.len());
                audio
                    .into_iter()
                    .enumerate()
                    .map(|(offset, path)| (pads.get(offset).copied(), path))
                    .collect()
            }
            None => audio.into_iter().map(|path| (None, path)).collect(),
        };
        if let Some(first) = first {
            self.panel.selected = first;
            self.panel.bank = bank_of(first);
        }
        for (pad, _) in &targets {
            if let Some(pad) = pad {
                self.panel.loading[*pad] = true;
                self.panel.errors[*pad] = None;
            }
        }
        cx.notify();
        let samples_dir = self.samples_dir();
        let key = self.key.clone();
        cx.spawn(async move |this, cx| {
            let stored: Vec<_> = cx
                .background_spawn(async move {
                    targets
                        .into_iter()
                        .map(|(pad, path)| (pad, read_dropped_sample(&samples_dir, &path)))
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Another insert since: the files stand, the loads do not.
                let still_bound = this.key == key;
                for (pad, result) in stored {
                    match (pad.filter(|_| still_bound), result) {
                        (Some(pad), Ok((name, bytes))) => this.send_to_pad(pad, name, bytes),
                        (Some(pad), Err(error)) => {
                            this.panel.loading[pad] = false;
                            this.panel.errors[pad] = Some(error);
                        }
                        (None, Ok(_)) => {}
                        (None, Err(error)) => this.show_note(error, cx),
                    }
                }
                this.refresh_files(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The host's answer to a sample load for one of this insert's pads. The
    /// result handler has already folded the name into the mirror and the
    /// waveform into the cache.
    pub fn notify_pad_sample_result(
        &mut self,
        pad: usize,
        ok: bool,
        name: &str,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if pad >= PADS {
            return;
        }
        self.panel.loading[pad] = false;
        self.panel.errors[pad] =
            (!ok).then(|| format!("{name}: {}", error.unwrap_or("load failed")));
        self.sync_from_mirror(cx);
    }

    /// A press on a pad: select it, hear it, and flash it — every press,
    /// however fast they come (a double-click is two hits, not a reset). A
    /// drag from it then sets its gain, or with Shift its tune.
    fn press_pad(&mut self, press: PadPress, cx: &mut Context<Self>) {
        let index = press.pad;
        self.panel.selected = index;
        self.panel.bank = bank_of(index);
        let pad = self.panel.params.pads[index].clone();
        if pad.sample_name.is_none() {
            cx.notify();
            return;
        }
        let field = if press.shift {
            PadAdjust::Tune
        } else {
            PadAdjust::Gain
        };
        self.pad_drag = Some(PadDrag {
            pad: index,
            field,
            start_y: press.y,
            start_value: match field {
                PadAdjust::Gain => pad.gain_db,
                PadAdjust::Tune => pad.tune_semitones,
            },
            dragging: false,
        });
        self.panel.pressed = Some(index);
        self.panel.hits[index] = self.panel.hits[index].wrapping_add(1);
        self.local_hits[index] = Some(Instant::now());
        self.release_audition(cx);
        self.audition = Some(pad.note);
        self.preview(pad.note, Some(PREVIEW_VELOCITY), cx);
        cx.notify();
    }

    /// A press on a mixer fader grabs it; a double-click sets 0 dB.
    fn press_fader(&mut self, press: FaderPress, cx: &mut Context<Self>) {
        let index = press.pad;
        self.panel.selected = index;
        let pad = self.panel.params.pads[index].clone();
        if press.clicks >= 2 {
            let mut unity = pad;
            unity.gain_db = 0.0;
            self.set_pad(index, unity, cx);
            return;
        }
        self.fader_drag = Some(FaderDrag {
            pad: index,
            start_y: press.y,
            start_db: pad.gain_db,
        });
        cx.notify();
    }

    /// Shows bank `bank`; the selection follows to its first pad unless it
    /// is already there.
    fn show_bank(&mut self, bank: usize, cx: &mut Context<Self>) {
        let bank = bank.min(PADS / drumsampler::BANK_PADS - 1);
        self.panel.bank = bank;
        if bank_of(self.panel.selected) != bank {
            self.panel.selected = bank * drumsampler::BANK_PADS;
        }
        cx.notify();
    }

    /// Plays pad `index` once, from the editor.
    fn play_pad(&mut self, index: usize, cx: &mut Context<Self>) {
        let pad = &self.panel.params.pads[index];
        if pad.sample_name.is_none() {
            return;
        }
        let note = pad.note;
        self.panel.hits[index] = self.panel.hits[index].wrapping_add(1);
        self.local_hits[index] = Some(Instant::now());
        // One-shots ignore the note-off; it only keeps the preview balanced.
        self.preview(note, Some(PREVIEW_VELOCITY), cx);
        self.preview(note, None, cx);
        cx.notify();
    }

    fn open_menu(&mut self, target: DrumMenuTarget, x: f32, y: f32, cx: &mut Context<Self>) {
        if let DrumMenuTarget::Pad(index) = target {
            self.panel.selected = index;
        }
        self.menu = Some((target, x, y));
        cx.notify();
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        let Some((target, _, _)) = self.menu.take() else {
            return;
        };
        let selected = self.panel.selected;
        match target {
            DrumMenuTarget::Pad(index) => {
                let pad = self.panel.params.pads[index].clone();
                match command {
                    command::PLAY => self.play_pad(index, cx),
                    command::LOAD => {
                        self.panel.selected = index;
                        self.browse(cx);
                    }
                    command::COPY => self.copied = Some(pad),
                    command::PASTE => {
                        if let Some(source) = self.copied.clone() {
                            self.set_pad(index, paste_settings(&pad, &source), cx);
                        }
                    }
                    command::RESET => self.set_pad(index, reset_pad(index, &pad), cx),
                    command::OWN_OUTPUT => {
                        if pad.output == 0 {
                            if let Some(output) = free_output(&self.panel.params, index) {
                                self.set_pad(index, Pad { output, ..pad }, cx);
                            }
                        }
                    }
                    command::FIRST_OUTPUT => self.set_pad(index, Pad { output: 0, ..pad }, cx),
                    command::MUTE => {
                        let muted = !pad.muted;
                        self.set_pad(index, Pad { muted, ..pad }, cx);
                    }
                    command::SOLO => {
                        let solo = !pad.solo;
                        self.set_pad(index, Pad { solo, ..pad }, cx);
                    }
                    command::SHOW_MIXER => self.panel.view = DrumView::Mixer,
                    command::SHOW_PADS => self.panel.view = DrumView::Pads,
                    _ => {}
                }
            }
            DrumMenuTarget::File(name) => match command {
                command::LOAD_FILE => self.load_file(name, cx),
                command::LOAD_FILE_EMPTY => {
                    if let Some(pad) = next_empty_pad(&self.panel.params, selected) {
                        self.panel.selected = pad;
                        self.panel.bank = bank_of(pad);
                        self.load_file(name, cx);
                    }
                }
                command::REVEAL => crate::layout::reveal_path(&self.samples_dir().join(name)),
                _ => {}
            },
            DrumMenuTarget::Waveform => {
                let pad = self.panel.params.pads[selected].clone();
                match command {
                    command::PLAY => self.play_pad(selected, cx),
                    command::RESET_START => self.set_pad(selected, Pad { start: 0.0, ..pad }, cx),
                    command::RESET_END => self.set_pad(selected, Pad { end: 1.0, ..pad }, cx),
                    command::RESET_REGION => self.set_pad(
                        selected,
                        Pad {
                            start: 0.0,
                            end: 1.0,
                            ..pad
                        },
                        cx,
                    ),
                    command::REVERSE => {
                        let reverse = !pad.reverse;
                        self.set_pad(selected, Pad { reverse, ..pad }, cx);
                    }
                    _ => {}
                }
            }
        }
        cx.notify();
    }

    fn fraction_at(&self, x: f32) -> Option<(f32, f32)> {
        let bounds = self.wave_bounds.get()?;
        let width = f32::from(bounds.size.width).max(1.0);
        let fraction = ((x - f32::from(bounds.origin.x)) / width).clamp(0.0, 1.0);
        Some((fraction, EDGE_GRAB_PX / width))
    }

    /// A press on the waveform grabs the nearer region edge; a double-click
    /// on one resets it.
    fn press_waveform(&mut self, x: f32, clicks: usize, cx: &mut Context<Self>) {
        let Some((fraction, tolerance)) = self.fraction_at(x) else {
            return;
        };
        let index = self.panel.selected;
        let pad = self.panel.params.pads[index].clone();
        let Some(edge) = edge_near(&pad, fraction, tolerance) else {
            return;
        };
        if clicks >= 2 {
            let mut reset = pad;
            match edge {
                RegionEdge::Start => reset.start = 0.0,
                RegionEdge::End => reset.end = 1.0,
            }
            self.set_pad(index, reset, cx);
            return;
        }
        self.panel.region_drag = Some(edge);
        cx.notify();
    }

    fn drag_to(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        if let Some(drag) = self.fader_drag {
            let mut next = self.panel.params.pads[drag.pad].clone();
            next.gain_db = fader_dragged(drag.start_db, drag.start_y - y);
            if next.gain_db != self.panel.params.pads[drag.pad].gain_db {
                self.set_pad(drag.pad, next, cx);
            }
            return;
        }
        if let Some(edge) = self.panel.region_drag {
            if let Some((fraction, _)) = self.fraction_at(x) {
                let index = self.panel.selected;
                let next = with_edge(&self.panel.params.pads[index], edge, fraction);
                self.set_pad(index, next, cx);
            }
            return;
        }
        let Some(mut drag) = self.pad_drag else {
            return;
        };
        let rise = drag.start_y - y;
        if !drag.dragging {
            if rise.abs() < DRAG_SLOP_PX {
                return;
            }
            drag.dragging = true;
            self.pad_drag = Some(drag);
            self.panel.adjusting = Some((drag.pad, drag.field));
        }
        let value = adjusted(drag.field, drag.start_value, rise);
        let mut next = self.panel.params.pads[drag.pad].clone();
        match drag.field {
            PadAdjust::Gain => next.gain_db = value,
            PadAdjust::Tune => next.tune_semitones = value,
        }
        self.set_pad(drag.pad, next, cx);
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.release_audition(cx);
        let pad = self.pad_drag.take().is_some();
        let fader = self.fader_drag.take().is_some();
        let region = self.panel.region_drag.take().is_some();
        let adjusting = self.panel.adjusting.take().is_some();
        let pressed = self.panel.pressed.take().is_some();
        if pad || fader || region || adjusting || pressed {
            cx.notify();
        }
    }

    fn preview(&self, pitch: u8, velocity: Option<u8>, cx: &mut App) {
        if let Some(preview) = self.host_ops.preview_note.as_ref() {
            preview(&self.key, 0, pitch, velocity, cx);
        }
    }

    fn release_audition(&mut self, cx: &mut App) {
        if let Some(pitch) = self.audition.take() {
            self.preview(pitch, None, cx);
        }
    }

    /// Reads the pads' levels from the plug-in's telemetry. `true` when the
    /// drawing changes.
    fn poll_levels(&mut self) -> bool {
        let now = Instant::now();
        let published = self
            .host_ops
            .pad_level_source
            .as_ref()
            .and_then(|source| source(&self.key));
        let levels = match published {
            Some((seq, levels)) => {
                let moved = match self.levels_seq {
                    Some((last, at)) if last == seq => at,
                    _ => now,
                };
                self.levels_seq = Some((seq, moved));
                if now.duration_since(moved) > LEVELS_STALE {
                    [0.0; PADS]
                } else {
                    let mut out = [0.0; PADS];
                    for (slot, value) in out.iter_mut().zip(levels.iter()) {
                        *slot = if value.is_finite() { *value } else { 0.0 };
                    }
                    out
                }
            }
            None => [0.0; PADS],
        };
        if levels == self.panel.levels {
            return false;
        }
        // A new attack flashes its pad, whatever played it; a press here
        // has flashed it already.
        for (index, (before, now)) in self.panel.levels.iter().zip(levels).enumerate() {
            let pressed_here =
                self.local_hits[index].is_some_and(|at| at.elapsed() < LOCAL_HIT_WINDOW);
            if is_new_hit(*before, now) && !pressed_here {
                self.panel.hits[index] = self.panel.hits[index].wrapping_add(1);
            }
        }
        self.panel.levels = levels;
        true
    }

    /// Polls the insert's output meter and pad levels at the CEF editors'
    /// telemetry rate, redrawing only when a reading moves.
    fn start_meter(&mut self, cx: &mut Context<Self>) {
        let alive = self.alive.clone();
        cx.spawn(async move |this, cx| {
            while alive.get() {
                cx.background_executor().timer(SHELL_METER_INTERVAL).await;
                let keep = this.update(cx, |this, cx| {
                    let key = this.key.clone();
                    let meter = this.meter.poll(this.host_ops.meter_source.as_ref(), &key);
                    if this.poll_levels() || meter {
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

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" && self.menu.take().is_some() {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if !self.search.is_focused(window) {
            return;
        }
        if event.keystroke.key == "escape" && !self.search.value.is_empty() {
            self.search.set_value("");
            cx.stop_propagation();
        } else {
            let _ = self.search.handle_key_ime(event, Some(cx));
        }
        cx.notify();
    }

    fn callbacks(&self, cx: &mut Context<Self>) -> DrumSamplerCallbacks {
        let entity = cx.entity().clone();
        fn with<T: Clone + 'static>(
            entity: &gpui::Entity<DrumSamplerEditorWindow>,
            f: impl Fn(&mut DrumSamplerEditorWindow, T, &mut Context<DrumSamplerEditorWindow>) + 'static,
        ) -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |value: &T, _window, app: &mut App| {
                let value = value.clone();
                let _ = entity.update(app, |this, cx| f(this, value, cx));
            })
        }
        fn void(
            entity: &gpui::Entity<DrumSamplerEditorWindow>,
            f: impl Fn(&mut DrumSamplerEditorWindow, &mut Context<DrumSamplerEditorWindow>) + 'static,
        ) -> Arc<dyn Fn(&mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |_window, app: &mut App| {
                let _ = entity.update(app, |this, cx| f(this, cx));
            })
        }
        DrumSamplerCallbacks {
            on_pad_press: with(&entity, |this, press: PadPress, cx| {
                this.press_pad(press, cx)
            }),
            on_set_pad: with(&entity, |this, (index, pad): (usize, Pad), cx| {
                this.set_pad(index, pad, cx)
            }),
            on_set_master: with(&entity, |this, (gain, tune): (f32, f32), cx| {
                this.set_master(gain, tune, cx)
            }),
            on_drop: with(
                &entity,
                |this, (first, paths): (Option<usize>, Vec<PathBuf>), cx| {
                    this.place_files(first, paths, cx)
                },
            ),
            on_waveform_press: with(&entity, |this, (x, clicks): (f32, usize), cx| {
                this.press_waveform(x, clicks, cx)
            }),
            on_load_file: with(&entity, |this, name: String, cx| this.load_file(name, cx)),
            on_browse: void(&entity, |this, cx| this.browse(cx)),
            on_refresh: void(&entity, |this, cx| this.refresh_files(cx)),
            on_set_outputs: with(&entity, |this, outputs: [u8; PADS], cx| {
                this.set_outputs(outputs, cx)
            }),
            on_view: with(&entity, |this, view: DrumView, cx| {
                this.panel.view = view;
                cx.notify();
            }),
            on_bank: with(&entity, |this, bank: usize, cx| this.show_bank(bank, cx)),
            on_select: with(&entity, |this, pad: usize, cx| {
                this.panel.selected = pad.min(PADS - 1);
                cx.notify();
            }),
            on_fader_press: with(&entity, |this, press: FaderPress, cx| {
                this.press_fader(press, cx)
            }),
            on_context: with(
                &entity,
                |this, (target, x, y): (DrumMenuTarget, f32, f32), cx| {
                    this.open_menu(target, x, y, cx)
                },
            ),
        }
    }
}

crate::impl_single_input_window_ime!(DrumSamplerEditorWindow, search);

impl NativeBuiltinEditor for DrumSamplerEditorWindow {
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

impl Render for DrumSamplerEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let alive = self.alive.clone();
        let search_field = text_field_with_callbacks_and_ime(
            &self.search,
            self.search.is_focused(window),
            bind_mouse_selection(entity.clone(), |this| &mut this.search),
            entity.clone(),
        )
        .into_any_element();
        let content = drum_sampler_panel(
            &self.panel,
            self.callbacks(cx),
            search_field,
            &self.search.value,
            self.wave_bounds.clone(),
        );

        let menu = self.menu.clone().map(|(target, x, y)| {
            let viewport = window.viewport_size();
            let command_target = entity.clone();
            let close_target = entity.clone();
            context_menu_overlay(
                menu_entries(&self.panel, &target, self.copied.is_some()),
                x,
                y,
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
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            // A region-edge or pad drag follows the pointer anywhere in the
            // window and ends wherever the button comes up — as does a pad
            // being heard.
            .on_mouse_move({
                let entity = entity.clone();
                move |event: &gpui::MouseMoveEvent, _window, cx| {
                    if event.pressed_button == Some(gpui::MouseButton::Left) {
                        let (x, y) = (f32::from(event.position.x), f32::from(event.position.y));
                        let _ = entity.update(cx, |this, cx| this.drag_to(x, y, cx));
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
            .child(native_plugin_shell(
                "drum-sampler-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // Closing must not leave an auditioned pad held.
                    let _ = entity.update(cx, |this, cx| this.release_audition(cx));
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

pub fn open_drum_sampler_editor(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<DrumSamplerEditorWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(
            px(DRUM_SAMPLER_WINDOW_WIDTH),
            px(DRUM_SAMPLER_WINDOW_HEIGHT),
        ),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(DRUM_SAMPLER_WINDOW_MIN_WIDTH),
        px(DRUM_SAMPLER_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| DrumSamplerEditorWindow::new(key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drop_fills_the_pads_from_its_target_and_stops_at_the_last() {
        assert_eq!(drop_pads(3, 1), vec![3]);
        // Across a bank, into the next one…
        assert_eq!(drop_pads(13, 5), vec![13, 14, 15, 16, 17]);
        // …and no further than pad 64.
        assert_eq!(drop_pads(61, 5), vec![61, 62, 63]);
        assert!(drop_pads(15, 0).is_empty());
    }

    #[test]
    fn a_new_attack_is_a_jump_not_a_release() {
        assert!(is_new_hit(0.0, 0.5));
        assert!(is_new_hit(0.1, 0.6), "a retrigger over a fading hit");
        assert!(!is_new_hit(0.6, 0.55), "the held peak falling");
        assert!(!is_new_hit(0.5, 0.52), "a wobble");
        assert!(!is_new_hit(0.0, 0.005), "too quiet to be a hit");
    }

    #[test]
    fn only_audio_files_are_taken() {
        assert!(is_audio_file(Path::new("C:/kit/Kick.WAV")));
        assert!(is_audio_file(Path::new("snare.flac")));
        assert!(!is_audio_file(Path::new("notes.txt")));
        assert!(!is_audio_file(Path::new("noext")));
    }

    #[test]
    fn a_pad_edit_is_sent_as_its_changed_fields_only() {
        let current = drumsampler::default_params();
        let mut next = current.clone();
        next.pads[2].gain_db = -6.0;
        next.master_tune = 3.0;
        let gain = drumsampler::ui_param_index("pad2Gain").unwrap();
        assert_eq!(
            wire_diff(&current, &next),
            vec![(gain, -6.0), (drumsampler::ipc::MASTER_TUNE_INDEX, 3.0)]
        );
    }
}
