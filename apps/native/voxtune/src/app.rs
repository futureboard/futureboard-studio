//! The window: the platform's own frame and menu bar around the editor.
//!
//! The editor (`SphereUIComponents::voxtune`) fills the client area; the app
//! adds only what a desktop app has around it:
//!
//! - a native window (on Windows the system caption and a Win32 menu bar,
//!   [`sphere_ui_components::native_win32_menu`]) whose title names the take;
//! - File ▸ Open / Export, the editor's commands in Edit / View / Transport,
//!   and the same shortcuts on the keyboard;
//! - the output device, drag-and-drop, and the per-file cache that brings a
//!   take's analysis and edits back when it is opened again.
//!
//! Status (device, export result, a failed save) goes to the editor's status
//! bar through its host, so nothing sits above the editor.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use gpui::prelude::*;
use gpui::{
    App, Context, Entity, ExternalPaths, KeyDownEvent, PathPromptOptions, Window, WindowBounds,
    WindowHandle, div, px, size,
};
use sphere_ui_components::components::title_bar::external_window_titlebar;
use sphere_ui_components::components::voxtune::{
    EngineTransport, VoxTransport, VoxtuneEditor, VoxtuneHost,
};
use sphere_ui_components::theme::Colors;
use sphere_ui_components::window_position::centered_window_bounds;
use sphere_voxtune::audio_io::encode_mono_wav_i16;
use sphere_voxtune::engine::{CantoAnalysisResult, CantoEngine};
use sphere_voxtune::load_job::{JobKind, LoadJob};
use sphere_voxtune::session::VoxDocument;
use sphere_voxtune::shared::Shared;
use sphere_voxtune::store::TakeStore;

use crate::output::Output;

const APP_NAME: &str = "Solfege Voxtune";
const WINDOW_WIDTH: f32 = 1280.0;
const WINDOW_HEIGHT: f32 = 800.0;
const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "aif", "aiff", "flac", "mp3", "ogg", "m4a", "aac", "mp4", "caf",
];

/// App commands. `editor:*` go to [`VoxtuneEditor::run_command`].
const COMMANDS: &[&str] = &[
    "file:open",
    "file:export",
    "file:close",
    "app:quit",
    "editor:undo",
    "editor:redo",
    "editor:tune-note",
    "editor:tune-all",
    "editor:join",
    "editor:mute",
    "editor:reset-note",
    "editor:fit",
    "editor:zoom-in",
    "editor:zoom-out",
    "editor:history",
    "editor:panel-note",
    "editor:panel-shape",
    "editor:panel-take",
    "editor:original",
    "editor:play",
    "editor:home",
    "editor:loop",
];

/// What the window and the editor's host share.
#[derive(Default)]
struct AppState {
    /// The open take.
    path: Option<PathBuf>,
    /// Output device line for the status bar.
    device: Option<String>,
    /// Last export result or failure, `(text, warning)`.
    notice: Option<(String, bool)>,
}

/// The editor's host: engine transport, the per-file cache, status text.
struct AppHost {
    transport: EngineTransport,
    has_output: bool,
    store: Rc<TakeStore>,
    state: Rc<RefCell<AppState>>,
}

impl VoxtuneHost for AppHost {
    fn transport(&self, cx: &App) -> VoxTransport {
        self.transport.transport(cx)
    }

    fn controls_transport(&self) -> bool {
        self.has_output
    }

    fn set_playing(&mut self, playing: bool, cx: &mut App) {
        self.transport.set_playing(playing, cx);
    }

    fn seek(&mut self, seconds: f64, cx: &mut App) {
        self.transport.seek(seconds, cx);
    }

    fn can_audition(&self) -> bool {
        self.has_output
    }

    fn looping(&self, cx: &App) -> Option<bool> {
        self.transport.looping(cx)
    }

    fn set_looping(&mut self, on: bool, cx: &mut App) {
        self.transport.set_looping(on, cx);
    }

    fn document_changed(&mut self, doc: &VoxDocument, _cx: &mut App) {
        let mut state = self.state.borrow_mut();
        let Some(path) = state.path.clone() else {
            return;
        };
        if let Err(e) = self.store.write_document(&path, doc) {
            state.notice = Some((format!("Edits could not be saved: {e}"), true));
        }
    }

    fn analysis_ready(&mut self, result: &CantoAnalysisResult, _cx: &mut App) {
        let Some(path) = self.state.borrow().path.clone() else {
            return;
        };
        if let Ok(json) = serde_json::to_string(result) {
            let _ = self.store.write_analysis(&path, &json);
        }
    }

    fn status(&self, _cx: &App) -> (Option<String>, Option<(String, bool)>) {
        let state = self.state.borrow();
        (state.device.clone(), state.notice.clone())
    }
}

pub struct VoxtuneApp {
    editor: Entity<VoxtuneEditor>,
    shared: Arc<Shared>,
    output: Option<Output>,
    store: Rc<TakeStore>,
    state: Rc<RefCell<AppState>>,
    /// The window draws its own title bar (no native frame on this platform).
    drawn_titlebar: bool,
    /// Title last given to the native caption.
    shown_title: String,
}

impl VoxtuneApp {
    fn new(open: Option<PathBuf>, drawn_titlebar: bool, cx: &mut Context<Self>) -> Self {
        let state = Rc::new(RefCell::new(AppState::default()));
        let (output, shared) = match Output::open() {
            Ok((output, shared)) => {
                state.borrow_mut().device = Some(format!(
                    "{} · {} Hz",
                    output.device_name, output.sample_rate
                ));
                (Some(output), shared)
            }
            // Editing still works without a device; playback is off and the
            // status bar says why.
            Err(e) => {
                state.borrow_mut().notice = Some((e, true));
                (None, Arc::new(Shared::new(CantoEngine::new(48_000.0))))
            }
        };
        let store = Rc::new(TakeStore::new(
            sphere_ui_components::paths::FutureboardPaths::resolve()
                .app_cache
                .join("Voxtune"),
        ));
        let host = AppHost {
            transport: EngineTransport {
                shared: shared.clone(),
                on_change: None,
            },
            has_output: output.is_some(),
            store: store.clone(),
            state: state.clone(),
        };
        let editor = cx.new(|cx| {
            let mut editor = VoxtuneEditor::new(shared.clone(), Box::new(host), cx);
            editor.set_empty_hint(
                "Open a vocal take with File ▸ Open (Ctrl+O), or drop one here.",
                cx,
            );
            editor
        });
        let mut app = Self {
            editor,
            shared,
            output,
            store,
            state,
            drawn_titlebar,
            shown_title: String::new(),
        };
        if let Some(path) = open {
            app.open(path, cx);
        }
        app
    }

    fn take_name(&self) -> Option<String> {
        self.state.borrow().path.as_deref().map(file_name)
    }

    fn title(&self) -> String {
        match self.take_name() {
            Some(name) => format!("{name} — {APP_NAME}"),
            None => APP_NAME.to_string(),
        }
    }

    fn set_notice(&mut self, notice: Option<(String, bool)>, cx: &mut Context<Self>) {
        self.state.borrow_mut().notice = notice;
        self.editor.update(cx, |_, cx| cx.notify());
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let is_audio = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_audio {
            self.set_notice(
                Some((format!("{} is not an audio file.", file_name(&path)), true)),
                cx,
            );
            return;
        }
        {
            let mut engine = self.shared.lock();
            engine.set_playing(false);
            self.shared.publish(&engine);
        }
        let cached = self.store.read_analysis(&path);
        let saved = self.store.read_document(&path);
        let job = LoadJob::start(
            self.shared.clone(),
            JobKind::File {
                path: path.clone(),
                cached,
            },
        );
        {
            let mut state = self.state.borrow_mut();
            state.path = Some(path.clone());
            state.notice = None;
        }
        let title = file_name(&path);
        self.editor
            .update(cx, |editor, cx| editor.start_load(job, title, saved, cx));
        cx.notify();
    }

    fn prompt_open(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open vocal take".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |app, cx| app.open(path, cx));
                }
            }
        })
        .detach();
    }

    fn can_export(&self, cx: &App) -> bool {
        let editor = self.editor.read(cx);
        editor.has_audio() && !editor.is_loading()
    }

    /// Renders the edited take to a WAV the user picks. The render works on a
    /// copy of the engine, so playback and editing carry on meanwhile.
    fn export(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.state.borrow().path.clone() else {
            return;
        };
        if !self.can_export(cx) {
            return;
        }
        let dir = source
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("take")
            .to_string();
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{stem}-tuned.wav")));
        let engine = self.shared.lock().clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                app.set_notice(
                    Some((format!("Exporting {}…", file_name(&path)), false)),
                    cx,
                )
            });
            let target = path.clone();
            let result = executor
                .spawn(async move {
                    let pcm = engine.render_offline();
                    let wav = encode_mono_wav_i16(&pcm, engine.sample_rate() as u32);
                    std::fs::write(&target, wav)
                })
                .await;
            let _ = this.update(cx, |app, cx| {
                let notice = match result {
                    Ok(()) => (format!("Exported {}", file_name(&path)), false),
                    Err(e) => (format!("Export failed: {e}"), true),
                };
                app.set_notice(Some(notice), cx);
            });
        })
        .detach();
    }

    /// Closes the take; the editor goes back to its empty state.
    fn close_take(&mut self, cx: &mut Context<Self>) {
        {
            let mut engine = self.shared.lock();
            engine.set_playing(false);
            self.shared.publish(&engine);
        }
        self.state.borrow_mut().path = None;
        let shared = self.shared.clone();
        self.editor
            .update(cx, |editor, cx| editor.set_engine(shared, cx));
        cx.notify();
    }

    fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        match command {
            "file:open" => self.prompt_open(cx),
            "file:export" => self.export(cx),
            "file:close" => self.close_take(cx),
            "app:quit" => cx.quit(),
            other => {
                if let Some(id) = other.strip_prefix("editor:") {
                    self.editor.update(cx, |editor, cx| {
                        editor.run_command(id, cx);
                    });
                }
            }
        }
    }

    /// App shortcuts. The editor handles its own (Space, Ctrl+Z, …) while it
    /// has focus; these work anywhere in the window.
    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let cmd = k.modifiers.control || k.modifiers.platform;
        let command = match (k.key.as_str(), cmd, k.modifiers.shift) {
            ("o", true, false) => "file:open",
            ("e", true, false) => "file:export",
            ("w", true, false) => "file:close",
            _ => return,
        };
        self.run_command(command, cx);
        cx.stop_propagation();
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

impl Render for VoxtuneApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title();
        if !self.drawn_titlebar && title != self.shown_title {
            window.set_window_title(&title);
            self.shown_title = title.clone();
        }
        if self
            .output
            .as_ref()
            .is_some_and(|o| o.failed.load(Ordering::Relaxed))
        {
            let mut state = self.state.borrow_mut();
            if state.device.is_some() {
                state.device = None;
                state.notice = Some((
                    "The output device was lost — restart Voxtune to reconnect.".into(),
                    true,
                ));
            }
        }

        div()
            .id("voxtune-app")
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .capture_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key(event, cx)),
            )
            .when(self.drawn_titlebar, |d| {
                d.child(external_window_titlebar(
                    title,
                    "voxtune-close",
                    |_window, cx: &mut App| cx.quit(),
                ))
            })
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
            .drag_over::<ExternalPaths>(|style, _, _, _| {
                style.bg(Colors::with_alpha(Colors::accent_primary(), 0.06))
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(path) = paths.paths().first() {
                    this.open(path.clone(), cx);
                }
            }))
    }
}

pub fn open_window(
    open: Option<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<VoxtuneApp>, String> {
    // The platform's own frame (caption, buttons, menu bar) where there is
    // one; elsewhere the drawn chrome.
    let native = sphere_ui_components::platform_chrome::native_tool_window_frame();
    let mut options =
        sphere_ui_components::platform_chrome::native_tool_window_options_partial(APP_NAME);
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        cx.primary_display().map(|display| display.bounds()),
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        cx,
    )));
    options.app_id = Some("SolfegeVoxtune".into());
    let handle = cx
        .open_window(options, move |window, cx| {
            window.on_window_should_close(cx, |_, cx| {
                cx.quit();
                true
            });
            cx.new(|cx| {
                #[cfg(target_os = "windows")]
                if native {
                    menu::attach(window, cx);
                }
                VoxtuneApp::new(open, !native, cx)
            })
        })
        .map_err(|error| error.to_string())?;
    Ok(handle)
}

/// The Win32 menu bar. Shortcuts shown after a tab are the ones
/// [`VoxtuneApp::on_key`] and the editor handle.
#[cfg(target_os = "windows")]
mod menu {
    use super::*;
    use sphere_ui_components::native_win32_menu::{
        NativeMenu, NativeMenuItem, attach as attach_bar,
    };

    fn item(label: &str, command: &str) -> NativeMenuItem {
        NativeMenuItem::command(label, command)
    }

    pub fn attach(window: &mut Window, cx: &mut Context<VoxtuneApp>) {
        let menus = [
            NativeMenu {
                label: "&File".into(),
                items: vec![
                    item("&Open Take…\tCtrl+O", "file:open"),
                    item("&Export Tuned WAV…\tCtrl+E", "file:export"),
                    NativeMenuItem::Separator,
                    item("&Close Take\tCtrl+W", "file:close"),
                    NativeMenuItem::Separator,
                    item("E&xit\tAlt+F4", "app:quit"),
                ],
            },
            NativeMenu {
                label: "&Edit".into(),
                items: vec![
                    item("&Undo\tCtrl+Z", "editor:undo"),
                    item("&Redo\tCtrl+Shift+Z", "editor:redo"),
                    NativeMenuItem::Separator,
                    item("&Tune Note", "editor:tune-note"),
                    item("Tune &All Notes", "editor:tune-all"),
                    item("&Join with Next Note\tJ", "editor:join"),
                    item("&Mute Note\tDel", "editor:mute"),
                    item("Re&set Note", "editor:reset-note"),
                ],
            },
            NativeMenu {
                label: "&View".into(),
                items: vec![
                    item("&Fit Take\tF", "editor:fit"),
                    item("Zoom &In\tCtrl+=", "editor:zoom-in"),
                    item("Zoom &Out\tCtrl+-", "editor:zoom-out"),
                    NativeMenuItem::Separator,
                    item("&Note Panel", "editor:panel-note"),
                    item("&Shape Panel", "editor:panel-shape"),
                    item("&Take Panel", "editor:panel-take"),
                    item("&History", "editor:history"),
                ],
            },
            NativeMenu {
                label: "&Transport".into(),
                items: vec![
                    item("&Play / Pause\tSpace", "editor:play"),
                    item("Go to &Start\tHome", "editor:home"),
                    item("&Loop", "editor:loop"),
                    NativeMenuItem::Separator,
                    item("Hear &Original (A/B)", "editor:original"),
                ],
            },
        ];
        let Some(commands) = attach_bar(window, &menus) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let chosen = commands.next().await;
                let Some(command) = COMMANDS.iter().copied().find(|c| *c == chosen) else {
                    continue;
                };
                if this
                    .update(cx, |app, cx| app.run_command(command, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}
