//! The egui application: screens, controls, overlays and shortcuts around a [`Player`].

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Key, Layout,
    Margin, Modifiers, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Ui,
    ViewportCommand,
};
use mkff_player::time::format_time;
use mkff_player::{AudioOutput, MediaFile, Picture, PlaybackState, Player, PlayerOptions, SPEEDS};
use serde::{Deserialize, Serialize};

use crate::renderer;
use crate::widgets::{self, Icon, ACCENT, FG, FG_DIM};

const SETTINGS_KEY: &str = "mkff-player-settings";
const HIDE_CONTROLS_AFTER: Duration = Duration::from_millis(2500);
const OSD_DURATION: Duration = Duration::from_millis(1200);
const TOAST_DURATION: Duration = Duration::from_secs(6);
const MAX_RECENT: usize = 10;
/// A file dialog that "returns" faster than this was never shown (no portal / zenity).
const NO_DIALOG: Duration = Duration::from_millis(300);

enum Picked {
    File(PathBuf),
    Cancelled,
    Unavailable,
}

/// Persisted between runs.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    volume: f32,
    muted: bool,
    looping: bool,
    show_stats: bool,
    recent: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            volume: 0.8,
            muted: false,
            looping: false,
            show_stats: false,
            recent: Vec::new(),
        }
    }
}

/// Command-line choices that override the settings for this run.
#[derive(Clone, Default)]
pub struct Startup {
    pub file: Option<PathBuf>,
    pub start: f64,
    pub volume: Option<f32>,
    pub muted: bool,
    pub looping: bool,
    pub paused: bool,
    pub fullscreen: bool,
    pub speed: f64,
    pub audio: AudioOutput,
}

struct Loading {
    path: PathBuf,
    rx: Receiver<Result<MediaFile, String>>,
    since: Instant,
    start: f64,
    autoplay: bool,
}

struct Toast {
    text: String,
    error: bool,
    at: Instant,
}

struct Scrub {
    was_playing: bool,
    last_seek: Option<f64>,
}

pub struct PlayerApp {
    /// Recent repaint times (UI frame rate in the statistics).
    paints: std::collections::VecDeque<Instant>,
    settings: Settings,
    startup: Startup,
    player: Option<Player>,
    path: Option<PathBuf>,
    loading: Option<Loading>,
    open_dialog: Option<Receiver<Picked>>,
    snapshot_rx: Option<Receiver<Result<PathBuf, String>>>,
    toasts: Vec<Toast>,
    osd: Option<(String, Instant)>,
    last_activity: Instant,
    last_pointer: Option<egui::Pos2>,
    scrub: Option<Scrub>,
    show_help: bool,
    speed: f64,
    gpu: bool,
    renderer_name: String,
    /// CPU path (no wgpu render state): the converted texture and its picture serial.
    cpu_texture: Option<(TextureHandle, u64)>,
    waker: Arc<dyn Fn() + Send + Sync>,
    first_frame: bool,
}

impl PlayerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Startup) -> Self {
        apply_theme(&cc.egui_ctx);
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();
        if let Some(v) = startup.volume {
            settings.volume = v;
        }
        if startup.muted {
            settings.muted = true;
        }
        if startup.looping {
            settings.looping = true;
        }
        let (gpu, renderer_name) = match &cc.wgpu_render_state {
            Some(rs) => {
                renderer::init(rs);
                (true, renderer::adapter_summary(rs))
            }
            None => (false, "CPU (egui texture)".into()),
        };
        log::info!("renderer: {renderer_name}");
        let ctx = cc.egui_ctx.clone();
        let mut app = PlayerApp {
            paints: Default::default(),
            settings,
            speed: if startup.speed > 0.0 {
                startup.speed
            } else {
                1.0
            },
            startup: startup.clone(),
            player: None,
            path: None,
            loading: None,
            open_dialog: None,
            snapshot_rx: None,
            toasts: Vec::new(),
            osd: None,
            last_activity: Instant::now(),
            last_pointer: None,
            scrub: None,
            show_help: false,
            gpu,
            renderer_name,
            cpu_texture: None,
            waker: Arc::new(move || ctx.request_repaint()),
            first_frame: true,
        };
        if let Some(file) = startup.file {
            app.open(&cc.egui_ctx, file, startup.start, !startup.paused);
        }
        app
    }

    // ----- opening ------------------------------------------------------------------------

    fn open(&mut self, ctx: &egui::Context, path: PathBuf, start: f64, autoplay: bool) {
        // stop the current file first: frees its decoder threads and audio device
        self.player = None;
        self.cpu_texture = None;
        self.scrub = None;
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let ctx2 = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("mkff-open".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(|| mkff_player::open_media(&p))
                    .unwrap_or_else(|_| Err("the demuxer panicked on this file".into()));
                let _ = tx.send(r);
                ctx2.request_repaint();
            });
        if let Err(e) = spawned {
            self.toast(format!("Could not open {}: {e}", path.display()), true);
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::Title(format!(
            "{} — MKFF Player",
            file_name(&path)
        )));
        self.loading = Some(Loading {
            path,
            rx,
            since: Instant::now(),
            start,
            autoplay,
        });
    }

    fn poll_loading(&mut self, ctx: &egui::Context) {
        let Some(loading) = &self.loading else { return };
        let result = match loading.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the open thread stopped".into()),
        };
        let loading = self.loading.take().expect("checked above");
        match result {
            Ok(media) => {
                let opts = PlayerOptions {
                    audio: self.startup.audio,
                    volume: self.settings.volume,
                    muted: self.settings.muted,
                    looping: self.settings.looping,
                    speed: self.speed,
                    start: loading.start,
                    autoplay: loading.autoplay,
                    waker: Some(Arc::clone(&self.waker)),
                    ..PlayerOptions::default()
                };
                let mut player = Player::new(media, opts);
                // half a 60 Hz refresh
                player.set_present_ahead(0.008);
                log::info!(
                    "opened {} in {:.0} ms: {:?}",
                    loading.path.display(),
                    loading.since.elapsed().as_secs_f64() * 1000.0,
                    player.info()
                );
                self.player = Some(player);
                self.remember(&loading.path);
                self.path = Some(loading.path);
                self.last_activity = Instant::now();
            }
            Err(e) => {
                log::error!("open {}: {e}", loading.path.display());
                self.toast(
                    format!("Could not open {}: {e}", file_name(&loading.path)),
                    true,
                );
                self.path = None;
                ctx.send_viewport_cmd(ViewportCommand::Title("MKFF Player".into()));
            }
        }
    }

    fn remember(&mut self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.settings.recent.retain(|p| p != &path);
        self.settings.recent.insert(0, path);
        self.settings.recent.truncate(MAX_RECENT);
    }

    fn show_open_dialog(&mut self) {
        if self.open_dialog.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let waker = Arc::clone(&self.waker);
        let dir = self
            .path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let exts: Vec<&'static str> = mkff_codecs::supported_extensions().collect();
        let spawned = std::thread::Builder::new()
            .name("mkff-open-dialog".into())
            .spawn(move || {
                let mut dialog = rfd::AsyncFileDialog::new()
                    .set_title("Open media")
                    .add_filter("Media files", &exts)
                    .add_filter("All files", &["*"]);
                if let Some(dir) = dir {
                    dialog = dialog.set_directory(dir);
                }
                let t0 = Instant::now();
                let picked = pollster::block_on(dialog.pick_file());
                log::debug!("open dialog returned after {:?}", t0.elapsed());
                let picked = match picked {
                    Some(f) => Picked::File(f.path().to_path_buf()),
                    None if t0.elapsed() < NO_DIALOG => Picked::Unavailable,
                    None => Picked::Cancelled,
                };
                let _ = tx.send(picked);
                waker();
            });
        match spawned {
            Ok(_) => self.open_dialog = Some(rx),
            Err(e) => self.toast(format!("File dialog: {e}"), true),
        }
    }

    fn poll_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.open_dialog {
            match rx.try_recv() {
                Ok(Picked::File(path)) => {
                    self.open_dialog = None;
                    self.open(ctx, path, 0.0, true);
                }
                Ok(Picked::Unavailable) => {
                    self.open_dialog = None;
                    self.toast(
                        "No file dialog on this system (it needs xdg-desktop-portal or zenity). \
                         Drop a file onto the window or pass it on the command line."
                            .into(),
                        true,
                    );
                }
                Ok(Picked::Cancelled) | Err(TryRecvError::Disconnected) => self.open_dialog = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.snapshot_rx {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.snapshot_rx = None;
                    self.toast(format!("Saved {}", path.display()), false);
                }
                Ok(Err(e)) => {
                    self.snapshot_rx = None;
                    if !e.is_empty() {
                        self.toast(format!("Snapshot: {e}"), true);
                    }
                }
                Err(TryRecvError::Disconnected) => self.snapshot_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    /// Save the picture on screen as PNG (asks where).
    fn snapshot(&mut self) {
        let Some(player) = &self.player else { return };
        let Some(pic) = player.current_picture().cloned() else {
            self.osd("No picture to save");
            return;
        };
        if self.snapshot_rx.is_some() {
            return;
        }
        let stem = self
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "snapshot".into());
        let name = format!(
            "{stem}-{}.png",
            format_time(pic.pts, true).replace([':', '.'], "-")
        );
        let dir = self
            .path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .filter(|d| !d.as_os_str().is_empty());
        let waker = Arc::clone(&self.waker);
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("mkff-snapshot".into())
            .spawn(move || {
                let mut dialog = rfd::AsyncFileDialog::new()
                    .set_title("Save snapshot")
                    .set_file_name(&name)
                    .add_filter("PNG image", &["png"]);
                if let Some(dir) = &dir {
                    dialog = dialog.set_directory(dir);
                }
                let t0 = Instant::now();
                let result = match pollster::block_on(dialog.save_file()) {
                    Some(f) => {
                        let path = f.path().to_path_buf();
                        write_png(&path, &pic).map(|_| path)
                    }
                    // no dialog here: save next to the video (or in the working directory)
                    None if t0.elapsed() < NO_DIALOG => {
                        let path = dir.unwrap_or_else(|| PathBuf::from(".")).join(&name);
                        write_png(&path, &pic).map(|_| path)
                    }
                    None => Err(String::new()),
                };
                let _ = tx.send(result);
                waker();
            });
        if spawned.is_ok() {
            self.snapshot_rx = Some(rx);
        }
    }

    // ----- feedback -----------------------------------------------------------------------

    fn toast(&mut self, text: String, error: bool) {
        self.toasts.push(Toast {
            text,
            error,
            at: Instant::now(),
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    fn osd(&mut self, text: impl Into<String>) {
        self.osd = Some((text.into(), Instant::now()));
    }

    fn activity(&mut self) {
        self.last_activity = Instant::now();
    }

    // ----- commands -----------------------------------------------------------------------

    fn toggle_play(&mut self) {
        if let Some(p) = &mut self.player {
            p.toggle_play();
            let label = if p.is_playing() { "Play" } else { "Pause" };
            self.osd(label);
        }
    }

    fn seek_by(&mut self, delta: f64) {
        if let Some(p) = &mut self.player {
            p.seek_relative(delta);
            let sign = if delta >= 0.0 { "+" } else { "−" };
            let text = format!(
                "{sign}{} s   {} / {}",
                delta.abs(),
                format_time(p.position(), false),
                format_time(p.duration(), false)
            );
            self.osd(text);
        }
    }

    fn seek_to_fraction(&mut self, frac: f64) {
        if let Some(p) = &mut self.player {
            p.seek(p.duration() * frac);
            let text = format_time(p.position(), false);
            self.osd(text);
        }
    }

    fn change_volume(&mut self, delta: f32) {
        let v = (self.settings.volume + delta).clamp(0.0, 1.0);
        self.set_volume(v);
        self.osd(format!("Volume {:.0} %", v * 100.0));
    }

    fn set_volume(&mut self, v: f32) {
        self.settings.volume = v;
        self.settings.muted = false;
        if let Some(p) = &mut self.player {
            p.set_volume(v);
            p.set_muted(false);
        }
    }

    fn toggle_mute(&mut self) {
        self.settings.muted = !self.settings.muted;
        if let Some(p) = &mut self.player {
            p.set_muted(self.settings.muted);
        }
        self.osd(if self.settings.muted {
            "Muted"
        } else {
            "Unmuted"
        });
    }

    fn set_speed(&mut self, speed: f64) {
        self.speed = speed;
        if let Some(p) = &mut self.player {
            p.set_speed(speed);
        }
        self.osd(format!("Speed {}", speed_label(speed)));
    }

    fn step_speed(&mut self, up: bool) {
        let i = SPEEDS
            .iter()
            .position(|&s| (s - self.speed).abs() < 1e-6)
            .unwrap_or(3);
        let j = if up {
            (i + 1).min(SPEEDS.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.set_speed(SPEEDS[j]);
    }

    fn toggle_loop(&mut self) {
        self.settings.looping = !self.settings.looping;
        if let Some(p) = &mut self.player {
            p.set_looping(self.settings.looping);
        }
        self.osd(if self.settings.looping {
            "Loop on"
        } else {
            "Loop off"
        });
    }

    fn step_frame(&mut self, forward: bool) {
        if let Some(p) = &mut self.player {
            p.step(forward);
            let text = match p.current_picture() {
                Some(pic) => format!("Frame {}", pic.index + 1),
                None => "Paused".into(),
            };
            self.osd(text);
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!full));
    }

    fn close_file(&mut self, ctx: &egui::Context) {
        self.player = None;
        self.path = None;
        self.loading = None;
        self.cpu_texture = None;
        ctx.send_viewport_cmd(ViewportCommand::Title("MKFF Player".into()));
    }

    // ----- input --------------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let pressed = |key: Key, mods: Modifiers| ctx.input_mut(|i| i.consume_key(mods, key));
        let none = Modifiers::NONE;
        let cmd = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;

        if pressed(Key::Q, cmd) || pressed(Key::W, cmd) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if pressed(Key::O, cmd) || pressed(Key::O, none) {
            self.show_open_dialog();
        }
        if pressed(Key::F1, none) || pressed(Key::H, none) {
            self.show_help = !self.show_help;
        }
        if pressed(Key::I, none) {
            self.settings.show_stats = !self.settings.show_stats;
        }
        if pressed(Key::F, none) || pressed(Key::F11, none) || pressed(Key::Enter, Modifiers::ALT) {
            self.toggle_fullscreen(ctx);
        }
        if pressed(Key::Escape, none) {
            if self.show_help {
                self.show_help = false;
            } else if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            }
        }
        if self.player.is_none() {
            return;
        }
        let mut any = false;
        let mut key = |k: Key, m: Modifiers| {
            let hit = pressed(k, m);
            any |= hit;
            hit
        };
        if key(Key::Space, none) || key(Key::K, none) {
            self.toggle_play();
        }
        if key(Key::ArrowLeft, none) {
            self.seek_by(-5.0);
        }
        if key(Key::ArrowRight, none) {
            self.seek_by(5.0);
        }
        if key(Key::ArrowLeft, shift) {
            self.seek_by(-1.0);
        }
        if key(Key::ArrowRight, shift) {
            self.seek_by(1.0);
        }
        if key(Key::ArrowLeft, cmd) {
            self.seek_by(-60.0);
        }
        if key(Key::ArrowRight, cmd) {
            self.seek_by(60.0);
        }
        if key(Key::J, none) {
            self.seek_by(-10.0);
        }
        if key(Key::L, none) {
            self.seek_by(10.0);
        }
        if key(Key::ArrowUp, none) {
            self.change_volume(0.05);
        }
        if key(Key::ArrowDown, none) {
            self.change_volume(-0.05);
        }
        if key(Key::M, none) {
            self.toggle_mute();
        }
        if key(Key::Comma, none) {
            self.step_frame(false);
        }
        if key(Key::Period, none) {
            self.step_frame(true);
        }
        if key(Key::OpenBracket, none) {
            self.step_speed(false);
        }
        if key(Key::CloseBracket, none) {
            self.step_speed(true);
        }
        if key(Key::Backspace, none) || key(Key::Equals, none) {
            self.set_speed(1.0);
        }
        if key(Key::R, none) {
            self.toggle_loop();
        }
        if key(Key::S, cmd) || key(Key::S, none) {
            self.snapshot();
        }
        if key(Key::Home, none) {
            self.seek_to_fraction(0.0);
        }
        if key(Key::End, none) {
            if let Some(p) = &mut self.player {
                p.seek(p.duration());
            }
        }
        let digits = [
            Key::Num0,
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        for (n, k) in digits.into_iter().enumerate() {
            if key(k, none) {
                self.seek_to_fraction(n as f64 / 10.0);
            }
        }
        if any {
            self.activity();
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(path) = dropped.into_iter().find_map(|f| f.path) {
            self.open(ctx, path, 0.0, true);
        }
    }

    // ----- drawing ------------------------------------------------------------------------

    fn draw_video(&mut self, ui: &mut Ui, rect: Rect) {
        let Some(player) = &self.player else { return };
        let Some(pic) = player.current_picture().cloned() else {
            if player.has_video() {
                ui.put(rect, egui::Spinner::new().size(36.0).color(FG_DIM));
            } else {
                self.draw_audio_only(ui, rect);
            }
            return;
        };
        let aspect = pic.display_aspect().max(0.01);
        let fit = if rect.width() / rect.height().max(1.0) > aspect {
            vec2(rect.height() * aspect, rect.height())
        } else {
            vec2(rect.width(), rect.width() / aspect)
        };
        // snap to whole pixels: no blurry half-texel edges
        let ppp = ui.ctx().pixels_per_point();
        let fit = vec2((fit.x * ppp).round() / ppp, (fit.y * ppp).round() / ppp);
        let center = pos2(
            (rect.center().x * ppp).round() / ppp,
            (rect.center().y * ppp).round() / ppp,
        );
        let image_rect = Rect::from_center_size(center, fit);
        if self.gpu {
            ui.painter()
                .add(eframe::egui_wgpu::Callback::new_paint_callback(
                    image_rect,
                    renderer::VideoCallback { picture: pic },
                ));
        } else {
            let tex = match &mut self.cpu_texture {
                Some((tex, serial)) if *serial == pic.serial => tex.clone(),
                slot => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [pic.width as usize, pic.height as usize],
                        &pic.to_rgba8(),
                    );
                    let tex = match slot.take() {
                        Some((mut tex, _)) => {
                            tex.set(image, TextureOptions::LINEAR);
                            tex
                        }
                        None => ui
                            .ctx()
                            .load_texture("mkff-video", image, TextureOptions::LINEAR),
                    };
                    *slot = Some((tex.clone(), pic.serial));
                    tex
                }
            };
            ui.painter().image(
                tex.id(),
                image_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }

    fn draw_audio_only(&self, ui: &mut Ui, rect: Rect) {
        let Some(player) = &self.player else { return };
        let p = ui.painter();
        let c = rect.center();
        // a slowly pulsing disc while playing
        let t = player.position() as f32;
        let pulse = if player.is_playing() {
            1.0 + 0.04 * (t * 4.0).sin()
        } else {
            1.0
        };
        p.circle_filled(
            c - vec2(0.0, 30.0),
            64.0 * pulse,
            Color32::from_rgb(32, 36, 48),
        );
        widgets::paint_icon(
            p,
            Icon::Volume(2),
            Rect::from_center_size(c - vec2(0.0, 30.0), vec2(56.0, 56.0)),
            ACCENT,
        );
        let a = player.info().audio.first();
        let line = match a {
            Some(a) => format!("{} · {} Hz · {} ch", a.codec, a.sample_rate, a.channels),
            None => String::new(),
        };
        p.text(
            c + vec2(0.0, 58.0),
            Align2::CENTER_CENTER,
            self.path.as_deref().map(file_name).unwrap_or_default(),
            FontId::proportional(20.0),
            FG,
        );
        p.text(
            c + vec2(0.0, 84.0),
            Align2::CENTER_CENTER,
            line,
            FontId::proportional(14.0),
            FG_DIM,
        );
    }

    fn draw_welcome(&mut self, ui: &mut Ui, rect: Rect) {
        let mut open_recent = None;
        let mut open_dialog = false;
        let width = 420.0f32.min(rect.width() - 32.0);
        let content = Rect::from_center_size(rect.center(), vec2(width, rect.height().min(560.0)));
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(content), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                let (r, _) = ui.allocate_exact_size(vec2(72.0, 72.0), Sense::hover());
                ui.painter()
                    .rect_filled(r, CornerRadius::same(18), Color32::from_rgb(30, 34, 46));
                widgets::paint_icon(ui.painter(), Icon::Play, r.shrink(20.0), ACCENT);
                ui.add_space(12.0);
                ui.label(RichText::new("MKFF Player").size(28.0).strong().color(FG));
                ui.label(
                    RichText::new("Any container, any codec — pure-Rust decode, no FFmpeg")
                        .size(13.0)
                        .color(FG_DIM),
                );
                ui.add_space(20.0);
                let open =
                    egui::Button::new(RichText::new("Open file…").size(16.0).color(Color32::WHITE))
                        .fill(ACCENT)
                        .corner_radius(CornerRadius::same(8))
                        .min_size(vec2(180.0, 40.0));
                if ui.add(open).clicked() {
                    open_dialog = true;
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new("or drop a file anywhere · Ctrl+O")
                        .size(12.0)
                        .color(FG_DIM),
                );

                if !self.settings.recent.is_empty() {
                    ui.add_space(24.0);
                    ui.label(RichText::new("Recent").size(13.0).color(FG_DIM));
                    ui.add_space(4.0);
                    for path in self.settings.recent.iter().take(6) {
                        let exists = path.exists();
                        let text = RichText::new(file_name(path)).size(14.0).color(if exists {
                            FG
                        } else {
                            FG_DIM
                        });
                        let resp = ui
                            .add_enabled(exists, egui::Button::new(text).frame(false))
                            .on_hover_text(path.display().to_string());
                        if resp.clicked() {
                            open_recent = Some(path.clone());
                        }
                    }
                }
            });
        });
        if open_dialog {
            self.show_open_dialog();
        }
        if let Some(path) = open_recent {
            self.open(ui.ctx(), path, 0.0, true);
        }
    }

    fn draw_loading(&self, ui: &mut Ui, rect: Rect) {
        let Some(l) = &self.loading else { return };
        ui.put(
            Rect::from_center_size(rect.center() - vec2(0.0, 20.0), vec2(40.0, 40.0)),
            egui::Spinner::new().size(36.0).color(ACCENT),
        );
        ui.painter().text(
            rect.center() + vec2(0.0, 24.0),
            Align2::CENTER_CENTER,
            format!("Opening {}…", file_name(&l.path)),
            FontId::proportional(15.0),
            FG_DIM,
        );
    }

    /// The bottom control bar. Returns its rect (for hover detection).
    fn draw_controls(&mut self, ctx: &egui::Context, screen: Rect, alpha: f32) -> Option<Rect> {
        let player = self.player.as_ref()?;
        let position = player.position();
        let duration = player.duration();
        let state = player.state();
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));

        let bar_h = 92.0;
        let bar = Rect::from_min_max(pos2(screen.left(), screen.bottom() - bar_h), screen.max);
        // gradient backing so controls read on bright video
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("mkff-controls-bg"),
        ));
        let steps = 12;
        for i in 0..steps {
            let f = i as f32 / steps as f32;
            let y0 = bar.top() - 30.0 + (bar.height() + 30.0) * f;
            let y1 = bar.top() - 30.0 + (bar.height() + 30.0) * (f + 1.0 / steps as f32);
            let a = (f * 200.0 * alpha) as u8;
            painter.rect_filled(
                Rect::from_min_max(pos2(bar.left(), y0), pos2(bar.right(), y1)),
                CornerRadius::ZERO,
                Color32::from_black_alpha(a),
            );
        }

        let mut seek = widgets::SeekOutcome::default();
        let mut actions: Vec<Action> = Vec::new();
        let volume = self.settings.volume;
        let muted = self.settings.muted;
        let looping = self.settings.looping;
        let show_stats = self.settings.show_stats;
        let speed = self.speed;

        egui::Area::new(egui::Id::new("mkff-controls"))
            .order(egui::Order::Foreground)
            .fixed_pos(bar.min)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                ui.set_width(bar.width());
                egui::Frame::NONE
                    .inner_margin(Margin::symmetric(16, 8))
                    .show(ui, |ui| {
                        ui.set_width(bar.width() - 32.0);
                        seek = widgets::seek_bar(ui, position, duration);
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let s = 36.0;
                            let (icon, tip) = match state {
                                PlaybackState::Playing => (Icon::Pause, "Pause (Space)"),
                                PlaybackState::Paused => (Icon::Play, "Play (Space)"),
                                PlaybackState::Ended => (Icon::Replay, "Replay (Space)"),
                            };
                            if widgets::icon_button(ui, icon, s, false, tip).clicked() {
                                actions.push(Action::TogglePlay);
                            }
                            if widgets::icon_button(ui, Icon::Back, s, false, "Back 10 s (J)")
                                .clicked()
                            {
                                actions.push(Action::Seek(-10.0));
                            }
                            if widgets::icon_button(ui, Icon::Forward, s, false, "Forward 10 s (L)")
                                .clicked()
                            {
                                actions.push(Action::Seek(10.0));
                            }
                            let vicon = if muted || volume == 0.0 {
                                Icon::Muted
                            } else if volume < 0.5 {
                                Icon::Volume(1)
                            } else {
                                Icon::Volume(2)
                            };
                            if widgets::icon_button(ui, vicon, s, false, "Mute (M)").clicked() {
                                actions.push(Action::ToggleMute);
                            }
                            if let Some(v) =
                                widgets::volume_slider(ui, if muted { 0.0 } else { volume }, 90.0)
                            {
                                actions.push(Action::Volume(v));
                            }
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(format!(
                                    "{} / {}",
                                    format_time(seek.scrub_to.unwrap_or(position), false),
                                    format_time(duration, false)
                                ))
                                .size(14.0)
                                .color(FG)
                                .monospace(),
                            );

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (ficon, ftip) = if fullscreen {
                                    (Icon::ExitFullscreen, "Exit full screen (F)")
                                } else {
                                    (Icon::Fullscreen, "Full screen (F)")
                                };
                                if widgets::icon_button(ui, ficon, s, false, ftip).clicked() {
                                    actions.push(Action::Fullscreen);
                                }
                                if widgets::icon_button(
                                    ui,
                                    Icon::Open,
                                    s,
                                    false,
                                    "Open file (Ctrl+O)",
                                )
                                .clicked()
                                {
                                    actions.push(Action::Open);
                                }
                                if widgets::icon_button(
                                    ui,
                                    Icon::Camera,
                                    s,
                                    false,
                                    "Save snapshot (S)",
                                )
                                .clicked()
                                {
                                    actions.push(Action::Snapshot);
                                }
                                if widgets::icon_button(
                                    ui,
                                    Icon::Info,
                                    s,
                                    show_stats,
                                    "Statistics (I)",
                                )
                                .clicked()
                                {
                                    actions.push(Action::Stats);
                                }
                                if widgets::icon_button(ui, Icon::Loop, s, looping, "Loop (R)")
                                    .clicked()
                                {
                                    actions.push(Action::Loop);
                                }
                                let label = RichText::new(speed_label(speed)).size(14.0).color(
                                    if (speed - 1.0).abs() > 1e-6 {
                                        ACCENT
                                    } else {
                                        FG
                                    },
                                );
                                ui.menu_button(label, |ui| {
                                    for s in SPEEDS {
                                        if ui
                                            .selectable_label(
                                                (s - speed).abs() < 1e-6,
                                                speed_label(s),
                                            )
                                            .clicked()
                                        {
                                            actions.push(Action::Speed(s));
                                            ui.close_menu();
                                        }
                                    }
                                })
                                .response
                                .on_hover_text("Playback speed ([ / ])");
                            });
                        });
                    });
            });

        // scrubbing: pause while the button is held, seek as the pointer moves
        if seek.started && self.scrub.is_none() {
            let was_playing = self.player.as_ref().is_some_and(|p| p.is_playing());
            if let Some(p) = &mut self.player {
                p.pause();
            }
            self.scrub = Some(Scrub {
                was_playing,
                last_seek: None,
            });
        }
        if let (Some(t), Some(scrub)) = (seek.scrub_to, &mut self.scrub) {
            if scrub.last_seek.is_none_or(|l| (l - t).abs() > 1e-3) {
                scrub.last_seek = Some(t);
                if let Some(p) = &mut self.player {
                    p.seek(t);
                }
            }
        }
        if let Some(t) = seek.released_at {
            if let Some(p) = &mut self.player {
                p.seek(t);
            }
            if let Some(scrub) = self.scrub.take() {
                if scrub.was_playing {
                    if let Some(p) = &mut self.player {
                        p.play();
                    }
                }
            }
        }
        if seek.scrub_to.is_none()
            && seek.released_at.is_none()
            && !ctx.input(|i| i.pointer.any_down())
        {
            // released outside the bar
            if let Some(scrub) = self.scrub.take() {
                if scrub.was_playing {
                    if let Some(p) = &mut self.player {
                        p.play();
                    }
                }
            }
        }

        for a in actions {
            self.activity();
            match a {
                Action::TogglePlay => self.toggle_play(),
                Action::Seek(d) => self.seek_by(d),
                Action::ToggleMute => self.toggle_mute(),
                Action::Volume(v) => self.set_volume(v),
                Action::Fullscreen => self.toggle_fullscreen(ctx),
                Action::Open => self.show_open_dialog(),
                Action::Snapshot => self.snapshot(),
                Action::Stats => self.settings.show_stats = !self.settings.show_stats,
                Action::Loop => self.toggle_loop(),
                Action::Speed(s) => self.set_speed(s),
            }
        }
        Some(bar.expand2(vec2(0.0, 30.0)))
    }

    fn draw_top_bar(&self, ctx: &egui::Context, screen: Rect, alpha: f32) {
        let Some(player) = &self.player else { return };
        let Some(path) = &self.path else { return };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("mkff-top"),
        ));
        let h = 56.0;
        for i in 0..8 {
            let f = i as f32 / 8.0;
            let a = ((1.0 - f) * 160.0 * alpha) as u8;
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(screen.left(), screen.top() + h * f),
                    pos2(screen.right(), screen.top() + h * (f + 0.125)),
                ),
                CornerRadius::ZERO,
                Color32::from_black_alpha(a),
            );
        }
        let fg = FG.gamma_multiply(alpha);
        painter.text(
            screen.left_top() + vec2(16.0, 14.0),
            Align2::LEFT_TOP,
            file_name(path),
            FontId::proportional(16.0),
            fg,
        );
        let info = player.info();
        let mut badges = Vec::new();
        if let Some(v) = &info.video {
            badges.push(format!("{}×{}", v.width, v.height));
            badges.push(
                format!("{:.3}", player.frame_rate())
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string()
                    + " fps",
            );
            badges.push(v.codec.clone());
        }
        if let Some(a) = info.audio.first() {
            badges.push(a.codec.to_uppercase());
        }
        painter.text(
            screen.right_top() + vec2(-16.0, 16.0),
            Align2::RIGHT_TOP,
            badges.join("  ·  "),
            FontId::proportional(13.0),
            FG_DIM.gamma_multiply(alpha),
        );
    }

    fn draw_stats(&self, ctx: &egui::Context) {
        let Some(player) = &self.player else { return };
        let s = player.stats();
        let info = player.info();
        let mut lines = Vec::new();
        let size = info
            .file_size
            .map(|b| format!(", {:.1} MB", b as f64 / 1e6))
            .unwrap_or_default();
        lines.push(format!(
            "File      {} ({}{size})",
            self.path.as_deref().map(file_name).unwrap_or_default(),
            info.container
        ));
        if let Some(v) = &info.video {
            let pic = player.current_picture();
            let color = pic
                .map(|p| {
                    let m = match p.matrix {
                        mkff_codecs::Matrix::Bt601 => "BT.601",
                        mkff_codecs::Matrix::Bt709 => "BT.709",
                        mkff_codecs::Matrix::Bt2020Ncl => "BT.2020",
                    };
                    let range = if p.full_range { "full" } else { "limited" };
                    format!(", {m} {range} range")
                })
                .unwrap_or_default();
            lines.push(format!(
                "Video     {} {}×{} @ {:.3} fps, {}{color}",
                v.codec,
                v.width,
                v.height,
                player.frame_rate(),
                v.pixel_format
            ));
            if let Some(br) = v.bitrate {
                lines.push(format!("          {:.0} kb/s", br as f64 / 1000.0));
            }
        }
        for (i, a) in info.audio.iter().enumerate() {
            lines.push(format!(
                "Audio #{i}  {} {} Hz, {} ch",
                a.codec, a.sample_rate, a.channels
            ));
        }
        match &s.audio_device {
            Some(dev) => lines.push(format!(
                "Output    {dev} · {} Hz · {} ch · underruns {} · errors {}",
                s.audio_rate, s.audio_channels, s.audio_underruns, s.audio_errors
            )),
            None => lines.push("Output    no audio".into()),
        }
        lines.push(format!(
            "Clock     {} · position {} · A−V {:+.0} ms",
            if s.audio_master {
                "audio master"
            } else {
                "system"
            },
            format_time(player.position(), true),
            s.av_offset * 1000.0
        ));
        if player.has_video() {
            lines.push(format!(
                "Frames    decoded {} · shown {} · dropped {} · skipped {} · errors {}",
                s.decoded, s.presented, s.dropped, s.skipped, s.video_errors
            ));
            lines.push(format!(
                "Decode    {:.2} ms/frame · queue {}",
                s.decode_ms, s.queued
            ));
        }
        lines.push(format!(
            "Renderer  {} · UI {} fps",
            self.renderer_name,
            self.paints.len()
        ));
        egui::Area::new(egui::Id::new("mkff-stats"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(12.0, 56.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(Color32::from_black_alpha(190))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        for l in lines {
                            ui.add(
                                egui::Label::new(RichText::new(l).monospace().size(12.0).color(FG))
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        }
                    });
            });
    }

    fn draw_help(&mut self, ctx: &egui::Context) {
        let rows = [
            ("Space / K", "Play / pause"),
            ("← / →", "Back / forward 5 s"),
            ("Shift + ← / →", "Back / forward 1 s"),
            ("Ctrl + ← / →", "Back / forward 1 min"),
            ("J / L", "Back / forward 10 s"),
            (", / .", "Previous / next frame"),
            ("Home / End, 0–9", "Start / end, jump to 0–90 %"),
            ("↑ / ↓, wheel", "Volume"),
            ("M", "Mute"),
            ("[ / ] / Backspace", "Slower / faster / normal speed"),
            ("R", "Loop"),
            ("F, F11, double-click", "Full screen"),
            ("S", "Save snapshot (PNG)"),
            ("I", "Statistics"),
            ("O / Ctrl + O, drop", "Open file"),
            ("H / F1", "This help"),
            ("Ctrl + Q", "Quit"),
        ];
        let mut open = self.show_help;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                egui::Grid::new("mkff-help")
                    .num_columns(2)
                    .spacing(vec2(24.0, 6.0))
                    .show(ui, |ui| {
                        for (k, v) in rows {
                            ui.label(RichText::new(k).monospace().color(ACCENT));
                            ui.label(v);
                            ui.end_row();
                        }
                    });
            });
        self.show_help = open;
    }

    fn draw_feedback(&mut self, ctx: &egui::Context, screen: Rect) {
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("mkff-osd"),
        ));
        if let Some((text, at)) = &self.osd {
            if at.elapsed() < OSD_DURATION {
                widgets::overlay_text(
                    &painter,
                    pos2(screen.center().x, screen.top() + screen.height() * 0.22),
                    Align2::CENTER_CENTER,
                    text,
                    18.0,
                );
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                self.osd = None;
            }
        }
        self.toasts.retain(|t| t.at.elapsed() < TOAST_DURATION);
        let mut y = screen.top() + 12.0;
        for t in &self.toasts {
            let galley = painter.layout(t.text.clone(), FontId::proportional(14.0), FG, 380.0);
            let rect = Rect::from_min_size(
                pos2(screen.right() - galley.size().x - 40.0, y),
                galley.size() + vec2(28.0, 16.0),
            );
            let color = if t.error {
                Color32::from_rgb(120, 32, 40)
            } else {
                Color32::from_rgb(34, 44, 66)
            };
            painter.rect(
                rect,
                CornerRadius::same(8),
                color.gamma_multiply(0.95),
                Stroke::new(1.0f32, Color32::from_white_alpha(30)),
                egui::StrokeKind::Inside,
            );
            painter.galley(rect.min + vec2(14.0, 8.0), galley, FG);
            y = rect.bottom() + 8.0;
        }
        if !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn draw_drop_hint(&self, ctx: &egui::Context, screen: Rect) {
        if ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("mkff-drop"),
        ));
        painter.rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(170));
        painter.rect_stroke(
            screen.shrink(16.0),
            CornerRadius::same(16),
            Stroke::new(3.0f32, ACCENT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            screen.center(),
            Align2::CENTER_CENTER,
            "Drop to play",
            FontId::proportional(28.0),
            FG,
        );
    }

    fn context_menu(&mut self, ctx: &egui::Context, resp: &egui::Response) {
        let mut action = None;
        resp.context_menu(|ui| {
            ui.set_min_width(200.0);
            if ui.button("Open file…").clicked() {
                action = Some(Action::Open);
                ui.close_menu();
            }
            if !self.settings.recent.is_empty() {
                ui.menu_button("Open recent", |ui| {
                    for p in self.settings.recent.clone() {
                        if ui
                            .button(file_name(&p))
                            .on_hover_text(p.display().to_string())
                            .clicked()
                        {
                            self.open(ctx, p, 0.0, true);
                            ui.close_menu();
                        }
                    }
                });
            }
            ui.separator();
            if let Some(p) = &self.player {
                let label = if p.is_playing() { "Pause" } else { "Play" };
                if ui.button(label).clicked() {
                    action = Some(Action::TogglePlay);
                    ui.close_menu();
                }
                ui.menu_button(format!("Speed ({})", speed_label(self.speed)), |ui| {
                    for s in SPEEDS {
                        if ui
                            .selectable_label((s - self.speed).abs() < 1e-6, speed_label(s))
                            .clicked()
                        {
                            action = Some(Action::Speed(s));
                            ui.close_menu();
                        }
                    }
                });
                let mut looping = self.settings.looping;
                if ui.checkbox(&mut looping, "Loop").clicked() {
                    action = Some(Action::Loop);
                }
                if ui.button("Save snapshot…").clicked() {
                    action = Some(Action::Snapshot);
                    ui.close_menu();
                }
            }
            let mut stats = self.settings.show_stats;
            if ui.checkbox(&mut stats, "Statistics").clicked() {
                action = Some(Action::Stats);
            }
            if ui.button("Full screen").clicked() {
                action = Some(Action::Fullscreen);
                ui.close_menu();
            }
            if ui.button("Keyboard shortcuts").clicked() {
                self.show_help = true;
                ui.close_menu();
            }
            if self.player.is_some() && ui.button("Close file").clicked() {
                self.close_file(ctx);
                ui.close_menu();
            }
        });
        if let Some(a) = action {
            match a {
                Action::TogglePlay => self.toggle_play(),
                Action::Open => self.show_open_dialog(),
                Action::Speed(s) => self.set_speed(s),
                Action::Loop => self.toggle_loop(),
                Action::Snapshot => self.snapshot(),
                Action::Stats => self.settings.show_stats = !self.settings.show_stats,
                Action::Fullscreen => self.toggle_fullscreen(ctx),
                _ => {}
            }
        }
    }
}

enum Action {
    TogglePlay,
    Seek(f64),
    ToggleMute,
    Volume(f32),
    Fullscreen,
    Open,
    Snapshot,
    Stats,
    Loop,
    Speed(f64),
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.paints.push_back(now);
        while self
            .paints
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
        {
            self.paints.pop_front();
        }
        if self.first_frame {
            self.first_frame = false;
            // the window position / size persist, full screen doesn't
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.startup.fullscreen));
        }
        self.poll_loading(ctx);
        self.poll_dialogs(ctx);
        self.handle_drops(ctx);
        self.handle_keys(ctx);

        if let Some(p) = &mut self.player {
            p.update();
            for e in p.take_events() {
                self.toasts.push(Toast {
                    text: e,
                    error: true,
                    at: Instant::now(),
                });
            }
        }

        // pointer activity shows the controls
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        if pointer != self.last_pointer {
            if pointer.is_some() && self.last_pointer.is_some() {
                self.activity();
            }
            self.last_pointer = pointer;
        }
        if ctx.input(|i| i.pointer.any_down() || i.pointer.any_click()) {
            self.activity();
        }

        let screen = ctx.screen_rect();
        let mut video_resp = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let resp = ui.interact(rect, egui::Id::new("mkff-video-area"), Sense::click());
                if self.loading.is_some() {
                    self.draw_loading(ui, rect);
                } else if self.player.is_some() {
                    self.draw_video(ui, rect);
                } else {
                    self.draw_welcome(ui, rect);
                }
                video_resp = Some(resp);
            });

        let playing = self.player.as_ref().is_some_and(|p| p.is_playing());
        let pointer_in_controls =
            pointer.is_some_and(|p| p.y > screen.bottom() - 130.0 || p.y < screen.top() + 60.0);
        let idle = self.last_activity.elapsed();
        let controls_visible = self.player.is_some()
            && (!playing
                || idle < HIDE_CONTROLS_AFTER
                || pointer_in_controls
                || self.scrub.is_some());
        let alpha =
            ctx.animate_bool_with_time(egui::Id::new("mkff-controls-alpha"), controls_visible, 0.2);

        if alpha > 0.0 {
            self.draw_top_bar(ctx, screen, alpha);
            self.draw_controls(ctx, screen, alpha);
        }
        if self.settings.show_stats && self.player.is_some() {
            self.draw_stats(ctx);
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.show_help {
            self.draw_help(ctx);
        }
        self.draw_feedback(ctx, screen);
        self.draw_drop_hint(ctx, screen);

        if let Some(resp) = video_resp {
            if self.player.is_some() {
                // clicks on the (possibly hidden) control bars only reveal them
                if !pointer_in_controls {
                    if resp.double_clicked() {
                        // the first click of the pair toggled playback: undo that
                        self.toggle_play();
                        self.toggle_fullscreen(ctx);
                    } else if resp.clicked() {
                        self.toggle_play();
                    }
                }
                if resp.hovered() {
                    let scroll = ctx.input(|i| i.raw_scroll_delta.y);
                    if scroll != 0.0 {
                        self.change_volume(if scroll > 0.0 { 0.05 } else { -0.05 });
                        self.activity();
                    }
                }
            }
            if self.player.is_some() && !controls_visible && resp.hovered() {
                ctx.set_cursor_icon(CursorIcon::None);
            }
            self.context_menu(ctx, &resp);
        }

        // sync settings the player may have changed
        if let Some(p) = &self.player {
            self.settings.looping = p.looping();
        }

        // repaint scheduling: on the next frame's due time while playing
        if let Some(p) = &self.player {
            if p.is_playing() {
                let wait = p
                    .time_to_next_frame()
                    .unwrap_or(Duration::from_millis(16))
                    .min(Duration::from_millis(if controls_visible {
                        33
                    } else {
                        100
                    }));
                ctx.request_repaint_after(wait);
                if controls_visible && idle < HIDE_CONTROLS_AFTER {
                    ctx.request_repaint_after(HIDE_CONTROLS_AFTER - idle);
                }
            }
        }
        if self.loading.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(10)
    }
}

fn speed_label(s: f64) -> String {
    let t = format!("{s:.2}");
    let t = t.trim_end_matches('0').trim_end_matches('.');
    format!("{t}×")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn write_png(path: &Path, pic: &Picture) -> Result<(), String> {
    let rgba = pic.to_rgba8();
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), pic.width, pic.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::BLACK;
    v.window_fill = Color32::from_rgb(22, 24, 30);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.6);
    v.hyperlink_color = ACCENT;
    v.widgets.hovered.bg_fill = Color32::from_rgb(52, 56, 66);
    v.widgets.active.bg_fill = Color32::from_rgb(64, 70, 84);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
    });
}
