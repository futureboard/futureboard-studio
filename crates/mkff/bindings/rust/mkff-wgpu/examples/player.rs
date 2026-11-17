//! Simple video player: MKFF decode → NV12 → RGBA → egui on wgpu.
//!
//! Plays any container / codec mkff-codecs supports: progressive MP4/MOV and
//! raw Annex-B (`.hevc` / `.h264`) through libmkff's hardware decoders first,
//! and MKV / WebM / AVI / MXF / TS / MPEG-PS / Ogg, VP8 / VP9 / AV1 / MPEG-2 /
//! ProRes / DNx / MJPEG — or anything libmkff can't decode here — in pure-Rust
//! software (no FFmpeg). Files are indexed quickly and decoded on demand.
//!
//! ```sh
//! cargo run -p mkff-wgpu --example player
//! cargo run -p mkff-wgpu --example player -- path/to/clip.mp4
//! cargo run -p mkff-wgpu --example player -- path/to/clip.hevc
//! cargo run -p mkff-wgpu --example player -- testdata/codecs/vp9_opus.webm
//! cargo run -p mkff-wgpu --example player -- testdata/codecs/prores_hq.mov
//! ```
//!
//! Space = play/pause · Left/Right = step · Esc = quit.
//! No audio — scrub via sample-index slider (seek + decode from prior sync).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions, Vec2};
use mkff_wgpu::VideoSource;

fn main() -> eframe::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_media_path);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MKFF × egui — video player")
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([480.0, 320.0]),
        ..Default::default()
    };

    eframe::run_native(
        "MKFF player",
        options,
        Box::new(move |cc| Ok(Box::new(PlayerApp::new(cc, path)))),
    )
}

fn default_media_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("testdata/tiny_main_256x144.mp4")
}

struct PlayerApp {
    path_edit: String,
    status: String,
    source: Option<VideoSource>,
    texture: Option<TextureHandle>,
    index: usize,
    dirty: bool,
    playing: bool,
    looping: bool,
    fps: f32,
    last_advance: Instant,
}

impl PlayerApp {
    fn new(cc: &eframe::CreationContext<'_>, path: PathBuf) -> Self {
        apply_theme(&cc.egui_ctx);
        let path_edit = path.display().to_string();
        let mut app = Self {
            path_edit,
            status: String::new(),
            source: None,
            texture: None,
            index: 0,
            dirty: true,
            playing: true,
            looping: true,
            fps: 10.0,
            last_advance: Instant::now(),
        };
        app.load_path(Path::new(&app.path_edit.clone()));
        app
    }

    fn load_path(&mut self, path: &Path) {
        self.playing = false;
        self.source = None;
        self.texture = None;
        self.index = 0;
        self.dirty = true;
        self.status = format!("Opening {}…", path.display());

        let t0 = Instant::now();
        match VideoSource::open(path) {
            Ok(mut source) => {
                let n = source.frame_count();
                let w = source.width();
                let h = source.height();
                self.status = format!(
                    "Indexed {} · {}×{} · {} frame{} ({:.0} ms) — decoding frame 0…",
                    path.display(),
                    w,
                    h,
                    n,
                    if n == 1 { "" } else { "s" },
                    t0.elapsed().as_secs_f64() * 1000.0
                );

                let t1 = Instant::now();
                match source.frame(0) {
                    Ok(frame) => {
                        let fw = frame.width;
                        let fh = frame.height;
                        self.status = format!(
                            "{} · {}×{} · {} frame{} · on-demand (open {:.0} ms, frame0 {:.0} ms)",
                            path.display(),
                            fw,
                            fh,
                            n,
                            if n == 1 { "" } else { "s" },
                            t0.elapsed().as_secs_f64() * 1000.0,
                            t1.elapsed().as_secs_f64() * 1000.0
                        );
                        self.source = Some(source);
                        self.playing = n > 1;
                        self.last_advance = Instant::now();
                        self.dirty = true;
                    }
                    Err(e) => {
                        self.status = format!("decode frame 0 failed: {e}");
                    }
                }
            }
            Err(e) => {
                self.status = format!("load failed: {e}");
            }
        }
    }

    fn frame_count(&self) -> usize {
        self.source.as_ref().map(|s| s.frame_count()).unwrap_or(0)
    }

    fn sync_texture(&mut self, ctx: &egui::Context) {
        if !self.dirty {
            return;
        }
        let Some(source) = self.source.as_mut() else {
            self.texture = None;
            self.dirty = false;
            return;
        };
        let index = self.index;
        match source.frame(index) {
            Ok(frame) => {
                let image = ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.to_rgba8(),
                );
                match &mut self.texture {
                    Some(tex) => tex.set(image, TextureOptions::LINEAR),
                    None => {
                        self.texture =
                            Some(ctx.load_texture("mkff-frame", image, TextureOptions::LINEAR));
                    }
                }
                self.dirty = false;
            }
            Err(e) => {
                self.status = format!("decode frame {index} failed: {e}");
                self.playing = false;
                self.dirty = false;
            }
        }
    }

    fn set_index(&mut self, index: usize) {
        let n = self.frame_count();
        if n == 0 {
            return;
        }
        let next = index.min(n - 1);
        if next != self.index {
            self.index = next;
            self.dirty = true;
            if let Some(src) = &self.source {
                self.status = format!(
                    "frame {}/{} · {}×{} · on-demand",
                    self.index + 1,
                    src.frame_count(),
                    src.width(),
                    src.height()
                );
            }
        }
    }

    fn advance(&mut self) {
        let n = self.frame_count();
        if n == 0 {
            return;
        }
        if self.index + 1 < n {
            self.set_index(self.index + 1);
        } else if self.looping {
            self.set_index(0);
        } else {
            self.playing = false;
        }
    }

    fn tick_playback(&mut self, ctx: &egui::Context) {
        if !self.playing || self.frame_count() <= 1 {
            return;
        }
        let period = Duration::from_secs_f32(1.0 / self.fps.max(1.0));
        if self.last_advance.elapsed() >= period {
            self.advance();
            self.last_advance = Instant::now();
        }
        ctx.request_repaint_after(period / 2);
    }
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick_playback(ctx);
        self.sync_texture(ctx);

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.playing = !self.playing;
            self.last_advance = Instant::now();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
            self.playing = false;
            self.set_index(self.index.saturating_sub(1));
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            self.playing = false;
            self.set_index(self.index + 1);
        }

        egui::TopBottomPanel::bottom("controls")
            .exact_height(88.0)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(18, 18, 18))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let play_label = if self.playing { "Pause" } else { "Play" };
                    let has = self.frame_count() > 0;
                    if ui.add_enabled(has, egui::Button::new(play_label)).clicked() {
                        self.playing = !self.playing;
                        self.last_advance = Instant::now();
                    }
                    if ui.add_enabled(has, egui::Button::new("Prev")).clicked() {
                        self.playing = false;
                        self.set_index(self.index.saturating_sub(1));
                    }
                    if ui.add_enabled(has, egui::Button::new("Next")).clicked() {
                        self.playing = false;
                        self.set_index(self.index + 1);
                    }
                    ui.checkbox(&mut self.looping, "Loop");
                    ui.add(
                        egui::DragValue::new(&mut self.fps)
                            .range(1.0..=60.0)
                            .prefix("FPS ")
                            .speed(0.5),
                    );

                    if has {
                        let max = (self.frame_count() - 1) as u32;
                        let mut slider = self.index as u32;
                        ui.style_mut().spacing.slider_width = ui.available_width().max(120.0) - 80.0;
                        if ui
                            .add(egui::Slider::new(&mut slider, 0..=max).text("frame"))
                            .changed()
                        {
                            self.playing = false;
                            self.set_index(slider as usize);
                        }
                    }
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.path_edit)
                            .desired_width(ui.available_width() - 72.0)
                            .hint_text("Any clip: .mp4 .mov .mkv .webm .avi .mxf .ts .mpg .ogg .hevc .h264 …"),
                    );
                    if ui.button("Load").clicked() {
                        let path = PathBuf::from(self.path_edit.trim());
                        self.load_path(&path);
                    }
                });
                ui.label(
                    RichText::new(&self.status)
                        .size(12.0)
                        .color(Color32::from_rgb(160, 160, 160)),
                );
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let avail = ui.available_size();
                if let Some(tex) = &self.texture {
                    let size = tex.size_vec2();
                    let scale = (avail.x / size.x).min(avail.y / size.y).max(0.01);
                    let draw = size * scale;
                    let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
                    let image_rect = egui::Rect::from_center_size(rect.center(), draw);
                    ui.painter().image(
                        tex.id(),
                        image_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    ui.allocate_ui_with_layout(
                        avail,
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            ui.label(
                                RichText::new("No frames")
                                    .size(18.0)
                                    .color(Color32::from_rgb(120, 120, 120)),
                            );
                        },
                    );
                }
            });
    }
}

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(18, 18, 18);
    visuals.window_fill = Color32::from_rgb(18, 18, 18);
    visuals.override_text_color = Some(Color32::from_rgb(230, 230, 230));
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(40, 40, 40);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(55, 55, 55);
    visuals.widgets.active.bg_fill = Color32::from_rgb(70, 70, 70);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 6.0);
    ctx.set_style(style);
}
