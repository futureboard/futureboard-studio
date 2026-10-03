//! Quick Sampler's native editor window.
//!
//! The editor of one Quick Sampler insert — a built-in plug-in that runs in
//! the plug-in host like every other — drawn natively with GPUI inside the
//! shared [`native_plugin_shell`] rather than as a web page in CEF.
//!
//! It talks to the plug-in the way the CEF editors do, through the host ops
//! `plugin_ops.rs` injects:
//!
//! * every edit is a wire param, sent with `forward_param` — which also folds
//!   it into Studio's state mirror (what the project saves and a restarted
//!   host replays) and marks the project edited;
//! * a sample is copied into the plug-in's Samples folder and sent as bytes
//!   with `load_pad_sample` (slot 0); the host decodes it, hands it to the
//!   audio thread, and answers through [`QuickSamplerEditorWindow::notify_sample_result`];
//! * the keyboard plays through `preview_note`, the engine's plug-in preview.
//!
//! The window decodes the sample itself only to draw it, off the UI thread.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

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
use crate::components::quick_sampler_panel::{
    QuickSamplerCallbacks, QuickSamplerPanelState, SampleMarker, SampleSummary, quick_sampler_panel,
};
use quicksampler::{LoopMode, QuickSamplerParams};

/// Opens with the display, the four modules side by side and the keyboard
/// all in view. Narrower, the modules wrap and the body scrolls; the header
/// and keyboard stay put.
pub const QUICK_SAMPLER_WINDOW_WIDTH: f32 = 1_040.0;
pub const QUICK_SAMPLER_WINDOW_HEIGHT: f32 = 800.0;
pub const QUICK_SAMPLER_WINDOW_MIN_WIDTH: f32 = 680.0;
pub const QUICK_SAMPLER_WINDOW_MIN_HEIGHT: f32 = 540.0;

const PREVIEW_VELOCITY: u8 = 100;
/// Columns of waveform overview read from a decoded sample.
const WAVEFORM_BUCKETS: usize = 1_600;
/// File types the Load Sample dialog offers: what the host's decoder reads.
pub const SAMPLE_EXTENSIONS: [&str; 7] = ["wav", "flac", "mp3", "ogg", "aif", "aiff", "m4a"];

/// Decodes `bytes` (a file named `name`) and describes it for the display.
/// Background thread only.
fn describe_sample(name: &str, bytes: &[u8]) -> Result<SampleSummary, String> {
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
    Ok(SampleSummary {
        file_name: name.to_string(),
        sample_rate: sample.sample_rate(),
        channels: sample.channels(),
        seconds: sample.seconds(),
        peaks: Arc::new(sample.peaks(WAVEFORM_BUCKETS)),
    })
}

mod menu_command {
    pub const LOAD: &str = "load";
    pub const RESET_REGION: &str = "reset-region";
    pub const RESET_LOOP: &str = "reset-loop";
    pub const LOOP_OFF: &str = "loop-off";
    pub const LOOP_FORWARD: &str = "loop-forward";
    pub const LOOP_PINGPONG: &str = "loop-pingpong";
    pub const REVERSE: &str = "reverse";
    pub const NORMALIZE: &str = "normalize";
}

/// The waveform's right-click menu: load another file, put the markers
/// back, and how the region plays.
fn waveform_menu_entries(p: &QuickSamplerParams, has_sample: bool) -> Vec<ContextMenuEntry> {
    let item = |enabled: bool, label: &str, command: &str| {
        if enabled {
            ContextMenuEntry::item(label, command)
        } else {
            ContextMenuEntry::disabled_item(label, command)
        }
    };
    let defaults = QuickSamplerParams::default();
    let region_moved = p.start != defaults.start || p.end != defaults.end;
    let loop_moved = p.loop_start != defaults.loop_start || p.loop_end != defaults.loop_end;
    vec![
        ContextMenuEntry::Header("Sample".into()),
        ContextMenuEntry::item(
            if has_sample {
                "Replace Sample…"
            } else {
                "Load Sample…"
            },
            menu_command::LOAD,
        ),
        ContextMenuEntry::Separator,
        item(
            has_sample && region_moved,
            "Reset Start and End",
            menu_command::RESET_REGION,
        ),
        item(
            has_sample && loop_moved,
            "Reset Loop Points",
            menu_command::RESET_LOOP,
        ),
        ContextMenuEntry::Separator,
        ContextMenuEntry::checked_item(
            "No Loop",
            menu_command::LOOP_OFF,
            p.loop_mode == LoopMode::Off,
        ),
        ContextMenuEntry::checked_item(
            "Loop",
            menu_command::LOOP_FORWARD,
            p.loop_mode == LoopMode::Forward,
        ),
        ContextMenuEntry::checked_item(
            "Ping-pong Loop",
            menu_command::LOOP_PINGPONG,
            p.loop_mode == LoopMode::PingPong,
        ),
        ContextMenuEntry::Separator,
        ContextMenuEntry::checked_item("Reverse", menu_command::REVERSE, p.reverse),
        ContextMenuEntry::checked_item("Normalize", menu_command::NORMALIZE, p.normalize),
    ]
}

pub struct QuickSamplerEditorWindow {
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    focus_handle: FocusHandle,
    focused_once: bool,
    panel: QuickSamplerPanelState,
    /// The sample file the panel shows (or is reading).
    shown_sample: Option<String>,
    /// Bumped per read, so a slow read for a file since replaced cannot land
    /// over the current one.
    load_generation: u64,
    /// Where the waveform was last painted, so a pointer x maps to a
    /// position in the sample.
    waveform_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The waveform's open right-click menu, at this window-space point.
    menu: Option<(f32, f32)>,
    meter: ShellMeter,
    /// Bumped when the window closes, ending the meter timer.
    alive: Rc<Cell<bool>>,
}

impl QuickSamplerEditorWindow {
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
            panel: QuickSamplerPanelState::default(),
            shown_sample: None,
            load_generation: 0,
            waveform_bounds: Rc::new(Cell::new(None)),
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
        let params = crate::components::builtin_plugin_editor::builtin_quick_sampler_params(
            &self.key.insert_id,
        )
        .unwrap_or_default();
        if self.panel.dragging.is_none() {
            self.panel.params = params.sampler;
        }
        if params.sample_name != self.shown_sample {
            self.show_stored_sample(params.sample_name, cx);
        }
        cx.notify();
    }

    /// Rebinds the window to another insert (the same plug-in opened from
    /// elsewhere), keeping the OS window.
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
            self.panel.dragging = None;
        }
        self.identity = identity;
        self.host_ops = host_ops;
        self.sync_from_mirror(cx);
    }

    fn samples_dir(&self) -> PathBuf {
        files::plugin_files_root(quicksampler::PLUGIN_NAME)
            .join(BuiltinFileKind::Samples.dir_name())
    }

    /// Reads a sample already in the Samples folder, only to draw it.
    fn show_stored_sample(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        self.shown_sample = name.clone();
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

    fn apply_summary(&mut self, result: Result<SampleSummary, String>) {
        self.panel.loading = false;
        match result {
            Ok(sample) => {
                self.panel.sample = Some(sample);
                self.panel.status = None;
            }
            Err(error) => {
                self.panel.sample = None;
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
            self.panel.status = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    /// Copies `source` into the plug-in's Samples folder, sends it to the
    /// host, and draws it. The play region and loop start over on the new
    /// file; how it plays (pitch, envelope, filter, output) is kept, so
    /// swapping one kick for another keeps the shaping.
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
                    let summary = describe_sample(&name, &bytes)?;
                    Ok::<_, String>((name, bytes, summary))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                match result {
                    Ok((name, bytes, summary)) => this.adopt_sample(name, bytes, summary, cx),
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
        summary: SampleSummary,
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
        self.apply_summary(Ok(summary));
        let defaults = QuickSamplerParams::default();
        let mut next = self.panel.params;
        next.start = defaults.start;
        next.end = defaults.end;
        next.loop_start = defaults.loop_start;
        next.loop_end = defaults.loop_end;
        self.set_params(next, cx);
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
    fn set_params(&mut self, next: QuickSamplerParams, cx: &mut Context<Self>) {
        let next = next.sanitized();
        let edits = quicksampler::ipc::wire_diff(&self.panel.params, &next);
        self.panel.params = next;
        if let Some(forward) = self.host_ops.forward_param.clone() {
            for (index, value) in edits {
                forward(&self.key, index, value, cx);
            }
        }
        cx.notify();
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        self.menu = None;
        let p = self.panel.params;
        let defaults = QuickSamplerParams::default();
        let next = match command {
            menu_command::LOAD => {
                self.browse_sample(cx);
                None
            }
            menu_command::RESET_REGION => Some(QuickSamplerParams {
                start: defaults.start,
                end: defaults.end,
                ..p
            }),
            menu_command::RESET_LOOP => Some(QuickSamplerParams {
                loop_start: defaults.loop_start,
                loop_end: defaults.loop_end,
                ..p
            }),
            menu_command::LOOP_OFF => Some(QuickSamplerParams {
                loop_mode: LoopMode::Off,
                ..p
            }),
            menu_command::LOOP_FORWARD => Some(QuickSamplerParams {
                loop_mode: LoopMode::Forward,
                ..p
            }),
            menu_command::LOOP_PINGPONG => Some(QuickSamplerParams {
                loop_mode: LoopMode::PingPong,
                ..p
            }),
            menu_command::REVERSE => Some(QuickSamplerParams {
                reverse: !p.reverse,
                ..p
            }),
            menu_command::NORMALIZE => Some(QuickSamplerParams {
                normalize: !p.normalize,
                ..p
            }),
            _ => None,
        };
        if let Some(next) = next {
            self.set_params(next, cx);
        }
        cx.notify();
    }

    fn fraction_at(&self, x: f32) -> Option<f32> {
        let bounds = self.waveform_bounds.get()?;
        let left = f32::from(bounds.origin.x);
        let width = f32::from(bounds.size.width).max(1.0);
        Some(((x - left) / width).clamp(0.0, 1.0))
    }

    fn press_waveform(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(fraction) = self.fraction_at(x) else {
            return;
        };
        self.panel.dragging = Some(SampleMarker::nearest(&self.panel.params, fraction));
        self.drag_marker_to(x, cx);
    }

    fn drag_marker_to(&mut self, x: f32, cx: &mut Context<Self>) {
        let (Some(marker), Some(fraction)) = (self.panel.dragging, self.fraction_at(x)) else {
            return;
        };
        let next = marker.moved(self.panel.params, fraction);
        if next != self.panel.params {
            self.set_params(next, cx);
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.panel.dragging.take().is_some() {
            cx.notify();
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
                    if this.meter.poll(this.host_ops.meter_source.as_ref(), &key) {
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

    fn callbacks(&self, cx: &mut Context<Self>) -> QuickSamplerCallbacks {
        let entity = cx.entity().clone();
        fn with<T: Clone + 'static>(
            entity: &gpui::Entity<QuickSamplerEditorWindow>,
            f: impl Fn(&mut QuickSamplerEditorWindow, T, &mut Context<QuickSamplerEditorWindow>)
            + 'static,
        ) -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |value: &T, _window, app: &mut App| {
                let value = value.clone();
                let _ = entity.update(app, |this, cx| f(this, value, cx));
            })
        }
        fn void(
            entity: &gpui::Entity<QuickSamplerEditorWindow>,
            f: impl Fn(&mut QuickSamplerEditorWindow, &mut Context<QuickSamplerEditorWindow>) + 'static,
        ) -> Arc<dyn Fn(&mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |_window, app: &mut App| {
                let _ = entity.update(app, |this, cx| f(this, cx));
            })
        }
        QuickSamplerCallbacks {
            on_browse: void(&entity, |this, cx| this.browse_sample(cx)),
            on_set_params: with(&entity, |this, params: QuickSamplerParams, cx| {
                this.set_params(params, cx)
            }),
            on_waveform_press: with(&entity, |this, x: f32, cx| this.press_waveform(x, cx)),
            on_waveform_menu: with(&entity, |this, point: (f32, f32), cx| {
                this.menu = Some(point);
                cx.notify();
            }),
            on_note_on: with(&entity, |this, pitch: u8, cx| this.note_on(pitch, cx)),
            on_note_off: with(&entity, |this, pitch: u8, cx| this.note_off(pitch, cx)),
            on_all_notes_off: void(&entity, |this, cx| {
                this.release_all(cx);
                cx.notify();
            }),
            on_shift_octave: with(&entity, |this, delta: i32, cx| {
                this.release_all(cx);
                this.panel.shift_keyboard_octave(delta);
                cx.notify();
            }),
        }
    }
}

impl NativeBuiltinEditor for QuickSamplerEditorWindow {
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

impl Render for QuickSamplerEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let entity = cx.entity().clone();
        let on_close = self.on_close.clone();
        let alive = self.alive.clone();
        let content = quick_sampler_panel(
            &self.panel,
            self.callbacks(cx),
            self.waveform_bounds.clone(),
        );

        let menu = self.menu.map(|(x, y)| {
            let viewport = window.viewport_size();
            let command_target = entity.clone();
            let close_target = entity.clone();
            context_menu_overlay(
                waveform_menu_entries(&self.panel.params, self.panel.sample.is_some()),
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
            // A marker drag follows the pointer anywhere in the window and
            // ends wherever the button comes up.
            .on_mouse_move({
                let entity = entity.clone();
                move |event: &gpui::MouseMoveEvent, _window, cx| {
                    if event.pressed_button == Some(gpui::MouseButton::Left) {
                        let x = f32::from(event.position.x);
                        let _ = entity.update(cx, |this, cx| this.drag_marker_to(x, cx));
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
                "quick-sampler-window-close",
                &self.identity,
                self.meter,
                move |window, cx| {
                    // Closing must not leave an auditioned note held — nothing
                    // would ever send its note-off.
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

pub fn open_quick_sampler_editor(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    key: PluginInstanceKey,
    identity: ShellIdentity,
    host_ops: BuiltinEditorHostOps,
    on_close: Arc<dyn Fn(&mut Window, &mut App)>,
    cx: &mut App,
) -> Result<WindowHandle<QuickSamplerEditorWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(
            px(QUICK_SAMPLER_WINDOW_WIDTH),
            px(QUICK_SAMPLER_WINDOW_HEIGHT),
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
        px(QUICK_SAMPLER_WINDOW_MIN_WIDTH),
        px(QUICK_SAMPLER_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| QuickSamplerEditorWindow::new(key, identity, host_ops, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_waveform_menu_offers_resets_only_when_something_moved() {
        let enabled = |entries: Vec<ContextMenuEntry>, wanted: &str| {
            entries.into_iter().find_map(|entry| match entry {
                ContextMenuEntry::Item {
                    command, disabled, ..
                } if command == wanted => Some(!disabled),
                _ => None,
            })
        };
        let p = QuickSamplerParams::default();
        assert_eq!(
            enabled(waveform_menu_entries(&p, true), menu_command::RESET_REGION),
            Some(false)
        );
        let trimmed = QuickSamplerParams { start: 0.2, ..p };
        assert_eq!(
            enabled(
                waveform_menu_entries(&trimmed, true),
                menu_command::RESET_REGION
            ),
            Some(true)
        );
        assert_eq!(
            enabled(
                waveform_menu_entries(&trimmed, false),
                menu_command::RESET_REGION
            ),
            Some(false),
            "nothing to reset without a sample"
        );
    }

    #[test]
    fn a_missing_or_empty_file_is_reported_not_drawn() {
        assert!(describe_sample("nothing.wav", &[]).is_err());
    }
}
