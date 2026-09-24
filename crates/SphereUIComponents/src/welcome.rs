//! Futureboard Studio start screen.
//!
//! Three regions, each with exactly one owner: a 32 px chrome band (drag +
//! caption controls + wordmark + account chip), a quiet navigation rail, and one
//! centre pane per rail entry. Recent projects live in exactly one place — the
//! Start pane's second column — so the screen never shows the same list twice
//! with two different selection models.
//!
//! Three rules the whole file follows, stated once:
//!
//! * **Rows are full-bleed and square.** A list row is a lane, not a card. Only
//!   the inset well behind a list rounds ([`theme::radius::SURFACE`]), and every
//!   row state — rest, hover, pressed, selected — paints on that same row
//!   rectangle, so a row never has two state geometries.
//! * **State layers, never fill swaps.** A GPUI div has exactly one background,
//!   so hover/pressed/selected colours are composited up front by [`row_paint`]
//!   and handed to the style closures as resolved values.
//! * **One activation path.** Mouse click, `Enter`, and the printed Ctrl/Cmd
//!   shortcut all call the same `activate_*` method, so a row cannot do one
//!   thing under the pointer and another under the keyboard.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, img, px, svg, App, Context, FocusHandle, FontFeatures, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, Rgba, Role, SharedString,
    StatefulInteractiveElement, Styled, Window, WindowControlArea,
};
use serde::Deserialize;
use serde_json::Value;

const FEED_API_URL: &str = "https://feed.futureboard.studio/api/posts";
const FEED_PUBLIC_BASE_URL: &str = "https://futureboard.studio/blog";
const FEED_FETCH_TIMEOUT_SECS: u64 = 8;

use crate::assets;
use crate::components::controls::{
    fb_badge, fb_button, fb_form_row, fb_section_header, fb_segment, fb_segmented_track,
    FbButtonKind, FbSegment,
};
use crate::components::form::{select, select_dismiss_backdrop, SelectOption};
use crate::components::text_input::{
    bind_mouse_selection, is_repeatable_edit_key, text_field_with_callbacks_and_ime,
    TextInputCallbacks,
};
use crate::components::title_bar::{
    begin_titlebar_drag, draggable_spacer, section_separator, window_control_button,
};
use crate::components::{TextInputAction, TextInputState};
use crate::embedded_assets::LOGO_TEXT_PATH;
use crate::i18n::I18n;
use crate::platform_chrome::PlatformChromePolicy;
use crate::project::{ProjectCreateOptions, ProjectTemplate, RecentProject, RecentProjectsStore};
use crate::settings::SettingsSchema;
use crate::theme::{self, elevation, radius, size, space, typography, Colors};

// ── Region geometry ──────────────────────────────────────────────────────────
//
// `theme.rs` carries a control-height ladder and a spacing scale but no width
// tokens (the one exception, `size::STRIP_WIDTH`, is mixer-specific). Region
// widths are therefore named here, the way `components/project_switcher.rs`
// names its panel width, rather than repeated as literals.

/// Fixed width of the navigation rail.
const RAIL_WIDTH: f32 = 180.0;
/// Width of the Recent column when the Start pane runs two columns.
const RECENT_COLUMN_WIDTH: f32 = 320.0;
/// Reading-width cap for the form and prose panes. Wider than this and a
/// label/value form stops scanning as one column.
const PANE_MAX_WIDTH: f32 = 640.0;
/// Below this centre-pane width the Start pane stacks its two columns instead
/// of placing them side by side. Chosen so the Start column keeps a usable
/// ~360 px next to the 320 px Recent column plus the gap between them.
const START_STACK_BREAKPOINT: f32 = 720.0;

/// A two-line list row: one `size::PROMINENT` control band plus the
/// `space::LOOSE` the second line needs. One height for the start, recent and
/// feed lists, so the three read as one system.
const LIST_ROW_HEIGHT: f32 = size::PROMINENT + space::LOOSE;

/// Glyph sizes. `theme::size` is a *control height* ladder, not a glyph ladder,
/// so these are named locally exactly as `controls.rs` derives its own glyph
/// size from a control's height.
const RAIL_ICON: f32 = 12.0;
const ROW_ICON: f32 = 14.0;

/// Wordmark drawn in the chrome band. Width and height belong together — the
/// asset's aspect ratio is not free to change independently — and 18 px clears
/// the 32 px band with room either side.
const LOGO_WIDTH: f32 = 99.5;
const LOGO_HEIGHT: f32 = 9.0;

/// Width of the leading-edge selection marker. A hairline dimension in the same
/// family as the 1 px borders the crate draws everywhere; `theme.rs` carries no
/// stroke-width scale, so it is named here rather than repeated.
const SELECTION_MARKER_WIDTH: f32 = 2.0;

/// Left inset that lines a note up with `fb_form_row`'s control column:
/// `fb_field_label`'s fixed label width plus the row's `space::BASE` gap.
/// `controls.rs` does not export that width, so it is mirrored here.
const FORM_LABEL_WIDTH: f32 = 86.0;

/// `FUTUREBOARD_WELCOME_DEBUG=1` enables QA logging for the start screen
/// (selected tab, resolved default project path, and recent project activity).
fn welcome_debug(args: std::fmt::Arguments<'_>) {
    if std::env::var("FUTUREBOARD_WELCOME_DEBUG").as_deref() == Ok("1") {
        eprintln!("[welcome] {args}");
    }
}

macro_rules! welcome_debug {
    ($($arg:tt)*) => { welcome_debug(format_args!($($arg)*)) };
}

#[derive(Debug, Clone, PartialEq)]
pub enum WelcomeAction {
    EmptyProject,
    MidiComposer,
    AudioSession,
    MixTemplate,
    CreateProject(ProjectCreateOptions),
    /// Legacy: request the studio-side Open Project dialog. Kept for back-compat;
    /// the Welcome screen now browses + validates itself and emits
    /// [`WelcomeAction::OpenProjectFile`] instead.
    OpenProject,
    /// Open a specific, already-validated project file (from the Welcome
    /// Open Project tab's browse flow).
    OpenProjectFile(PathBuf),
    OpenRecent(PathBuf),
    /// Open the workspace shell directly with a blank, unsaved project.
    OpenEmptyWorkspace,
}

/// Which centre pane is showing. Every variant has an explicit arm in
/// [`WelcomeWindow::render_welcome`] — a catch-all is what previously let a rail
/// entry silently render somebody else's pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupNav {
    Start,
    NewProject,
    OpenProject,
    Feed,
}

/// Which New Project dropdown is open. Only one may be open at a time, so the
/// pane needs a single piece of state rather than one flag per field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectFieldMenu {
    SampleRate,
    Bpm,
    TimeSignature,
}

/// Sample rates offered when creating a project. Matches the rates the audio
/// settings expose, so a new project cannot be created at a rate the engine is
/// never configured for.
const PROJECT_SAMPLE_RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 192_000];

/// Tempo presets. Free entry stays available in Project Settings once the
/// project exists; the start screen only needs the common starting points.
const PROJECT_BPM_PRESETS: [f32; 10] = [
    70.0, 80.0, 90.0, 100.0, 110.0, 120.0, 128.0, 140.0, 160.0, 174.0,
];

/// Time signatures offered when creating a project.
const PROJECT_TIME_SIGNATURES: [(u32, u32); 8] = [
    (4, 4),
    (3, 4),
    (2, 4),
    (6, 8),
    (5, 4),
    (7, 8),
    (9, 8),
    (12, 8),
];

/// The templates offered by the New Project segmented control, in display order.
const PROJECT_TEMPLATES: [ProjectTemplate; 5] = [
    ProjectTemplate::Empty,
    ProjectTemplate::BeatMaking,
    ProjectTemplate::Recording,
    ProjectTemplate::Mixing,
    ProjectTemplate::Scoring,
];

/// Ctrl (Cmd on macOS) shortcuts for the Start rows, in row order, as
/// `(key, needs_shift)`.
///
/// One table feeds both the badge a row prints and the keystroke that fires it,
/// so the two cannot drift apart — the previous screen printed `Ctrl + N` next
/// to a row that the shortcut did not actually activate.
const START_SHORTCUTS: [(&str, bool); START_ROW_COUNT] = [
    ("n", false),
    ("m", true),
    ("a", true),
    ("t", true),
    ("o", false),
];

/// Four template rows plus the Open Project row.
const START_ROW_COUNT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WelcomeSelection {
    Start(usize),
    Recent(usize),
    Continue,
}

#[derive(Debug, Clone)]
enum FeedLoadState {
    Idle,
    Loading,
    Loaded,
    Failed(SharedString),
}

#[derive(Debug, Clone)]
struct FeedPost {
    title: SharedString,
    /// Empty when the payload carried no usable summary; the row substitutes a
    /// localized fallback at render time rather than baking English into the
    /// background parse.
    excerpt: SharedString,
    published_at: SharedString,
    slug: Option<SharedString>,
}

#[derive(Debug, Deserialize)]
struct FeedResponse {
    docs: Vec<PayloadPost>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayloadPost {
    title: String,
    slug: Option<String>,
    published_at: Option<String>,
    content: Option<Value>,
    meta: Option<PayloadPostMeta>,
}

#[derive(Debug, Deserialize)]
struct PayloadPostMeta {
    description: Option<String>,
}

#[derive(Clone)]
pub struct WelcomeCallbacks {
    pub on_action: Arc<dyn Fn(WelcomeAction, &mut Window, &mut App) + 'static>,
    /// Optional edition-owned action rendered at the bottom of the Welcome rail.
    pub footer_action: Option<WelcomeFooterAction>,
}

#[derive(Clone)]
pub struct WelcomeFooterAction {
    pub label: &'static str,
    pub icon: &'static str,
    pub on_click: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
}

pub struct WelcomeWindow {
    active_nav: StartupNav,
    recent_projects: Vec<RecentProject>,
    selected: Option<WelcomeSelection>,
    callbacks: WelcomeCallbacks,
    project_name_input: TextInputState,
    selected_template: ProjectTemplate,
    project_sample_rate: u32,
    project_bpm: f32,
    project_time_signature: (u32, u32),
    /// Open New Project dropdown, if any.
    open_project_menu: Option<ProjectFieldMenu>,
    // Default project location (resolved at construction from settings)
    default_project_dir: PathBuf,
    default_dir_configured: bool,
    /// Whether that folder exists on disk right now. Resolved once here and on
    /// every change instead of `Path::exists()` per frame — a render function
    /// must not stat the filesystem.
    default_dir_exists: bool,
    // Quick audio status readout (from saved settings)
    audio_backend: SharedString,
    audio_device_out: SharedString,
    /// `Edition · version`, resolved once. `edition::current_edition_info`
    /// invokes an installed provider, so it is not a per-frame read.
    edition_line: SharedString,
    // Open Project tab: inline validation error from the last browse attempt.
    open_error: Option<SharedString>,
    feed_state: FeedLoadState,
    feed_posts: Vec<FeedPost>,
    /// UI language from settings at construction (`schema.general.language`).
    language: String,
    /// One-shot guard for the work that needs a `Context` the constructor never
    /// receives. See [`WelcomeWindow::bootstrap`].
    bootstrapped: bool,
}

impl WelcomeWindow {
    pub fn new(callbacks: WelcomeCallbacks, focus_handle: FocusHandle) -> Self {
        // A single small JSON read. The `missing` flags it carries are whatever
        // was last persisted; they are re-derived off the UI thread by
        // `spawn_refresh_recent_missing`, so a row can briefly show a stale
        // "Missing" badge (or omit a fresh one) for the frames before that task
        // lands. `refresh_missing()` is deliberately NOT called here: it stats
        // every entry *and* rewrites the file synchronously, and the splash
        // boot (`startup::run_lightweight_boot`) already did that pass.
        let recent = RecentProjectsStore::load();

        // Default project path + audio readout from saved settings (the global
        // SettingsModel entity does not exist yet at Welcome time).
        let schema = SettingsSchema::load_from_disk();
        let default_project_dir = schema.general.resolved_default_project_dir();
        let default_dir_configured = schema.general.has_configured_project_dir();
        let default_dir_exists = default_project_dir.exists();
        welcome_debug!(
            "default project path resolved -> {} (configured={} exists={})",
            default_project_dir.display(),
            default_dir_configured,
            default_dir_exists
        );

        let language = schema.general.language.clone();
        let i18n = I18n::new(&language);
        let default_name = i18n.tr("project.default-name");
        let mut project_name_input = TextInputState::new("welcome-project-name", focus_handle)
            .with_placeholder(default_name.clone())
            .with_accessible_label(i18n.tr("wizard.field.name"));
        project_name_input.set_value(default_name);

        let edition = crate::edition::current_edition_info()
            .map(|info| info.edition.to_string())
            .unwrap_or_else(|| i18n.tr_or("welcome.edition.community", "Community"));
        let edition_line =
            SharedString::from(format!("{edition} · {}", crate::edition::app_version()));

        Self {
            active_nav: StartupNav::Start,
            // The list scrolls, so it shows everything the store holds rather
            // than an arbitrary first seven.
            recent_projects: recent.entries().to_vec(),
            selected: Some(WelcomeSelection::Start(0)),
            callbacks,
            project_name_input,
            selected_template: ProjectTemplate::Empty,
            project_sample_rate: schema.general.project_defaults.sample_rate,
            project_bpm: 120.0,
            project_time_signature: (4, 4),
            open_project_menu: None,
            default_project_dir,
            default_dir_configured,
            default_dir_exists,
            audio_backend: SharedString::from(schema.hardware.audio.driver_type),
            audio_device_out: SharedString::from(schema.hardware.audio.device_out),
            edition_line,
            open_error: None,
            feed_state: FeedLoadState::Idle,
            feed_posts: Vec::new(),
            language,
            bootstrapped: false,
        }
    }

    /// One-shot startup work that needs a `Context`.
    ///
    /// `new()` is invoked from `cx.new(|cx| …)` in the app layer with only a
    /// `FocusHandle`, so the window cannot spawn its own tasks there. The first
    /// `render` is the earliest hook available; the guard keeps it to exactly
    /// one pass and the body starts a background task rather than doing work
    /// inline, so `render` itself stays free of I/O.
    fn bootstrap(&mut self, cx: &mut Context<Self>) {
        if self.bootstrapped {
            return;
        }
        self.bootstrapped = true;
        self.spawn_refresh_recent_missing(cx);
    }

    /// Re-derive the recent list's `missing` flags off the UI thread.
    ///
    /// Per-entry `Path::exists()` is a synchronous stat that can stall for
    /// hundreds of ms on cloud-backed (OneDrive/Dropbox placeholder) paths, so
    /// the paths are snapshotted, stat'd on the background executor, and applied
    /// on the foreground. Modelled on `StudioLayout::spawn_refresh_recent_missing`.
    fn spawn_refresh_recent_missing(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .recent_projects
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let missing: Vec<bool> = cx
                .background_executor()
                .spawn(async move { paths.iter().map(|path| !path.exists()).collect() })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A length mismatch means the list changed underneath us; drop
                // the result rather than flag the wrong rows.
                if missing.len() != this.recent_projects.len() {
                    return;
                }
                for (entry, &is_missing) in this.recent_projects.iter_mut().zip(&missing) {
                    entry.missing = is_missing;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.is_held && !is_repeatable_edit_key(event) {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key.eq_ignore_ascii_case("tab")
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.function
        {
            if modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if self.project_name_input.is_focused(window) {
            let action = self.project_name_input.handle_key_ime(event, Some(cx));
            match action {
                TextInputAction::Submit => self.create_project_from_welcome(window, cx),
                TextInputAction::Cancel => self.set_nav(StartupNav::Start, cx),
                TextInputAction::Consumed | TextInputAction::Pass => cx.notify(),
            }
            return;
        }
        if let Some(index) = welcome_shortcut_index(event) {
            self.activate_start_row(index, window, cx);
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" | "numpad_enter" => self.activate_selection(window, cx),
            "up" | "arrow_up" => self.move_selection(-1, cx),
            "down" | "arrow_down" => self.move_selection(1, cx),
            "left" | "arrow_left" => self.move_selection_column(false, cx),
            "right" | "arrow_right" => self.move_selection_column(true, cx),
            // Escape unwinds one layer at a time: the transient dropdown, then
            // the sub-pane, and only from the Start pane does it close the
            // window (which on Linux under QuitMode::LastWindowClosed quits).
            "escape" => {
                if self.open_project_menu.take().is_some() {
                    cx.notify();
                } else if self.active_nav != StartupNav::Start {
                    self.set_nav(StartupNav::Start, cx);
                } else {
                    window.remove_window();
                }
            }
            _ => {}
        }
    }

    /// Switch the centre pane. Every nav change goes through here so the Feed's
    /// lazy fetch and the Open pane's error lifetime cannot be forgotten at a
    /// call site.
    fn set_nav(&mut self, nav: StartupNav, cx: &mut Context<Self>) {
        self.active_nav = nav;
        if nav != StartupNav::OpenProject {
            self.open_error = None;
        }
        if nav == StartupNav::Feed {
            self.fetch_feed_if_needed(cx);
        }
        welcome_debug!("selected tab -> {:?}", self.active_nav);
        cx.notify();
    }

    /// The single activation path for a Start row — pointer, `Enter`, and the
    /// printed Ctrl/Cmd shortcut all land here.
    ///
    /// A template row *reveals* the New Project pane with that template
    /// preselected rather than creating a project outright, which is what makes
    /// the printed shortcut badge honest and still lets the user name the
    /// project before it exists on disk.
    fn activate_start_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let i18n = I18n::new(&self.language);
        let Some(row) = start_rows(i18n).get(index).cloned() else {
            return;
        };
        self.selected = Some(WelcomeSelection::Start(index));
        if row.action == WelcomeAction::OpenProject {
            self.set_nav(StartupNav::OpenProject, cx);
            return;
        }
        if let Some(template) = template_for_action(&row.action) {
            self.selected_template = template;
            self.project_bpm = template.default_bpm();
            self.project_time_signature = template.time_signature();
        }
        self.set_nav(StartupNav::NewProject, cx);
        // Naming the project is the pane's whole purpose, and the shortcut path
        // never touches the pointer, so land the caret in the field.
        window.focus(&self.project_name_input.focus_handle, cx);
    }

    fn activate_recent(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(recent) = self.recent_projects.get(index) else {
            return;
        };
        let missing = recent.missing;
        let path = recent.path.clone();
        self.selected = Some(WelcomeSelection::Recent(index));
        cx.notify();
        if missing {
            // The row is already labelled and non-interactive; refuse rather
            // than hand the app a path that is not there.
            welcome_debug!("recent project ignored (missing) -> {}", path.display());
            return;
        }
        welcome_debug!("recent project activated -> {}", path.display());
        (self.callbacks.on_action)(WelcomeAction::OpenRecent(path), window, cx);
    }

    fn activate_continue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(WelcomeSelection::Continue);
        cx.notify();
        (self.callbacks.on_action)(WelcomeAction::OpenEmptyWorkspace, window, cx);
    }

    fn activate_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Some(WelcomeSelection::Start(index)) => self.activate_start_row(index, window, cx),
            Some(WelcomeSelection::Recent(index)) => self.activate_recent(index, window, cx),
            Some(WelcomeSelection::Continue) => self.activate_continue(window, cx),
            None => {}
        }
    }

    /// Move the Start pane's selection one step, within whichever of the two
    /// columns the selection currently sits in.
    ///
    /// "Continue Without Project" is deliberately *not* in this ring: it is a
    /// button with its own focus ring rather than a row with a selected state,
    /// and a selection that moved onto something with no visible selected state
    /// would be a selection the user cannot see.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.active_nav != StartupNav::Start {
            return;
        }
        let recent_len = self.recent_projects.len();
        let next = match self.selected {
            Some(WelcomeSelection::Recent(index)) if recent_len > 0 => {
                let last = recent_len as isize - 1;
                let target = (index as isize + delta).clamp(0, last) as usize;
                WelcomeSelection::Recent(target)
            }
            Some(WelcomeSelection::Start(index)) => {
                let last = START_ROW_COUNT as isize - 1;
                let target = (index as isize + delta).clamp(0, last) as usize;
                WelcomeSelection::Start(target)
            }
            // Continue, an out-of-range recent index, or nothing selected: land
            // on the first start row rather than guess.
            _ => WelcomeSelection::Start(0),
        };
        self.selected = Some(next);
        cx.notify();
    }

    /// Left/Right cross between the Start pane's two columns, which is the only
    /// way to reach the Recent list from the keyboard without Tab.
    fn move_selection_column(&mut self, to_recent: bool, cx: &mut Context<Self>) {
        if self.active_nav != StartupNav::Start {
            return;
        }
        let in_recent = matches!(self.selected, Some(WelcomeSelection::Recent(_)));
        if to_recent && !in_recent && !self.recent_projects.is_empty() {
            self.selected = Some(WelcomeSelection::Recent(0));
            cx.notify();
        } else if !to_recent && in_recent {
            self.selected = Some(WelcomeSelection::Start(0));
            cx.notify();
        }
    }

    /// `sanitize_project_name` never returns empty — it substitutes
    /// "Untitled Project" — so the real "did the user type a name?" question is
    /// whether anything survives the same trim that function applies.
    fn project_name_is_valid(&self) -> bool {
        !self
            .project_name_input
            .value
            .trim_matches(|c: char| c == ' ' || c == '.')
            .is_empty()
    }

    fn create_project_from_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.project_name_is_valid() {
            // The button is disabled and the field is labelled; Enter must not
            // quietly create "Untitled Project" behind the user's back.
            cx.notify();
            return;
        }
        let name = crate::project::io::sanitize_project_name(&self.project_name_input.value);
        let options = ProjectCreateOptions {
            name,
            base_dir: self.default_project_dir.clone(),
            template: self.selected_template,
            sample_rate: self.project_sample_rate,
            bpm: self.project_bpm,
            time_signature_num: self.project_time_signature.0,
            time_signature_den: self.project_time_signature.1,
        };
        welcome_debug!(
            "create project requested name={} dir={} template={}",
            options.name,
            options.base_dir.display(),
            options.template.label()
        );
        (self.callbacks.on_action)(WelcomeAction::CreateProject(options), window, cx);
    }

    /// Open a folder picker to choose the default project directory. Persists
    /// the choice to settings on success. If the picker is unavailable or the
    /// user cancels, nothing changes (no faked success).
    fn change_default_dir(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let start_dir = self.default_project_dir.clone();
            let title = I18n::new(&self.language).tr("dialog.choose-project-location");
            let entity = cx.entity().clone();
            cx.spawn(async move |_this, cx| {
                let result = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .set_directory(&start_dir)
                    .pick_folder()
                    .await;
                let Some(handle) = result else {
                    welcome_debug!("default project path change cancelled");
                    return;
                };
                let path = handle.path().to_path_buf();
                // Best-effort: create the folder now so it is ready for new projects.
                let created = std::fs::create_dir_all(&path).is_ok();
                SettingsSchema::persist_default_project_directory(Some(path.clone()));
                welcome_debug!("default project path changed -> {}", path.display());
                let _ = entity.update(cx, |this, cx| {
                    this.default_project_dir = path;
                    this.default_dir_configured = true;
                    this.default_dir_exists = created;
                    cx.notify();
                });
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            self.open_error = Some(SharedString::from(no_native_dialogs_message(I18n::new(
                &self.language,
            ))));
            cx.notify();
        }
    }

    /// Load the public PayloadCMS feed once per Welcome window. Network work stays
    /// on GPUI's background executor and uses the blocking HTTP client so it does
    /// not require a Tokio runtime on the UI/task executors.
    ///
    /// Driven from the Feed rail entry and from Retry — never from `render`,
    /// which must stay free of network, filesystem and decoding work.
    fn fetch_feed_if_needed(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.feed_state, FeedLoadState::Idle) {
            return;
        }
        self.feed_state = FeedLoadState::Loading;
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { fetch_feed_posts() })
                .await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok(posts) => {
                        this.feed_posts = posts;
                        this.feed_state = FeedLoadState::Loaded;
                    }
                    Err(error) => {
                        this.feed_posts.clear();
                        this.feed_state = FeedLoadState::Failed(SharedString::from(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A failed feed used to be terminal because nothing ever reset the state.
    fn retry_feed(&mut self, cx: &mut Context<Self>) {
        self.feed_state = FeedLoadState::Idle;
        self.feed_posts.clear();
        self.fetch_feed_if_needed(cx);
        cx.notify();
    }

    fn browse_and_open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_error = None;
        cx.notify();
        #[cfg(feature = "native-dialogs")]
        {
            let start_dir = self.default_project_dir.clone();
            let on_action = self.callbacks.on_action.clone();
            let i18n = I18n::new(&self.language);
            let title = i18n.tr("dialog.open-project");
            let project_filter = i18n.tr("dialog.filter.futureboard-project");
            // `spawn_in` keeps a window handle across the async picker await, so the
            // validated path can be handed to `on_action` (which needs a Window to
            // close Welcome) once the picker resolves.
            cx.spawn_in(window, async move |this, cx| {
                let result = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .set_directory(&start_dir)
                    .add_filter(
                        project_filter,
                        crate::project::io::SUPPORTED_PROJECT_FILE_EXTS,
                    )
                    // Projects exported by another DAW open through the
                    // importers in `crate::project::import`.
                    .add_filter(
                        "Cubase XML Track Archive",
                        crate::project::IMPORT_PROJECT_FILE_EXTS,
                    )
                    .pick_file()
                    .await;
                let Some(handle) = result else {
                    welcome_debug!("open project cancelled");
                    return;
                };
                let path = handle.path().to_path_buf();
                match crate::project::validate_project_file(&path) {
                    Ok(version) => {
                        welcome_debug!("open project validated (v{version}) -> {}", path.display());
                        // Hand off to the app: opens the studio loading `path` and
                        // closes Welcome (see the on_action callback in app.rs).
                        let _ = cx.update(|window, app| {
                            on_action(WelcomeAction::OpenProjectFile(path), window, app);
                        });
                    }
                    Err(e) => {
                        let msg = format!("{} Details: {}", e.user_message(), e.technical_detail());
                        welcome_debug!("open project rejected -> {msg}");
                        let _ = this.update(cx, |this, cx| {
                            this.open_error = Some(SharedString::from(msg));
                            this.active_nav = StartupNav::OpenProject;
                            cx.notify();
                        });
                    }
                }
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            let _ = window;
            self.open_error = Some(SharedString::from(no_native_dialogs_message(I18n::new(
                &self.language,
            ))));
            self.active_nav = StartupNav::OpenProject;
            cx.notify();
        }
    }
}

/// Map a keystroke onto a Start row index via [`START_SHORTCUTS`], so the badge
/// and the binding are read from the same table.
fn welcome_shortcut_index(event: &KeyDownEvent) -> Option<usize> {
    let modifiers = event.keystroke.modifiers;
    if !(modifiers.control || modifiers.platform) || modifiers.alt {
        return None;
    }
    let key = event.keystroke.key.as_str();
    START_SHORTCUTS
        .iter()
        .position(|(candidate, shift)| *candidate == key && *shift == modifiers.shift)
}

// Route platform IME (CJK/Thai composition + candidate-window positioning) to
// the project-name field. Coexists with handle_key_with_clipboard; GPUI
// suppresses key dispatch for keystrokes the IME consumes.
crate::impl_single_input_window_ime!(WelcomeWindow, project_name_input);

impl Render for WelcomeWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.bootstrap(cx);

        let i18n = I18n::new(&self.language);
        let target = cx.entity().clone();
        let project_name_callbacks =
            bind_mouse_selection(target.clone(), |this| &mut this.project_name_input);
        div()
            .id("futureboard-welcome-root")
            .role(Role::Application)
            .aria_label("Futureboard Studio Welcome")
            .key_context("WelcomeWindow")
            .capture_key_down(move |event, window, cx| {
                let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
            })
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_window())
            .cursor(gpui::CursorStyle::Arrow)
            // Swallow the platform's default click-drag gesture on the Welcome
            // root so static labels/rows never render as "selected" while the
            // user drags across them (GPUI has no CSS `user-select`). Rows and
            // buttons register their own handlers deeper in the tree and fire
            // first (bubble order), so clicks are unaffected. `TextInputState`
            // calls `cx.stop_propagation()` on its own mouse-down, so this never
            // reaches (and never affects) the project-name field, which keeps its
            // own IBeam cursor and drag-to-select behavior.
            .on_mouse_down(MouseButton::Left, |_event, window, _cx| {
                window.prevent_default();
            })
            .child(welcome_chrome(window))
            .child(self.render_welcome(window, cx, project_name_callbacks, i18n))
            // The account menu belongs to the window that draws the chip. It is
            // anchored from this element — whose origin is the window's
            // top-left — so the dismiss backdrop covers the window rather than
            // the 32 px band the chip sits in. The anchor is the chrome band's
            // height: there is no second header below it.
            .children(crate::components::app_chrome::account_menu_overlay(
                window,
                PlatformChromePolicy::current().titlebar_height_px,
                space::BASE,
            ))
    }
}

impl WelcomeWindow {
    fn render_welcome(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
        project_name_callbacks: TextInputCallbacks,
        i18n: I18n,
    ) -> gpui::AnyElement {
        // Layout owner for the whole body. The rail is fixed; the centre pane
        // takes the rest and is the only region that scrolls.
        let viewport_width: f32 = window.viewport_size().width.into();
        let side_by_side = (viewport_width - RAIL_WIDTH) >= START_STACK_BREAKPOINT;

        // Bound to a local first: a `match` arm below takes `&mut self`, which
        // it cannot do while the scrutinee is still a place expression on self.
        let nav = self.active_nav;
        let center = match nav {
            StartupNav::Start => self.start_pane(cx, side_by_side, i18n),
            StartupNav::NewProject => new_project_pane(
                cx,
                &self.project_name_input,
                self.project_name_input.is_focused(window),
                self.project_name_is_valid(),
                project_name_callbacks,
                self.selected_template,
                ProjectFieldValues {
                    sample_rate: self.project_sample_rate,
                    bpm: self.project_bpm,
                    time_signature: self.project_time_signature,
                    open_menu: self.open_project_menu,
                },
                LocationState {
                    dir: self.default_project_dir.clone(),
                    configured: self.default_dir_configured,
                    exists: self.default_dir_exists,
                },
                i18n,
            ),
            StartupNav::OpenProject => open_project_pane(cx, self.open_error.clone(), i18n),
            StartupNav::Feed => feed_pane(cx, &self.feed_state, &self.feed_posts, i18n),
        };

        let dismiss_target = cx.entity().clone();
        let menu_open = self.open_project_menu.is_some();

        div()
            // Anchor plane for the New Project dropdowns' click-outside
            // backdrop, which must cover the whole start-screen body.
            .relative()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .bg(Colors::surface_base())
            .child(left_rail(
                cx,
                self.active_nav,
                &self.callbacks,
                self.audio_backend.clone(),
                self.audio_device_out.clone(),
                self.edition_line.clone(),
                i18n,
            ))
            .child(center)
            .when(menu_open, |root| {
                root.child(select_dismiss_backdrop(Arc::new(
                    move |_: &(), _window, cx| {
                        let _ = dismiss_target.update(cx, |this, cx| {
                            this.open_project_menu = None;
                            cx.notify();
                        });
                    },
                )))
            })
            .into_any_element()
    }
}

// ── Chrome band ──────────────────────────────────────────────────────────────

/// The screen's only chrome row: wordmark, drag region, account chip, caption
/// controls. One 32 px band rather than a titlebar plus a second brand header,
/// laid out on the three-track pattern the Studio shell uses.
///
/// Square by contract — the chrome row is on DESIGN.md's must-stay-square list —
/// and safe to tag `WindowControlArea::Drag` as a whole because every
/// interactive child (`account_chip`, `window_control_button`) calls
/// `.occlude()`, which breaks hit-test iteration before it reaches this element.
fn welcome_chrome(window: &Window) -> impl IntoElement {
    let policy = PlatformChromePolicy::current();

    let mut chrome = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(policy.titlebar_height_px))
        .w_full()
        .flex_none()
        .pl(policy.traffic_light_left_padding())
        .bg(Colors::surface_titlebar())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .rounded(px(radius::NONE))
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, begin_titlebar_drag)
        .child(
            // Shrinks (and clips the wordmark) before the caption controls are
            // pushed off a narrow window: predictable truncation, not reflow.
            div()
                .flex()
                .flex_row()
                .items_center()
                .min_w(px(0.0))
                .overflow_hidden()
                .px(px(space::LOOSE))
                .child(
                    img(SharedString::from(LOGO_TEXT_PATH))
                        .w(px(LOGO_WIDTH))
                        .h(px(LOGO_HEIGHT))
                        .flex_none(),
                ),
        )
        .child(draggable_spacer());

    if let Some(account) = crate::components::app_chrome::account_chip() {
        chrome = chrome.child(section_separator()).child(account);
    }

    if policy.show_window_controls {
        let (max_path, max_fallback) = if window.is_maximized() {
            (assets::ICON_RESTORE_PATH, "RESTORE")
        } else {
            (assets::ICON_MAXIMIZE_PATH, "MAX")
        };
        chrome = chrome
            .child(section_separator())
            .child(window_control_button(
                WindowControlArea::Min,
                assets::ICON_MINIMIZE_PATH,
                "-",
            ))
            .child(window_control_button(
                WindowControlArea::Max,
                max_path,
                max_fallback,
            ))
            .child(window_control_button(
                WindowControlArea::Close,
                assets::ICON_X_PATH,
                "X",
            ));
    }
    chrome
}

// ── Navigation rail ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn left_rail(
    cx: &mut Context<WelcomeWindow>,
    active: StartupNav,
    callbacks: &WelcomeCallbacks,
    audio_backend: SharedString,
    audio_device_out: SharedString,
    edition_line: SharedString,
    i18n: I18n,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w(px(RAIL_WIDTH))
        .flex_none()
        .min_h_0()
        .border_r(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_sidebar())
        .px(px(space::SNUG))
        .py(px(space::BASE))
        .gap(px(space::HAIR))
        .child(rail_item(
            cx,
            StartupNav::Start,
            "welcome-rail-start",
            i18n.tr("welcome.nav.start"),
            assets::ICON_STAR_PATH,
            active,
            None,
        ))
        .child(rail_item(
            cx,
            StartupNav::NewProject,
            "welcome-rail-new",
            i18n.tr("welcome.nav.new"),
            assets::ICON_PLUS_PATH,
            active,
            None,
        ))
        .child(rail_item(
            cx,
            StartupNav::OpenProject,
            "welcome-rail-open",
            i18n.tr("welcome.open-project"),
            assets::ICON_FOLDER_OPEN_PATH,
            active,
            None,
        ))
        .child(rail_item(
            cx,
            StartupNav::Feed,
            "welcome-rail-feed",
            i18n.tr("welcome.nav.feed"),
            assets::ICON_NEWSPAPER_PATH,
            active,
            None,
        ))
        .child(div().flex_1())
        .child(rail_foot(
            cx,
            callbacks,
            audio_backend,
            audio_device_out,
            edition_line,
            active,
            i18n,
        ))
}

/// One rail entry.
///
/// Selection is a `state.selected` fill plus a leading-edge accent overlay —
/// never a grown border, which would reflow the row on every tab change. The
/// overlay is inset by exactly the corner radius so it stays on the straight
/// part of the left edge.
fn rail_item(
    cx: &mut Context<WelcomeWindow>,
    nav: StartupNav,
    id: &'static str,
    label: impl Into<SharedString>,
    icon: &'static str,
    active: StartupNav,
    action: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
) -> impl IntoElement {
    let label = label.into();
    // Action rows (the edition footer action) never show the active highlight —
    // only real tabs do.
    let changes_nav = action.is_none();
    let is_active = changes_nav && active == nav;

    let plane = Colors::surface_sidebar();
    let rest = if is_active {
        Colors::composite(plane, Colors::state_selected())
    } else {
        Colors::with_alpha(plane, 0.0)
    };
    let hover_base = if is_active { rest } else { plane };
    let hover = Colors::composite(hover_base, Colors::state_hover());
    let pressed = Colors::composite(hover_base, Colors::state_recessed());
    let focus = Colors::state_focus_ring();
    let tint = if is_active {
        Colors::text_primary()
    } else {
        Colors::text_muted()
    };

    let target = cx.entity().clone();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_selected(is_active)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .relative()
        .flex()
        .items_center()
        .gap(px(space::BASE))
        .h(px(size::DEFAULT))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .on_click(move |_event, window, cx| {
            if changes_nav {
                let _ = target.update(cx, |this, cx| this.set_nav(nav, cx));
            }
            if let Some(callback) = &action {
                callback(window, cx);
            }
        })
        .when(is_active, |item| {
            item.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(radius::CONTROL))
                    .bottom(px(radius::CONTROL))
                    .w(px(SELECTION_MARKER_WIDTH))
                    .bg(Colors::accent_primary()),
            )
        })
        .child(
            svg()
                .path(icon)
                .w(px(RAIL_ICON))
                .h(px(RAIL_ICON))
                .flex_none()
                .text_color(tint),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_weight(if is_active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(tint)
                .child(label),
        )
}

/// The quiet foot of the rail: what the machine will play through, what build
/// this is, and the edition's own action. All read-only status except the last,
/// and all of it real — the audio pair is the configured device from settings,
/// the version pair comes from the edition provider.
#[allow(clippy::too_many_arguments)]
fn rail_foot(
    cx: &mut Context<WelcomeWindow>,
    callbacks: &WelcomeCallbacks,
    audio_backend: SharedString,
    audio_device_out: SharedString,
    edition_line: SharedString,
    active: StartupNav,
    i18n: I18n,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .pt(px(space::BASE))
        .child(div().px(px(space::SNUG)).child(fb_section_header(
            i18n.tr("welcome.nav.audio").to_uppercase(),
        )))
        .child(status_line(
            i18n.tr("settings.field.backend"),
            audio_backend,
        ))
        .child(status_line(
            i18n.tr("settings.field.output-device"),
            audio_device_out,
        ))
        .child(
            div()
                .my(px(space::SNUG))
                .mx(px(space::SNUG))
                .h(px(1.0))
                .bg(Colors::divider()),
        )
        .child(
            div()
                .px(px(space::SNUG))
                .truncate()
                .text_size(px(typography::DENSE_CAPTION))
                .font_features(tabular_features())
                .text_color(Colors::text_faint())
                .child(edition_line),
        )
        .when_some(callbacks.footer_action.clone(), |foot, action| {
            foot.child(rail_item(
                cx,
                StartupNav::Start,
                "welcome-rail-footer",
                action.label.to_string(),
                action.icon,
                active,
                Some(action.on_click),
            ))
        })
}

/// A read-only `label / value` pair in the rail foot. The value truncates
/// rather than wraps; device names are long and the rail width is fixed.
fn status_line(label: impl Into<SharedString>, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .px(px(space::SNUG))
        .py(px(space::HAIR))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_faint())
                .child(label.into()),
        )
        .child(
            div()
                .truncate()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_secondary())
                .child(value.into()),
        )
}

// ── Start pane ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StartRow {
    title: String,
    description: String,
    icon: &'static str,
    action: WelcomeAction,
}

/// The Start rows, in the order [`START_SHORTCUTS`] indexes them.
///
/// Titles and descriptions come from the already-translated `template.*`
/// namespace rather than a second English table: `template_for_action` maps
/// these rows onto templates one-for-one, so the two must name the same things.
fn start_rows(i18n: I18n) -> Vec<StartRow> {
    let mut rows: Vec<StartRow> = [
        (
            ProjectTemplate::Empty,
            assets::ICON_PLUS_PATH,
            WelcomeAction::EmptyProject,
        ),
        (
            ProjectTemplate::BeatMaking,
            assets::ICON_MUSIC_PATH,
            WelcomeAction::MidiComposer,
        ),
        (
            ProjectTemplate::Recording,
            assets::ICON_MIC_PATH,
            WelcomeAction::AudioSession,
        ),
        (
            ProjectTemplate::Mixing,
            assets::ICON_SLIDERS_HORIZONTAL_PATH,
            WelcomeAction::MixTemplate,
        ),
    ]
    .into_iter()
    .map(|(template, icon, action)| StartRow {
        title: template_label(template, i18n),
        description: template_description(template, i18n),
        icon,
        action,
    })
    .collect();

    rows.push(StartRow {
        title: i18n.tr("welcome.open-project"),
        description: i18n.tr_or("welcome.start.open-desc", "Choose an existing project file"),
        icon: assets::ICON_FOLDER_OPEN_PATH,
        action: WelcomeAction::OpenProject,
    });
    rows
}

/// Shortcut badge text for a Start row, read from the same table the key
/// handler reads.
fn shortcut_label(index: usize) -> String {
    let modifier = if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    };
    match START_SHORTCUTS.get(index) {
        Some((key, shift)) if *shift => {
            format!("{modifier} + Shift + {}", key.to_uppercase())
        }
        Some((key, _)) => format!("{modifier} + {}", key.to_uppercase()),
        None => String::new(),
    }
}

impl WelcomeWindow {
    /// Start pane: two lists side by side, or stacked under one scroller when
    /// the centre is too narrow to carry both.
    ///
    /// No `FbButtonKind::Primary` lives here — the rows *are* the actions, and
    /// the accent is spent on selection and focus alone.
    fn start_pane(
        &mut self,
        cx: &mut Context<Self>,
        side_by_side: bool,
        i18n: I18n,
    ) -> gpui::AnyElement {
        let start_column = self.start_column(cx, side_by_side, i18n);
        let recent_column = self.recent_column(cx, side_by_side, i18n);

        let body = if side_by_side {
            // Two columns, each list its own scroll owner, both stretched to
            // the body's height by the flex default.
            div()
                .id("welcome-start-body")
                .flex()
                .flex_row()
                .gap(px(space::BLOCK))
                .flex_1()
                .min_h_0()
                .child(start_column)
                .child(recent_column)
        } else {
            // Too narrow for two columns: stack them under one scroll owner,
            // and let the lists take their natural height inside it.
            div()
                .id("welcome-start-body")
                .flex()
                .flex_col()
                .gap(px(space::BLOCK))
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(start_column)
                .child(recent_column)
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p(px(space::BLOCK))
            .bg(Colors::surface_base())
            .child(body)
            .into_any_element()
    }

    fn start_column(
        &mut self,
        cx: &mut Context<Self>,
        side_by_side: bool,
        i18n: I18n,
    ) -> gpui::AnyElement {
        let paint = row_paint(Colors::surface_input());
        let selected = self.selected;

        let mut well = list_well("welcome-start-list").when(side_by_side, |well| {
            well.flex_1().min_h_0().overflow_y_scroll()
        });
        for (index, row) in start_rows(i18n).into_iter().enumerate() {
            let is_selected = selected == Some(WelcomeSelection::Start(index));
            let target = cx.entity().clone();
            if index > 0 {
                well = well.child(row_divider());
            }
            well = well.child(start_row(
                index,
                row,
                is_selected,
                paint,
                move |window, cx| {
                    let _ =
                        target.update(cx, |this, cx| this.activate_start_row(index, window, cx));
                },
            ));
        }

        let continue_target = cx.entity().clone();

        div()
            .flex()
            .flex_col()
            .gap(px(space::LOOSE))
            .when(side_by_side, |column| column.flex_1().min_w_0().min_h_0())
            .when(!side_by_side, |column| column.w_full().flex_none())
            .child(fb_section_header(
                i18n.tr("welcome.nav.start").to_uppercase(),
            ))
            .child(well)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(fb_button(
                        "welcome-continue",
                        i18n.tr("welcome.button.continue-without-project"),
                        FbButtonKind::Default,
                        true,
                        move |_event, window, cx| {
                            let _ = continue_target
                                .update(cx, |this, cx| this.activate_continue(window, cx));
                        },
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(i18n.tr_or(
                                "welcome.continue.description",
                                "Work in an unsaved session until you save",
                            )),
                    ),
            )
            .into_any_element()
    }

    fn recent_column(
        &mut self,
        cx: &mut Context<Self>,
        side_by_side: bool,
        i18n: I18n,
    ) -> gpui::AnyElement {
        let paint = row_paint(Colors::surface_input());
        let selected = self.selected;

        let mut well = list_well("welcome-recent-list").when(side_by_side, |well| {
            well.flex_1().min_h_0().overflow_y_scroll()
        });
        if self.recent_projects.is_empty() {
            well = well.child(empty_state(i18n.tr("menu.file-open_recent-empty")));
        } else {
            for (index, item) in self.recent_projects.iter().enumerate() {
                let is_selected = selected == Some(WelcomeSelection::Recent(index));
                let target = cx.entity().clone();
                if index > 0 {
                    well = well.child(row_divider());
                }
                well = well.child(recent_row(
                    index,
                    item,
                    is_selected,
                    paint,
                    i18n,
                    move |window, cx| {
                        let _ =
                            target.update(cx, |this, cx| this.activate_recent(index, window, cx));
                    },
                ));
            }
        }

        div()
            .flex()
            .flex_col()
            .gap(px(space::LOOSE))
            .min_h_0()
            .when(side_by_side, |column| {
                column.w(px(RECENT_COLUMN_WIDTH)).flex_none()
            })
            .when(!side_by_side, |column| column.w_full().flex_none())
            .child(fb_section_header(
                i18n.tr("welcome.nav.recent").to_uppercase(),
            ))
            .child(well)
            .into_any_element()
    }
}

/// A start row: glyph, title, description, shortcut badge.
///
/// Full-bleed and square (`radius::NONE`). Every state paints on this same
/// rectangle: hover and pressed as composited fills, selection as a
/// `state.selected` fill plus the leading accent bar.
fn start_row(
    index: usize,
    row: StartRow,
    selected: bool,
    paint: RowPaint,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let shortcut = shortcut_label(index);
    let accessible_label = format!("{}. {}. {}", row.title, row.description, shortcut);
    let rest = paint.rest_for(selected);
    let hover = paint.hover_for(selected);
    let pressed = paint.pressed_for(selected);
    let focus = Colors::state_focus_ring();
    let tint = if selected {
        Colors::text_primary()
    } else {
        Colors::text_muted()
    };

    div()
        .id(("welcome-start-row", index))
        .role(Role::Button)
        .aria_label(accessible_label)
        .aria_selected(selected)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .w_full()
        .h(px(LIST_ROW_HEIGHT))
        // Rows are a fixed band: without this a full list inside a fixed-height
        // well would flex-shrink every row instead of scrolling.
        .flex_none()
        .px(px(space::LOOSE))
        .rounded(px(radius::NONE))
        .bg(rest)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .on_click(move |_event, window, cx| on_activate(window, cx))
        .when(selected, |item| item.child(selection_marker()))
        .child(
            svg()
                .path(row.icon)
                .w(px(ROW_ICON))
                .h(px(ROW_ICON))
                .flex_none()
                .text_color(tint),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(row.title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(row.description),
                ),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(typography::DENSE_CAPTION))
                .font_features(tabular_features())
                .text_color(Colors::text_faint())
                .child(shortcut),
        )
}

/// A recent-project row: name and relative date, then the full path.
///
/// A missing project is carried on two channels — a warning badge *and* a
/// disabled label — rather than by dimming the whole row, and it takes no
/// pointer affordance because there is nothing to open.
fn recent_row(
    index: usize,
    recent: &RecentProject,
    selected: bool,
    paint: RowPaint,
    i18n: I18n,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let name = recent.name.clone();
    let path_label = recent.path.to_string_lossy().to_string();
    let missing = recent.missing;
    let last_opened = format_last_opened(recent.last_opened_at, i18n);
    let accessible_label = if missing {
        format!("{name}. {}", i18n.tr("project.file.missing"))
    } else {
        format!("{name}. {path_label}")
    };
    let rest = paint.rest_for(selected);
    let hover = paint.hover_for(selected);
    let pressed = paint.pressed_for(selected);
    let focus = Colors::state_focus_ring();

    div()
        .id(("welcome-recent-row", index))
        .role(Role::Button)
        .aria_label(accessible_label)
        .aria_selected(selected)
        .aria_disabled(missing)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .relative()
        .flex()
        .flex_col()
        .justify_center()
        .w_full()
        .h(px(LIST_ROW_HEIGHT))
        .flex_none()
        .px(px(space::LOOSE))
        .rounded(px(radius::NONE))
        .bg(rest)
        .when(!missing, |row| {
            row.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
                .on_click(move |_event, window, cx| on_activate(window, cx))
        })
        .when(missing, |row| row.cursor(gpui::CursorStyle::Arrow))
        .when(selected, |row| row.child(selection_marker()))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(
                    div()
                        .truncate()
                        .min_w_0()
                        .flex_1()
                        .text_size(px(typography::UI_SM))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if missing {
                            Colors::text_disabled()
                        } else {
                            Colors::text_primary()
                        })
                        .child(name),
                )
                .when(missing, |row| {
                    row.child(div().flex_none().child(fb_badge(
                        i18n.tr("project.file.missing"),
                        Colors::semantic_warning(),
                    )))
                })
                .when(!missing && !last_opened.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_size(px(typography::DENSE_CAPTION))
                            .font_features(tabular_features())
                            .text_color(Colors::text_muted())
                            .child(last_opened.clone()),
                    )
                }),
        )
        .child(
            div()
                .truncate()
                .text_size(px(typography::UI_XS))
                .font_features(tabular_features())
                .text_color(if missing {
                    Colors::text_disabled()
                } else {
                    Colors::text_faint()
                })
                .child(path_label),
        )
}

// ── New Project pane ─────────────────────────────────────────────────────────

/// The project-format fields the New Project pane edits, plus which of their
/// dropdowns is open. Grouped so the pane keeps one argument for "the format of
/// the project being created" instead of four positional ones.
#[derive(Clone, Copy)]
struct ProjectFieldValues {
    sample_rate: u32,
    bpm: f32,
    time_signature: (u32, u32),
    open_menu: Option<ProjectFieldMenu>,
}

/// Where the project will land, and whether that folder is real yet.
#[derive(Clone)]
struct LocationState {
    dir: PathBuf,
    configured: bool,
    exists: bool,
}

/// One labelled dropdown row in the New Project pane. Every option commits
/// straight into `WelcomeWindow`'s real create-project state, so what the pane
/// shows is what [`WelcomeWindow::create_project_from_welcome`] sends.
fn project_field_row(
    cx: &mut Context<WelcomeWindow>,
    id: &'static str,
    label: impl Into<String>,
    menu: ProjectFieldMenu,
    open_menu: Option<ProjectFieldMenu>,
    selected_id: String,
    options: Vec<SelectOption>,
    apply: impl Fn(&mut WelcomeWindow, &str) + 'static,
) -> impl IntoElement {
    let toggle_target = cx.entity().clone();
    let change_target = cx.entity().clone();
    fb_form_row(
        label,
        select(
            id,
            Some(selected_id.as_str()),
            "-",
            options,
            open_menu == Some(menu),
            false,
            Arc::new(move |_: &(), _window, cx| {
                let _ = toggle_target.update(cx, |this, cx| {
                    this.open_project_menu = if this.open_project_menu == Some(menu) {
                        None
                    } else {
                        Some(menu)
                    };
                    cx.notify();
                });
            }),
            Arc::new(move |value: &String, _window, cx| {
                let value = value.clone();
                let _ = change_target.update(cx, |this, cx| {
                    apply(this, &value);
                    this.open_project_menu = None;
                    cx.notify();
                });
            }),
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn new_project_pane(
    cx: &mut Context<WelcomeWindow>,
    project_name_input: &TextInputState,
    name_focused: bool,
    name_valid: bool,
    name_callbacks: TextInputCallbacks,
    selected_template: ProjectTemplate,
    fields: ProjectFieldValues,
    location: LocationState,
    i18n: I18n,
) -> gpui::AnyElement {
    let ProjectFieldValues {
        sample_rate,
        bpm,
        time_signature,
        open_menu,
    } = fields;
    let safe_name = crate::project::io::sanitize_project_name(&project_name_input.value);
    let preview = location.dir.join(&safe_name).to_string_lossy().to_string();
    let ime_target = cx.entity().clone();
    let create_target = cx.entity().clone();
    let continue_target = cx.entity().clone();
    let browse_target = cx.entity().clone();

    // Template picker. Shared inner edges of a segmented control must stay
    // square, which is exactly what `FbSegment` encodes; the track's
    // `space::TIGHT` inset is what makes `radius::inner(SURFACE, TIGHT)` land on
    // `radius::CONTROL` for the segments.
    let last_template = PROJECT_TEMPLATES.len() - 1;
    let mut template_track = fb_segmented_track().w_full();
    for (index, template) in PROJECT_TEMPLATES.into_iter().enumerate() {
        let position = if index == 0 {
            FbSegment::First
        } else if index == last_template {
            FbSegment::Last
        } else {
            FbSegment::Middle
        };
        let target = cx.entity().clone();
        template_track = template_track.child(fb_segment(
            ("welcome-template", index),
            template_label(template, i18n),
            selected_template == template,
            position,
            move |_event, _window, cx| {
                let _ = target.update(cx, |this, cx| {
                    this.selected_template = template;
                    this.project_bpm = template.default_bpm();
                    this.project_time_signature = template.time_signature();
                    cx.notify();
                });
            },
        ));
    }

    // The location plate is a recessed group: the resolved path plus the one
    // control that changes it. `fb_button` already draws `radius::CONTROL`,
    // which is `radius::inner(SURFACE, TIGHT)` — concentric by construction.
    let location_plate = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .w_full()
        .p(px(space::TIGHT))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .px(px(space::BASE))
                .text_size(px(typography::UI_XS))
                .font_features(tabular_features())
                .text_color(Colors::text_secondary())
                .child(preview),
        )
        .child(fb_button(
            "welcome-change-default-dir",
            i18n.tr("wizard.button.browse"),
            FbButtonKind::Default,
            true,
            move |_event, _window, cx| {
                let _ = browse_target.update(cx, |this, cx| this.change_default_dir(cx));
            },
        ));

    let mut form = div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .w_full()
        .child(fb_form_row(
            i18n.tr("wizard.field.name"),
            text_field_with_callbacks_and_ime(
                project_name_input,
                name_focused,
                name_callbacks,
                ime_target,
            ),
        ));

    if !name_valid {
        form = form.child(field_note(
            i18n.tr("wizard.error.name-required"),
            Colors::status_error(),
        ));
    }

    form = form
        .child(fb_form_row(
            i18n.tr("wizard.field.location"),
            location_plate,
        ))
        .when(!location.configured || !location.exists, |form| {
            form.child(field_note(
                i18n.tr_or(
                    "welcome.location.will-create",
                    "This folder is created when the project is saved.",
                ),
                Colors::text_faint(),
            ))
        })
        .child(fb_form_row(
            i18n.tr("wizard.summary.template"),
            template_track,
        ))
        .child(project_field_row(
            cx,
            "welcome-project-sample-rate",
            i18n.tr("wizard.field.sample-rate"),
            ProjectFieldMenu::SampleRate,
            open_menu,
            sample_rate.to_string(),
            PROJECT_SAMPLE_RATES
                .iter()
                .map(|rate| SelectOption::new(rate.to_string(), format!("{rate} Hz")))
                .collect(),
            |this, value| {
                if let Ok(rate) = value.parse::<u32>() {
                    this.project_sample_rate = rate;
                }
            },
        ))
        .child(project_field_row(
            cx,
            "welcome-project-bpm",
            i18n.tr("wizard.field.tempo"),
            ProjectFieldMenu::Bpm,
            open_menu,
            format!("{bpm:.0}"),
            PROJECT_BPM_PRESETS
                .iter()
                .map(|preset| SelectOption::new(format!("{preset:.0}"), format!("{preset:.0} BPM")))
                .collect(),
            |this, value| {
                if let Ok(parsed) = value.parse::<f32>() {
                    this.project_bpm = parsed.clamp(20.0, 999.0);
                }
            },
        ))
        .child(project_field_row(
            cx,
            "welcome-project-time-signature",
            i18n.tr("wizard.field.time-signature"),
            ProjectFieldMenu::TimeSignature,
            open_menu,
            format!("{}/{}", time_signature.0, time_signature.1),
            PROJECT_TIME_SIGNATURES
                .iter()
                .map(|(num, den)| {
                    let label = format!("{num}/{den}");
                    SelectOption::new(label.clone(), label)
                })
                .collect(),
            |this, value| {
                if let Some((num, den)) = value.split_once('/') {
                    if let (Ok(num), Ok(den)) = (num.parse::<u32>(), den.parse::<u32>()) {
                        this.project_time_signature = (num.max(1), den.max(1));
                    }
                }
            },
        ));

    pane_scroller("welcome-new-pane")
        .child(
            pane_body()
                .child(pane_header(
                    i18n.tr("wizard.title"),
                    i18n.tr_or(
                        "welcome.new.subtitle",
                        "Name it, choose a template, and start.",
                    ),
                ))
                .child(form)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::BASE))
                        .pt(px(space::BASE))
                        // The pane's single primary action. Nothing else on this
                        // screen wears the accent fill.
                        .child(fb_button(
                            "welcome-create-project",
                            i18n.tr("wizard.button.create"),
                            FbButtonKind::Primary,
                            name_valid,
                            move |_event, window, cx| {
                                let _ = create_target.update(cx, |this, cx| {
                                    this.create_project_from_welcome(window, cx)
                                });
                            },
                        ))
                        .child(fb_button(
                            "welcome-new-continue",
                            i18n.tr("welcome.button.continue-without-project"),
                            FbButtonKind::Default,
                            true,
                            move |_event, window, cx| {
                                let _ = continue_target
                                    .update(cx, |this, cx| this.activate_continue(window, cx));
                            },
                        )),
                ),
        )
        .into_any_element()
}

// ── Open Project pane ────────────────────────────────────────────────────────

/// Browse for a project file, and report exactly why one was rejected.
///
/// Recent projects are deliberately absent: they have one owner, the Start
/// pane's second column, and Escape returns there.
fn open_project_pane(
    cx: &mut Context<WelcomeWindow>,
    open_error: Option<SharedString>,
    i18n: I18n,
) -> gpui::AnyElement {
    let browse_target = cx.entity().clone();

    pane_scroller("welcome-open-pane")
        .child(
            pane_body()
                .child(pane_header(
                    i18n.tr("welcome.open-project"),
                    i18n.tr_or(
                        "welcome.open.subtitle",
                        "Choose a Futureboard project file to open.",
                    ),
                ))
                .child(fb_button(
                    "welcome-open-browse",
                    i18n.tr("wizard.button.browse"),
                    FbButtonKind::Primary,
                    true,
                    move |_event, window, cx| {
                        let _ = browse_target
                            .update(cx, |this, cx| this.browse_and_open_project(window, cx));
                    },
                ))
                .when_some(open_error, |pane, msg| pane.child(open_error_banner(msg))),
        )
        .into_any_element()
}

/// Validation failure from the last browse attempt.
///
/// Two channels, without inventing an icon the asset set does not have: an
/// error-tinted wash *and* an error-tinted border *and* a bold error label.
fn open_error_banner(msg: SharedString) -> impl IntoElement {
    let tone = Colors::status_error();
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .w_full()
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::with_alpha(tone, 0.55))
        .bg(Colors::composite(
            Colors::surface_panel(),
            Colors::with_alpha(tone, 0.12),
        ))
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .font_weight(FontWeight::BOLD)
                .text_color(tone)
                .child(msg),
        )
}

// ── Feed pane ────────────────────────────────────────────────────────────────

fn feed_pane(
    cx: &mut Context<WelcomeWindow>,
    state: &FeedLoadState,
    posts: &[FeedPost],
    i18n: I18n,
) -> gpui::AnyElement {
    let retry_target = cx.entity().clone();
    let content = match state {
        FeedLoadState::Idle | FeedLoadState::Loading => feed_status_card(
            i18n.tr_or("welcome.feed.loading", "Loading feed…"),
            i18n.tr_or(
                "welcome.feed.loading-detail",
                "Fetching the latest public posts.",
            ),
            None,
        ),
        FeedLoadState::Failed(error) => feed_status_card(
            i18n.tr_or("welcome.feed.error", "Feed unavailable"),
            error.to_string(),
            Some(
                fb_button(
                    "welcome-feed-retry",
                    i18n.tr_or("welcome.feed.retry", "Retry"),
                    FbButtonKind::Default,
                    true,
                    move |_event, _window, cx| {
                        let _ = retry_target.update(cx, |this, cx| this.retry_feed(cx));
                    },
                )
                .into_any_element(),
            ),
        ),
        FeedLoadState::Loaded if posts.is_empty() => feed_status_card(
            i18n.tr_or("welcome.feed.empty", "No posts yet"),
            i18n.tr_or(
                "welcome.feed.empty-detail",
                "Published updates will appear here.",
            ),
            None,
        ),
        FeedLoadState::Loaded => {
            let paint = row_paint(Colors::surface_input());
            let mut well = list_well("welcome-feed-list");
            for (index, post) in posts.iter().take(8).enumerate() {
                if index > 0 {
                    well = well.child(row_divider());
                }
                well = well.child(feed_post_row(index, post, paint, i18n));
            }
            well.into_any_element()
        }
    };

    pane_scroller("welcome-feed-pane")
        .child(
            pane_body()
                .child(pane_header(
                    i18n.tr_or("welcome.feed.title", "Futureboard Feed"),
                    i18n.tr_or("welcome.feed.subtitle", "Latest public Studio updates."),
                ))
                .child(content),
        )
        .into_any_element()
}

fn feed_status_card(
    title: impl Into<String>,
    detail: impl Into<String>,
    action: Option<gpui::AnyElement>,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(space::SNUG))
        .w_full()
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(
            div()
                .text_size(px(typography::UI_SM))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(detail.into()),
        )
        .children(action)
        .into_any_element()
}

/// One feed post. Square, full-bleed, and keyboard-reachable — the destination
/// is carried by a trailing chevron rather than by printing a raw URL, which is
/// a browser affordance rather than a Studio one.
fn feed_post_row(index: usize, post: &FeedPost, paint: RowPaint, i18n: I18n) -> impl IntoElement {
    let title = post.title.clone();
    let published_at = post.published_at.clone();
    let excerpt = if post.excerpt.is_empty() {
        SharedString::from(i18n.tr_or(
            "welcome.feed.excerpt-fallback",
            "Read the latest Futureboard Studio update.",
        ))
    } else {
        post.excerpt.clone()
    };
    let public_url = post
        .slug
        .as_ref()
        .map(|slug| format!("{FEED_PUBLIC_BASE_URL}/p/{slug}"));
    let has_link = public_url.is_some();
    let rest = paint.rest;
    let hover = paint.hover;
    let pressed = paint.pressed;
    let focus = Colors::state_focus_ring();

    div()
        .id(("welcome-feed-post", index))
        .role(Role::Button)
        .aria_label(title.clone())
        .aria_disabled(!has_link)
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::LOOSE))
        .w_full()
        .h(px(LIST_ROW_HEIGHT))
        .flex_none()
        .px(px(space::LOOSE))
        .rounded(px(radius::NONE))
        .bg(rest)
        .when_some(public_url, |row, url| {
            row.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url);
                })
        })
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::BASE))
                        .child(
                            div()
                                .truncate()
                                .min_w_0()
                                .flex_1()
                                .text_size(px(typography::UI_SM))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(title),
                        )
                        .when(!published_at.is_empty(), |line| {
                            line.child(
                                div()
                                    .flex_none()
                                    .text_size(px(typography::DENSE_CAPTION))
                                    .font_features(tabular_features())
                                    .text_color(Colors::text_muted())
                                    .child(published_at),
                            )
                        }),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(excerpt),
                ),
        )
        .when(has_link, |row| {
            row.child(
                svg()
                    .path(assets::ICON_CHEVRON_RIGHT_PATH)
                    .w(px(RAIL_ICON))
                    .h(px(RAIL_ICON))
                    .flex_none()
                    .text_color(Colors::text_faint()),
            )
        })
}

// ── Shared pane and list scaffolding ─────────────────────────────────────────

/// A centre pane's outer element: the pane owns its own scroll, so a short
/// window never clips a footer action out of reach.
///
/// The `select` menus inside are painted `deferred`, so they escape this clip
/// rather than being cut off by it.
fn pane_scroller(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .overflow_y_scroll()
        .p(px(space::BLOCK))
        .bg(Colors::surface_base())
}

/// Reading-width column inside a pane scroller.
fn pane_body() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SECTION))
        .w_full()
        .max_w(px(PANE_MAX_WIDTH))
}

fn pane_header(title: impl Into<String>, subtitle: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::TIGHT))
        .child(
            div()
                .text_size(px(typography::UI_TITLE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(subtitle.into()),
        )
}

/// A quiet note under a form row — validation, or a fact about the location.
/// Indented to the control column so it reads as belonging to the field above.
fn field_note(text: impl Into<String>, tone: Rgba) -> impl IntoElement {
    div()
        .pl(px(FORM_LABEL_WIDTH + space::BASE))
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(tone)
        .child(text.into())
}

/// The inset backplate a list sits in.
///
/// This is the *only* thing in a list that rounds: it recesses to the input
/// plane, and the square rows inside it lift off that plane with their state
/// layers. Its `space::TIGHT` padding is also what keeps a focused row's
/// `elevation::focus_ring` — a spread shadow drawn outside the row — inside the
/// scroll container's content mask instead of clipped away at its edge.
fn list_well(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .w_full()
        .p(px(space::TIGHT))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_input())
}

/// Hairline between two rows. Rows separate by value and a rule, not by a gap.
fn row_divider() -> impl IntoElement {
    div().w_full().h(px(1.0)).flex_none().bg(Colors::divider())
}

/// Leading-edge selection marker. An overlay, never a grown border: growing a
/// border would reflow the row's content every time the selection moved.
fn selection_marker() -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top_0()
        .bottom_0()
        .w(px(SELECTION_MARKER_WIDTH))
        .bg(Colors::accent_primary())
}

fn empty_state(text: impl Into<String>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(LIST_ROW_HEIGHT * 2.0))
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child(text.into())
}

// ── Row state layers ─────────────────────────────────────────────────────────

/// The fills one list row can paint, resolved once when the row is built.
///
/// A GPUI div has exactly one background, so `.hover(|s| s.bg(state_hover()))`
/// would *replace* the rest fill with a translucent wash rather than lift the
/// row. Every value here is therefore composited up front with
/// [`Colors::composite`], which DESIGN.md marks as a control-path helper that
/// must never run inside a paint loop.
#[derive(Clone, Copy)]
struct RowPaint {
    rest: Rgba,
    hover: Rgba,
    pressed: Rgba,
    selected: Rgba,
    selected_hover: Rgba,
    selected_pressed: Rgba,
}

fn row_paint(plane: Rgba) -> RowPaint {
    let selected = Colors::composite(plane, Colors::state_selected());
    RowPaint {
        // A row at rest paints nothing, so the well's recessed plane and the
        // hairlines between rows stay visible.
        rest: Colors::with_alpha(plane, 0.0),
        hover: Colors::composite(plane, Colors::state_hover()),
        pressed: Colors::composite(plane, Colors::state_recessed()),
        selected,
        selected_hover: Colors::composite(plane, Colors::state_selected_hover()),
        selected_pressed: Colors::composite(selected, Colors::state_recessed()),
    }
}

impl RowPaint {
    fn rest_for(self, selected: bool) -> Rgba {
        if selected {
            self.selected
        } else {
            self.rest
        }
    }

    fn hover_for(self, selected: bool) -> Rgba {
        if selected {
            self.selected_hover
        } else {
            self.hover
        }
    }

    fn pressed_for(self, selected: bool) -> Rgba {
        if selected {
            self.selected_pressed
        } else {
            self.pressed
        }
    }
}

// ── Typography and i18n helpers ──────────────────────────────────────────────

/// Tabular (fixed-advance) digits.
///
/// Dates, sample rates, tempi, paths and version strings must not jitter as
/// their digits change. `theme.rs` is the right long-term home for this — three
/// other surfaces already want it — but it is a `Font` concern with no colour
/// token behind it, so it lives here until the token module gains one.
fn tabular_features() -> FontFeatures {
    // Built once and handed out as an `Arc` bump: a numeric readout appears a
    // dozen times per frame, and the descriptor never changes.
    static FEATURES: std::sync::OnceLock<FontFeatures> = std::sync::OnceLock::new();
    FEATURES
        .get_or_init(|| FontFeatures(Arc::new(vec![("tnum".to_string(), 1)])))
        .clone()
}

/// Substitute a `{ $n }` placeholder over [`I18n::tr_or`].
///
/// `I18n::tr_vars` resolves through `tr`, which returns the *key* when it is
/// missing, so a not-yet-translated key would render as
/// `welcome.recent.opened-hours`. Going through `tr_or` keeps the English
/// fallback readable until the locale files carry the key.
fn tr_count(i18n: I18n, key: &str, fallback: &str, n: u64) -> String {
    i18n.tr_or(key, fallback).replace("{ $n }", &n.to_string())
}

#[cfg(not(feature = "native-dialogs"))]
fn no_native_dialogs_message(i18n: I18n) -> String {
    i18n.tr_or(
        "welcome.dialogs.unavailable",
        "Native file dialogs are unavailable in this build.",
    )
}

fn template_key(template: ProjectTemplate) -> &'static str {
    match template {
        ProjectTemplate::Empty => "empty",
        ProjectTemplate::Recording => "recording",
        ProjectTemplate::BeatMaking => "beat-making",
        ProjectTemplate::Mixing => "mixing",
        ProjectTemplate::Scoring => "scoring",
    }
}

/// `ProjectTemplate::label()` is English-only data (`project/template.rs` is a
/// non-UI module), but the `template.*` namespace is already translated in every
/// locale, so the start screen resolves names through i18n and falls back to the
/// data label rather than duplicating a second English table.
fn template_label(template: ProjectTemplate, i18n: I18n) -> String {
    i18n.tr_or(
        &format!("template.{}.label", template_key(template)),
        template.label(),
    )
}

fn template_description(template: ProjectTemplate, i18n: I18n) -> String {
    i18n.tr_or(
        &format!("template.{}.description", template_key(template)),
        "",
    )
}

fn template_for_action(action: &WelcomeAction) -> Option<ProjectTemplate> {
    match action {
        WelcomeAction::EmptyProject => Some(ProjectTemplate::Empty),
        WelcomeAction::MidiComposer => Some(ProjectTemplate::BeatMaking),
        WelcomeAction::AudioSession => Some(ProjectTemplate::Recording),
        WelcomeAction::MixTemplate => Some(ProjectTemplate::Mixing),
        _ => None,
    }
}

/// Render a coarse "time ago" label from a unix-seconds timestamp. Empty when
/// the timestamp is zero/unknown. Intentionally low-resolution — exact times
/// add no value on the start screen — but a project older than a month now says
/// so rather than showing nothing at all.
fn format_last_opened(last_opened_at: u64, i18n: I18n) -> String {
    if last_opened_at == 0 {
        return String::new();
    }
    let now = crate::project::now_secs();
    let just_now = || i18n.tr_or("welcome.recent.opened-just-now", "Just now");
    if now <= last_opened_at {
        return just_now();
    }
    let secs = now - last_opened_at;
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if mins < 1 {
        just_now()
    } else if hours < 1 {
        tr_count(i18n, "welcome.recent.opened-minutes", "{ $n }m ago", mins)
    } else if days < 1 {
        tr_count(i18n, "welcome.recent.opened-hours", "{ $n }h ago", hours)
    } else if days < 30 {
        tr_count(i18n, "welcome.recent.opened-days", "{ $n }d ago", days)
    } else {
        i18n.tr_or("welcome.recent.opened-long-ago", "Over a month ago")
    }
}

// ── Feed transport ───────────────────────────────────────────────────────────

fn fetch_feed_posts() -> Result<Vec<FeedPost>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(FEED_FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create feed client: {e}"))?;

    let response = client
        .get(FEED_API_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|e| format!("Could not reach the public feed API: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("Feed API returned HTTP {status}."));
    }

    let payload = response
        .json::<FeedResponse>()
        .map_err(|e| format!("Could not read feed payload: {e}"))?;

    Ok(payload
        .docs
        .into_iter()
        .map(feed_post_from_payload)
        .collect())
}

fn feed_post_from_payload(post: PayloadPost) -> FeedPost {
    // Left empty when the payload has no usable summary: the row substitutes a
    // localized fallback, so no English string is baked in off the UI thread.
    let excerpt = post
        .meta
        .as_ref()
        .and_then(|meta| meta.description.as_deref())
        .filter(|description| !description.trim().is_empty())
        .map(trim_feed_excerpt)
        .or_else(|| post.content.as_ref().map(lexical_excerpt))
        .filter(|excerpt| !excerpt.trim().is_empty())
        .unwrap_or_default();

    FeedPost {
        title: SharedString::from(post.title),
        excerpt: SharedString::from(excerpt),
        published_at: SharedString::from(format_feed_date(post.published_at.as_deref())),
        slug: post.slug.map(SharedString::from),
    }
}

fn format_feed_date(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let date = value.split('T').next().unwrap_or(value);
    let mut parts = date.split('-');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(year), Some(month), Some(day)) => format!("{day}/{month}/{year}"),
        _ => String::new(),
    }
}

fn lexical_excerpt(content: &Value) -> String {
    let mut out = String::new();
    collect_lexical_text(content, &mut out);
    trim_feed_excerpt(&out)
}

fn collect_lexical_text(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push_str(text);
            }
            if let Some(children) = map.get("children") {
                collect_lexical_text(children, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_lexical_text(item, out);
            }
        }
        _ => {}
    }
}

fn trim_feed_excerpt(input: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 170;
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }
    let mut trimmed: String = normalized.chars().take(MAX_CHARS).collect();
    trimmed.push('…');
    trimmed
}
