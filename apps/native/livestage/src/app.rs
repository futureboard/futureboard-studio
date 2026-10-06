//! The LiveStage window.
//!
//! One window, three views, in the order a live setup is used:
//!
//! ```txt
//! Setup   which interface, at what rate, recording where
//! Patch   which inputs feed which channels, which mixes leave which outputs
//! Mixer   the strips, played for the rest of the night
//! ```
//!
//! The toolbar above them holds what is needed from any of the three: the
//! session file, the record button and the health of the interface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    Render, Styled, WeakEntity, Window, WindowBounds, WindowHandle, div, px, size,
};
use livestage_engine::{Command, Id, LiveEngine, Session, StripRef};
use sphere_ui_components::components::controls::{
    FbButtonKind, FbLatch, FbSegment, fb_button, fb_segment, fb_segmented_track, fb_toggle,
};
use sphere_ui_components::components::title_bar::external_window_titlebar;
use sphere_ui_components::theme::{Colors, space, typography};
use sphere_ui_components::window_position::centered_window_bounds;

/// Meters and status are re-read at 30 Hz, the rate the rest of Futureboard
/// meters at.
const REFRESH: Duration = Duration::from_millis(33);

/// How fast a meter falls after a peak: 20 dB per second.
const METER_FALL_PER_TICK: f32 = 0.927;

/// How long a clip light stays lit after the strip last clipped.
const CLIP_HOLD: Duration = Duration::from_secs(2);

/// How long a status message stands.
const MESSAGE_LINGER: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Mixer,
    Patch,
    Setup,
}

/// A strip's meter as drawn: peaks that fall at a steady rate, and a clip
/// light that holds long enough to be seen.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterDisplay {
    pub output: (f32, f32),
    pub input: (f32, f32),
    pub clipped_at: Option<Instant>,
}

impl MeterDisplay {
    pub fn clipping(&self) -> bool {
        self.clipped_at.is_some_and(|at| at.elapsed() < CLIP_HOLD)
    }
}

/// Which pop-up menu is open, and where.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuKind {
    AddInsert(StripRef),
    StripOutput(StripRef),
    ChannelInput(Id),
    AddSend(Id),
}

#[derive(Debug, Clone, Copy)]
pub struct OpenMenu {
    pub kind: MenuKind,
    pub at: Point<Pixels>,
}

pub struct LiveStageApp {
    pub(crate) engine: LiveEngine,
    pub(crate) session_path: PathBuf,
    pub(crate) view: View,
    pub(crate) meters: HashMap<StripRef, MeterDisplay>,
    pub(crate) menu: Option<OpenMenu>,
    pub(crate) editors: crate::editors::Editors,
    pub(crate) setup: crate::setup::SetupState,
    #[cfg(feature = "external-plugins")]
    pub(crate) installed: Option<Vec<livestage_engine::external::InstalledEffect>>,
    message: Option<(String, Instant)>,
}

impl LiveStageApp {
    fn new(session: Session, session_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let fresh = session.channels.is_empty();
        let mut engine = LiveEngine::new(session);
        if fresh {
            // A new session: one channel per interface input, faders down.
            for index in 0..engine.input_channels() {
                let _ = engine.apply(Command::AddChannel {
                    name: format!("Input {}", index + 1),
                    input: livestage_engine::InputPatch::mono(index as u16),
                });
            }
        }
        Self::spawn_refresh(cx);
        cx.on_app_quit(|this, _cx| {
            this.shut_down();
            async {}
        })
        .detach();
        let setup = crate::setup::SetupState::from_session(engine.session());
        Self {
            engine,
            session_path,
            view: View::Mixer,
            meters: HashMap::new(),
            menu: None,
            editors: crate::editors::Editors::default(),
            setup,
            #[cfg(feature = "external-plugins")]
            installed: None,
            message: None,
        }
    }

    fn spawn_refresh(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                let alive = this
                    .update(cx, |this, cx| {
                        this.tick(cx);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.engine.poll();
        let now = Instant::now();
        for (strip, levels) in self.engine.meters() {
            let display = self.meters.entry(strip).or_default();
            let fall = |shown: f32, peak: f32| peak.max(shown * METER_FALL_PER_TICK);
            display.output = (
                fall(display.output.0, levels.output.0),
                fall(display.output.1, levels.output.1),
            );
            display.input = (
                fall(display.input.0, levels.input.0),
                fall(display.input.1, levels.input.1),
            );
            if levels.output.0 >= 1.0
                || levels.output.1 >= 1.0
                || levels.input.0 >= 1.0
                || levels.input.1 >= 1.0
            {
                display.clipped_at = Some(now);
            }
        }
        #[cfg(feature = "external-plugins")]
        {
            let events = self.engine.take_external_events();
            crate::editors::handle_external_events(self, events, cx);
        }
        #[cfg(not(feature = "external-plugins"))]
        let _ = cx;
    }

    /// Apply `command`, reporting a refusal in the status line.
    pub(crate) fn run(&mut self, command: Command) {
        if let Err(error) = self.engine.apply(command) {
            self.say(error);
        }
    }

    pub(crate) fn say(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
    }

    /// Stop recording and save; what quitting has to do.
    fn shut_down(&mut self) {
        let _ = self.engine.stop_recording();
        let path = self.session_path.clone();
        if let Err(error) = self.engine.save(&path, Duration::from_secs(2)) {
            eprintln!("[livestage] session not saved: {error}");
        }
        remember_session(&path);
    }

    fn save(&mut self) {
        let path = self.session_path.clone();
        match self.engine.save(&path, Duration::from_secs(2)) {
            Ok(()) => {
                remember_session(&path);
                self.say(format!("Saved {}", path.display()));
            }
            Err(error) => self.say(format!("Not saved: {error}")),
        }
    }

    fn save_as(&mut self, cx: &mut Context<Self>) {
        let dir = self
            .session_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(sessions_folder);
        let name = self
            .session_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Session.livestage.json".to_string());
        let answer = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = answer.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.session_path = path;
                this.save();
                cx.notify();
            });
        })
        .detach();
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open session".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.open_path(path, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let session = match Session::load(&path) {
            Ok(session) => session,
            Err(error) => {
                self.say(error);
                return;
            }
        };
        // The open session is saved first: opening another must not lose it.
        let current = self.session_path.clone();
        let _ = self.engine.save(&current, Duration::from_secs(2));
        self.editors.close_all(&mut self.engine, cx);
        match self.engine.load(session) {
            Ok(()) => {
                self.session_path = path.clone();
                remember_session(&path);
                self.meters.clear();
                self.setup = crate::setup::SetupState::from_session(self.engine.session());
                self.say(format!("Opened {}", path.display()));
            }
            Err(error) => self.say(error),
        }
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        let current = self.session_path.clone();
        let _ = self.engine.save(&current, Duration::from_secs(2));
        self.editors.close_all(&mut self.engine, cx);
        let mut session = Session {
            audio: self.engine.session().audio.clone(),
            recording: self.engine.session().recording.clone(),
            ..Session::default()
        };
        session.name = "Untitled".to_string();
        if let Err(error) = self.engine.load(session) {
            self.say(error);
            return;
        }
        for index in 0..self.engine.input_channels() {
            let _ = self.engine.apply(Command::AddChannel {
                name: format!("Input {}", index + 1),
                input: livestage_engine::InputPatch::mono(index as u16),
            });
        }
        self.session_path = unused_session_path();
        self.meters.clear();
        self.say("New session");
    }

    fn toggle_recording(&mut self) {
        if self.engine.is_recording() {
            if let Some(summary) = self.engine.stop_recording() {
                let mut line = format!(
                    "Recorded {} file(s), {:.0} s, in {}",
                    summary.files.len(),
                    summary.seconds,
                    summary.folder.display()
                );
                if summary.dropped_samples > 0 {
                    line.push_str(&format!(
                        " — {} samples lost: the disk fell behind",
                        summary.dropped_samples
                    ));
                }
                if let Some(error) = summary.errors.first() {
                    line = format!("Recording failed: {error}");
                }
                self.say(line);
            }
        } else {
            self.run(Command::StartRecording);
        }
    }

    fn status_line(&self) -> String {
        if let Some((message, at)) = &self.message {
            if at.elapsed() < MESSAGE_LINGER {
                return message.clone();
            }
        }
        let status = self.engine.status();
        if let Some(error) = status.error {
            return format!("Audio: {error}");
        }
        if status.input_underruns > 0 {
            return format!(
                "{} input underrun(s): the interface's input and output are drifting, or the buffer is too small",
                status.input_underruns
            );
        }
        self.session_path.display().to_string()
    }

    fn toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let status = self.engine.status();
        let recording = self.engine.is_recording();
        let view = self.view;
        let tab = |id: &'static str, label: &'static str, target: View, position: FbSegment| {
            let this = this.clone();
            fb_segment(id, label, view == target, position, move |_, _, cx| {
                let _ = this.update(cx, |app, cx| {
                    app.view = target;
                    app.menu = None;
                    cx.notify();
                });
            })
        };
        let button =
            |id: &'static str, label: &'static str, action: fn(&mut Self, &mut Context<Self>)| {
                let this = this.clone();
                fb_button(id, label, FbButtonKind::Ghost, true, move |_, _, cx| {
                    let _ = this.update(cx, |app, cx| {
                        action(app, cx);
                        cx.notify();
                    });
                })
            };
        let device = if status.running {
            format!(
                "{} · {} Hz · {} in / {} out · DSP {:.0}%",
                status.output_device.as_deref().unwrap_or(""),
                status.sample_rate,
                status.in_channels,
                status.out_channels,
                status.load * 100.0
            )
        } else {
            "No audio device".to_string()
        };
        let record_label = match status.recording_seconds {
            Some(seconds) => format!("● REC {}", format_clock(seconds)),
            None => "● REC".to_string(),
        };
        let record = {
            let this = this.clone();
            fb_toggle(
                "livestage-record",
                record_label,
                FbLatch::Arm,
                recording,
                24.0,
                move |_, _, cx| {
                    let _ = this.update(cx, |app, cx| {
                        app.toggle_recording();
                        cx.notify();
                    });
                },
            )
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .px(px(space::LOOSE))
            .py(px(space::TIGHT))
            .border_b_1()
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::HAIR))
                    .child(button("livestage-new", "New", Self::new_session))
                    .child(button("livestage-open", "Open…", Self::open))
                    .child(button("livestage-save", "Save", |app, _| app.save()))
                    .child(button("livestage-save-as", "Save As…", Self::save_as)),
            )
            .child(
                fb_segmented_track()
                    .child(tab(
                        "livestage-view-mixer",
                        "Mixer",
                        View::Mixer,
                        FbSegment::First,
                    ))
                    .child(tab(
                        "livestage-view-patch",
                        "Patch",
                        View::Patch,
                        FbSegment::Middle,
                    ))
                    .child(tab(
                        "livestage-view-setup",
                        "Setup",
                        View::Setup,
                        FbSegment::Last,
                    )),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(typography::UI_SM))
                    .text_color(if status.running {
                        Colors::text_secondary()
                    } else {
                        Colors::status_error()
                    })
                    .truncate()
                    .child(device),
            )
            .child(record)
    }

    fn footer(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(space::LOOSE))
            .py(px(space::TIGHT))
            .border_t_1()
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .text_size(px(typography::UI_SM))
            .text_color(Colors::text_muted())
            .child(div().truncate().child(self.status_line()))
    }
}

impl Render for LiveStageApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = format!("LiveStage — {}", self.engine.session().name);
        let body = match self.view {
            View::Mixer => crate::strip::mixer_view(self, cx).into_any_element(),
            View::Patch => crate::patch::patch_view(self, cx).into_any_element(),
            View::Setup => crate::setup::setup_view(self, cx).into_any_element(),
        };
        let menu = self
            .menu
            .map(|menu| crate::strip::menu_overlay(self, menu, window, cx));
        div()
            .id("livestage-root")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(sphere_ui_components::theme::ui_font())
            .child(external_window_titlebar(
                title,
                "livestage-close",
                move |_window, cx| {
                    cx.quit();
                },
            ))
            .child(self.toolbar(cx))
            .child(div().flex_1().min_h(px(0.0)).flex().flex_col().child(body))
            .child(self.footer())
            .when_some(menu, |root, menu| root.child(menu))
    }
}

/// `mm:ss`, or `h:mm:ss` past an hour.
pub fn format_clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Run `f` on the app from a control callback.
pub fn with_app(
    this: &WeakEntity<LiveStageApp>,
    cx: &mut App,
    f: impl FnOnce(&mut LiveStageApp, &mut Context<LiveStageApp>),
) {
    let _ = this.update(cx, |app, cx| {
        f(app, cx);
        cx.notify();
    });
}

/// Apply `command` from a control callback.
pub fn send(this: &WeakEntity<LiveStageApp>, cx: &mut App, command: Command) {
    with_app(this, cx, |app, _| app.run(command));
}

fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Futureboard").join("LiveStage")
}

fn sessions_folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let documents = home.join("Documents");
    if documents.is_dir() {
        documents.join("LiveStage")
    } else {
        home.join("LiveStage")
    }
}

/// A session file name in the sessions folder that is not taken yet.
fn unused_session_path() -> PathBuf {
    let folder = sessions_folder();
    let mut n = 1;
    loop {
        let path = folder.join(if n == 1 {
            "Session.livestage.json".to_string()
        } else {
            format!("Session {n}.livestage.json")
        });
        if !path.exists() {
            return path;
        }
        n += 1;
    }
}

/// The session LiveStage opens with next time. One in the temp folder (the
/// preview's demo) is never one to come back to.
fn remember_session(path: &Path) {
    if path.starts_with(std::env::temp_dir()) {
        return;
    }
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("last-session.json"),
        serde_json::json!({ "path": path }).to_string(),
    );
}

fn last_session() -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_dir().join("last-session.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let path = PathBuf::from(value.get("path")?.as_str()?);
    path.exists().then_some(path)
}

pub fn open_main_window(cx: &mut App) -> Result<WindowHandle<LiveStageApp>, String> {
    let (session, path) = match last_session() {
        Some(path) => match Session::load(&path) {
            Ok(session) => (session, path),
            Err(error) => {
                eprintln!("[livestage] {error}; starting a new session");
                (Session::default(), unused_session_path())
            }
        },
        None => (Session::default(), unused_session_path()),
    };
    open_window_with(session, path, cx)
}

/// Open the mixer on `session`, saved to `path`.
pub fn open_window_with(
    session: Session,
    path: PathBuf,
    cx: &mut App,
) -> Result<WindowHandle<LiveStageApp>, String> {
    let mut options = sphere_ui_components::platform_chrome::studio_window_options();
    options.show = true;
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        cx.primary_display().map(|display| display.bounds()),
        size(px(1280.0), px(800.0)),
        cx,
    )));
    options.window_min_size = Some(size(px(720.0), px(520.0)));
    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| LiveStageApp::new(session, path, cx))
    })
    .map_err(|error| error.to_string())
}
