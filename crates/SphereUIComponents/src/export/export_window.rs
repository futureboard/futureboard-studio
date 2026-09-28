//! Native "Export" dialog: what to render and how.
//!
//! The window owns a plain [`EngineProjectSnapshot`] + [`ExportProjectDefaults`]
//! captured when it opens and edits an [`ExportSettings`]. Render hands the
//! finished job to the Render dialog ([`super::render_dialog`]), which runs it,
//! shows its progress and reports every file it wrote — this window only
//! chooses.
//!
//! What it chooses, independently of each other: the stereo mixdown, any set
//! of mixer channels (one file each), or both, from one render; and whether
//! that render runs offline or plays through the live engine in real time.
//!
//! Three rules shape the layout.
//!
//! * **One scroll owner, one clip owner.** The root clips (its corners and
//!   frame are the window shell's, not painted here), the body `export-body` is the
//!   only scroller, and the status strip plus the action footer stay pinned. A
//!   short or narrow window scrolls its form instead of hiding it.
//! * **Readouts come from the request, not from a parallel calculation.**
//!   Everything numeric is read out of [`ExportSettings::estimate`], which is
//!   built from the same `ArrangementExportRequest` the engine receives, so a
//!   number shown here cannot disagree with the file that gets written.
//! * **Nothing on screen is decorative.** Every control writes a value the
//!   encoder or the renderer consumes; options the project cannot satisfy are
//!   disabled and say why, or are absent entirely.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, App, AppContext, Bounds, Context, Entity, EntityInputHandler, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Point, Render, Role,
    StatefulInteractiveElement, Styled, Toggled, UTF16Selection, Window, WindowHandle,
};

use sphere_encoder::AudioFileFormat;
use DirectAudio::plugin_bridge::PluginBridgeSinkMap;
use DirectAudio::types::EngineProjectSnapshot;
use DirectAudio::RenderJob;

use crate::assets;
use crate::components::controls::{
    fb_button, fb_section_header, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::form::select::{select, select_dismiss_backdrop, SelectOption};
use crate::components::text_input::{bind_mouse_selection, text_field_with_callbacks_and_ime};
use crate::components::title_bar::{external_window_titlebar_with_icon, TITLEBAR_HEIGHT};
use crate::components::{TextInputAction, TextInputState};
use crate::i18n::I18n;
use crate::theme::{self, elevation, radius, size, space, typography, Colors};

use super::export_settings::{
    sanitize_file_stem, ExportChannelMode, ExportChannelPreset, ExportEstimate,
    ExportNormalizeChoice, ExportProjectDefaults, ExportRangeChoice, ExportRenderMode,
    ExportSampleRateChoice, ExportSettings, ExportSettingsError, ExportTailChoice,
    FLAC_COMPRESSION_RANGE, PEAK_TARGETS_DB, TAIL_FIXED_SECONDS, TAIL_SILENCE_MAX_SECONDS,
    TAIL_SILENCE_THRESHOLD_DB,
};
use super::render_dialog::{open_render_dialog, RealtimeTransportHooks, RenderLaunch};
use super::ui_kit::*;

// ── Window geometry ──────────────────────────────────────────────────────────

pub const EXPORT_WINDOW_WIDTH: f32 = 880.0;
const EXPORT_WINDOW_HEIGHT: f32 = 660.0;
/// Below this the channel sidebar and the form's label column would fight for
/// the same pixels.
const EXPORT_WINDOW_MIN_WIDTH: f32 = 720.0;
/// The Channel Selection sidebar: wide enough for a channel name and its kind
/// ("VSTi Out") on one line; longer names truncate.
const CHANNEL_SIDEBAR_WIDTH: f32 = 272.0;
/// The body scrolls, so the floor only has to keep the titlebar, one section,
/// the status strip and the footer on screen at once.
const EXPORT_WINDOW_MIN_HEIGHT: f32 = 520.0;
/// Beat fields hold "1234.000" plus a little slack; a full-width numeric input
/// next to a three-character value reads as a mistake.
const BEAT_FIELD_WIDTH: f32 = 104.0;

/// What an Export command opens the window for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportIntent {
    /// The stereo mixdown (File → Export Arrangement / Audio).
    Mixdown,
    /// Every mixer channel, one file each, without the mixdown (File → Export
    /// Stems).
    Stems,
}

/// The last settings a render was started with, for the next time the window
/// opens — in this session, for any project. The range, the destination and
/// the channel picks that do not exist in the next project are not carried.
static LAST_SETTINGS: Mutex<Option<ExportSettings>> = Mutex::new(None);

/// Which dropdown is currently open (only one at a time). Mode and channel count
/// are exclusive three- and two-way choices, so they are segmented controls
/// rather than dropdowns and do not appear here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectField {
    Format,
    FormatOption,
    FlacCompression,
    Range,
    SampleRate,
    Normalize,
    Tail,
}

/// Which text field currently owns the keyboard. Drives both the IME bridge and
/// the key-priority chain, so "typing" always beats "application command".
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportField {
    Name,
    RangeStart,
    RangeEnd,
}

pub struct ExportArrangementWindow {
    project_name: String,
    /// Captured at open so `render` never reads a global mid-frame.
    language: String,
    snapshot: EngineProjectSnapshot,
    bridge_sinks: PluginBridgeSinkMap,
    audio_engine: Option<DirectAudio::AudioEngine>,
    defaults: ExportProjectDefaults,
    settings: ExportSettings,
    /// Editable export stem (mixdown file name / stems folder prefix).
    name_input: TextInputState,
    /// Custom-range bounds, in beats. Beats (not bars) because the export
    /// snapshot carries a single global time signature and no signature map, so
    /// bars.beats could not be converted exactly.
    range_start_input: TextInputState,
    range_end_input: TextInputState,
    /// True while a custom-range field holds text that is not a number. The
    /// model keeps its last valid range; Export stays disabled until the draft
    /// parses, so a half-typed value can never silently redefine the range.
    range_draft_invalid: bool,
    /// Cached validation + geometry for the current settings.
    ///
    /// Recomputed on mutation, never during `render`: building it stats the
    /// output folder and walks the snapshot's tempo map and clip list, and
    /// DESIGN.md keeps render functions free of filesystem and scanning work.
    estimate: Result<ExportEstimate, ExportSettingsError>,
    /// Why the last Render or Browse did not happen, shown in the status strip
    /// until the next edit.
    failure: Option<String>,
    open_select: Option<SelectField>,
    /// How a realtime render borrows the studio's transport. `None` when the
    /// opener has none (no studio), which leaves realtime unavailable.
    realtime_hooks: Option<RealtimeTransportHooks>,
    /// One-shot: the first frame moves keyboard focus into the name field, so
    /// the first Tab has an anchor instead of starting from nowhere.
    focus_primed: bool,
    /// Files the last Render press would replace, shown for confirmation;
    /// the next press with the same files goes ahead. Cleared by any edit.
    confirm_replace: Option<Vec<PathBuf>>,
    focus_handle: FocusHandle,
}

impl ExportArrangementWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_name: String,
        snapshot: EngineProjectSnapshot,
        bridge_sinks: PluginBridgeSinkMap,
        audio_engine: Option<DirectAudio::AudioEngine>,
        mut defaults: ExportProjectDefaults,
        intent: ExportIntent,
        realtime_hooks: Option<RealtimeTransportHooks>,
        cx: &mut Context<Self>,
    ) -> Self {
        if realtime_hooks.is_none() || audio_engine.is_none() {
            defaults.live_sample_rate = 0;
        }
        let mut settings = remembered_settings(&defaults);
        match intent {
            ExportIntent::Mixdown => {}
            ExportIntent::Stems => {
                settings.include_mixdown = false;
                settings.apply_channel_preset(ExportChannelPreset::All, &defaults);
            }
        }
        // Default output: <project>.wav in the temp dir as a safe fallback; the
        // opener can override with a project Exports folder.
        let file = ExportSettings::default_file_name(&project_name, settings.format);
        settings.output_path = Some(std::env::temp_dir().join(file));
        let stem = export_stem_from_input(&project_name);
        let mut name_input = TextInputState::new("export-file-name", cx.focus_handle())
            .with_placeholder("Export name");
        name_input.set_value(&stem);

        let mut range_start_input = TextInputState::new("export-range-start", cx.focus_handle())
            .with_placeholder("0")
            .with_ascii_charset("0123456789.");
        let mut range_end_input = TextInputState::new("export-range-end", cx.focus_handle())
            .with_placeholder("0")
            .with_ascii_charset("0123456789.");
        range_start_input.set_value(format_beats(0.0));
        range_end_input.set_value(format_beats(defaults.content_end_beat.max(1.0)));

        let estimate = settings.estimate(&snapshot, &defaults);
        let language = I18n::from_app(cx).locale().code().to_string();

        Self {
            project_name,
            language,
            snapshot,
            bridge_sinks,
            audio_engine,
            defaults,
            settings,
            name_input,
            range_start_input,
            range_end_input,
            range_draft_invalid: false,
            estimate,
            failure: None,
            open_select: None,
            realtime_hooks,
            focus_primed: false,
            confirm_replace: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Override the default output path (e.g. project Exports folder).
    pub fn set_default_output(&mut self, path: PathBuf) {
        self.settings.output_path = Some(path);
        self.sync_name_from_path();
        self.refresh_estimate();
    }

    fn export_name(&self) -> String {
        export_stem_from_input(&self.name_input.value)
    }

    /// Keep `output_path` in the same folder, with the name field as the stem.
    fn sync_output_from_name(&mut self) {
        let stem = self.export_name();
        let parent = self
            .settings
            .output_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let file = format!("{stem}.{}", self.settings.format.extension());
        self.settings.output_path = Some(parent.join(file));
    }

    fn sync_name_from_path(&mut self) {
        if let Some(stem) = self
            .settings
            .output_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .filter(|s| !s.trim().is_empty())
        {
            self.name_input.set_value(stem);
        }
    }

    /// Recompute the cached validation + geometry. Call after every mutation of
    /// `settings`; never from `render`. An edit also clears the last failure:
    /// it described settings that no longer hold.
    fn refresh_estimate(&mut self) {
        self.estimate = self.settings.estimate(&self.snapshot, &self.defaults);
        self.failure = None;
        self.confirm_replace = None;
    }

    fn can_export(&self) -> bool {
        self.estimate.is_ok() && !self.range_draft_invalid
    }

    /// Parse both custom-range fields. The model only moves when *both* parse,
    /// so a partially typed pair never resolves to a range nobody asked for.
    fn sync_range_from_inputs(&mut self) {
        if !matches!(self.settings.range, ExportRangeChoice::Custom { .. }) {
            return;
        }
        let start = self.range_start_input.value.trim().parse::<f64>();
        let end = self.range_end_input.value.trim().parse::<f64>();
        match (start, end) {
            (Ok(start), Ok(end))
                if start.is_finite() && end.is_finite() && start >= 0.0 && end >= 0.0 =>
            {
                self.range_draft_invalid = false;
                self.settings.range = ExportRangeChoice::Custom {
                    start_beat: start,
                    end_beat: end,
                };
            }
            _ => self.range_draft_invalid = true,
        }
    }

    fn seed_range_inputs(&mut self, start_beat: f64, end_beat: f64) {
        self.range_start_input.set_value(format_beats(start_beat));
        self.range_end_input.set_value(format_beats(end_beat));
        self.range_draft_invalid = false;
    }

    fn focused_field(&self, window: &Window) -> Option<ExportField> {
        if self.name_input.is_focused(window) {
            Some(ExportField::Name)
        } else if self.range_start_input.is_focused(window) {
            Some(ExportField::RangeStart)
        } else if self.range_end_input.is_focused(window) {
            Some(ExportField::RangeEnd)
        } else {
            None
        }
    }

    fn close(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    /// Key priority, per DESIGN.md: dialog → text/numeric input → local surface
    /// → application command.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();

        // 1. An open dropdown owns Escape, so dismissing a menu never also
        //    dismisses the dialog behind it — including while a field is
        //    focused.
        if key == "escape" && self.open_select.is_some() {
            self.open_select = None;
            cx.notify();
            return;
        }

        // 2. Typing beats every application command.
        let focused = self.focused_field(window);
        {
            if let Some(field) = focused {
                let action = match field {
                    ExportField::Name => self.name_input.handle_key_ime(event, Some(cx)),
                    ExportField::RangeStart => {
                        self.range_start_input.handle_key_ime(event, Some(cx))
                    }
                    ExportField::RangeEnd => self.range_end_input.handle_key_ime(event, Some(cx)),
                };
                match field {
                    ExportField::Name => self.sync_output_from_name(),
                    ExportField::RangeStart | ExportField::RangeEnd => {
                        self.sync_range_from_inputs()
                    }
                }
                self.refresh_estimate();
                match action {
                    // Enter in the name field is the dialog's accept gesture.
                    // In a numeric field it only commits the value.
                    TextInputAction::Submit => {
                        if field == ExportField::Name && self.can_export() {
                            self.start_render(window, cx);
                            return;
                        }
                    }
                    TextInputAction::Cancel => {
                        self.close(window, cx);
                        return;
                    }
                    TextInputAction::Consumed | TextInputAction::Pass => {}
                }
                cx.notify();
                return;
            }
        }

        match key {
            "escape" => self.close(window, cx),
            "enter" | "numpad_enter" if self.can_export() => self.start_render(window, cx),
            _ => {}
        }
    }

    fn browse_output(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let entity = cx.entity().clone();
            let format = self.settings.format;
            let pick_file = self.settings.include_mixdown;
            let export_name = self.export_name();
            let start = self
                .settings
                .output_path
                .clone()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(std::env::temp_dir);
            let file = format!("{export_name}.{}", format.extension());
            cx.spawn(async move |_this, cx| {
                let dialog = rfd::AsyncFileDialog::new()
                    .set_title(if pick_file {
                        "Export Mixdown"
                    } else {
                        "Choose Export Folder"
                    })
                    .set_directory(&start);
                let result = if pick_file {
                    dialog
                        .set_file_name(&file)
                        .add_filter(format.as_str().to_uppercase(), &[format.extension()])
                        .save_file()
                        .await
                } else {
                    dialog.pick_folder().await
                };
                if let Some(handle) = result {
                    let path = if pick_file {
                        handle.path().to_path_buf()
                    } else {
                        handle
                            .path()
                            .join(format!("{export_name}.{}", format.extension()))
                    };
                    let _ = entity.update(cx, |this, cx| {
                        this.settings.output_path = Some(path);
                        this.sync_name_from_path();
                        this.refresh_estimate();
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            self.failure = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    // ── Render ───────────────────────────────────────────────────────────────

    /// Build the job and either hand it to the Render dialog or ask before
    /// replacing existing files. The renderer overwrites its destinations on
    /// success, so this is a destructive action and DESIGN.md requires an
    /// explicit Cancel.
    fn start_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A confirmation holds only for the settings it was asked about: the
        // refresh below clears it, so take it first.
        let confirmed = self.confirm_replace.take();
        self.open_select = None;
        self.sync_output_from_name();
        self.refresh_estimate();
        let Some(job) = self.resolve_job(cx) else {
            return;
        };

        // Replacing files is confirmed in this window (the status strip names
        // them, the button turns into "Replace & Render") rather than by a
        // prompt window: a prompt closes as the Render dialog opens, and on
        // Windows a dialog created while the prompt is still being destroyed
        // is owned by it and goes with it.
        let existing = existing_destinations(&job);
        if !existing.is_empty() && confirmed.as_deref() != Some(existing.as_slice()) {
            self.confirm_replace = Some(existing);
            cx.notify();
            return;
        }
        self.launch_render(job, window, cx);
    }

    /// The job for the current settings, or `None` with the reason shown.
    fn resolve_job(&mut self, cx: &mut Context<Self>) -> Option<RenderJob> {
        match self
            .settings
            .to_job(&self.snapshot, &self.defaults, &self.export_name())
        {
            Ok(job) => Some(job),
            Err(err) => {
                self.failure = Some(localized_error(&self.language, &err));
                cx.notify();
                None
            }
        }
    }

    /// Open the Render dialog on `job` over this window. The settings are
    /// remembered for the next time the window opens.
    fn launch_render(&mut self, job: RenderJob, window: &mut Window, cx: &mut Context<Self>) {
        let realtime = self.settings.render_mode == ExportRenderMode::Realtime;
        let launch = RenderLaunch {
            job,
            snapshot: self.snapshot.clone(),
            bridge_sinks: self.bridge_sinks.clone(),
            audio_engine: self.audio_engine.clone(),
            realtime: if realtime {
                self.realtime_hooks.clone()
            } else {
                None
            },
        };
        if realtime && launch.realtime.is_none() {
            self.failure = Some(localized_error(
                &self.language,
                &ExportSettingsError::RealtimeUnavailable,
            ));
            cx.notify();
            return;
        }
        if let Ok(mut last) = LAST_SETTINGS.lock() {
            *last = Some(self.settings.clone());
        }
        // The Render dialog opens over this window, which stays open behind
        // it: closing the report brings back the same settings to adjust and
        // render again. (It cannot close first and hand over: on Windows a
        // dialog is owned by the window active when it is created, window
        // destruction is asynchronous, and a dialog whose owner goes away goes
        // with it.)
        if let Err(error) = open_render_dialog(Some(window.bounds()), &self.language, launch, cx) {
            self.failure = Some(error);
            cx.notify();
        }
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

impl Render for ExportArrangementWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One-shot focus anchor. Without it the first Tab has nothing to move
        // from, so no focus ring is ever visible.
        if !self.focus_primed {
            self.focus_primed = true;
            self.name_input.focus_handle.focus(window, cx);
        }

        let i18n = I18n::new(&self.language);
        let target = cx.entity().clone();
        let dismiss_backdrop = self.open_select.is_some().then(|| {
            let target = target.clone();
            select_dismiss_backdrop(Arc::new(move |_, _window, cx| {
                let _ = target.update(cx, |this, cx| {
                    if this.open_select.take().is_some() {
                        cx.notify();
                    }
                });
            }))
        });

        let body = self.render_editing(window, i18n, &target);
        let footer = self.footer_editing(i18n, &target);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_base())
            // The clip owner. No radius, frame or shadow: the window shell
            // draws those.
            .overflow_hidden()
            .capture_key_down({
                let target = target.clone();
                move |event, window, cx| {
                    let _ = target.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar_with_icon(
                Some(assets::ICON_SHARE_PATH),
                i18n.tr_or("export.title", "Export"),
                "export-window-close",
                {
                    let target = target.clone();
                    move |window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(body)
            .children(self.status_strip(i18n))
            .child(footer)
            .children(dismiss_backdrop)
    }
}

impl ExportArrangementWindow {
    // ── Editing body ─────────────────────────────────────────────────────────

    fn render_editing(
        &self,
        window: &Window,
        i18n: I18n,
        target: &Entity<Self>,
    ) -> gpui::AnyElement {
        // Two regions, each its own scroll owner: what to render on the left
        // (the mixdown and the channels, as a list that can be long), how to
        // render it on the right.
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(self.channel_sidebar(i18n, target))
            .child(
                body_scroll()
                    .min_w_0()
                    .child(self.section_destination(window, i18n, target))
                    .child(self.section_range(window, i18n, target))
                    .child(self.section_render_mode(i18n, target))
                    .child(self.section_format(i18n, target))
                    .child(self.section_summary(i18n)),
            )
            .into_any_element()
    }

    /// Where the files land: the stem the user types, the folder, and a literal
    /// statement of what will be written there.
    fn section_destination(
        &self,
        window: &Window,
        i18n: I18n,
        target: &Entity<Self>,
    ) -> impl IntoElement {
        let batch = !self.settings.include_mixdown;
        let focused = self.name_input.is_focused(window);
        let name_field = text_field_with_callbacks_and_ime(
            &self.name_input,
            focused,
            bind_mouse_selection(target.clone(), |this| &mut this.name_input),
            target.clone(),
        );

        let folder = self
            .settings
            .normalized_output_path()
            .and_then(|path| path.parent().map(|dir| dir.display().to_string()))
            .unwrap_or_else(|| i18n.tr_or("export.readout.no-folder", "No folder selected"));

        let browse = fb_button(
            "export-browse",
            i18n.tr_or("export.action.browse", "Browse…"),
            FbButtonKind::Default,
            true,
            {
                let target = target.clone();
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| this.browse_output(cx));
                }
            },
        );

        form_section(
            i18n.tr_or("export.section.destination", "Destination"),
            vec![
                form_row(
                    if batch {
                        i18n.tr_or("export.field.base-name", "Base name")
                    } else {
                        i18n.tr_or("export.field.name", "File name")
                    },
                    name_field,
                ),
                form_row(
                    i18n.tr_or("export.field.folder", "Folder"),
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .child(readout_surface(folder))
                        .child(browse),
                ),
                readout_row(
                    i18n.tr_or("export.readout.writes-to", "Writes"),
                    self.destination_summary(i18n),
                ),
            ],
        )
    }

    /// Channel Selection: what gets rendered — the mixdown, any set of mixer
    /// channels, or both, from one render. Laid out like a DAW's export
    /// channel list: the master on top, then every channel in mixer order
    /// (the order the files are numbered in), quick picks pinned below.
    fn channel_sidebar(&self, i18n: I18n, target: &Entity<Self>) -> impl IntoElement {
        let selected = self.settings.batch_target_count(&self.defaults);
        let total = self.defaults.track_targets.len();

        let header = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .px(px(space::BASE))
            .pt(px(space::LOOSE))
            .pb(px(space::SNUG))
            .child(div().flex_1().min_w_0().child(fb_section_header(
                i18n.tr_or("export.section.channels", "Channel Selection"),
            )))
            .child(
                div()
                    .flex_shrink_0()
                    .font_features(tabular_figures())
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_muted())
                    .child(tr_vars_or(
                        i18n,
                        "export.output.files",
                        "{ $count } files",
                        &[(
                            "count",
                            (selected + usize::from(self.settings.include_mixdown)).to_string(),
                        )],
                    )),
            );

        let mixdown = {
            let target = target.clone();
            channel_row(
                "export-include-mixdown",
                i18n.tr_or("export.output.mixdown", "Mixdown (Stereo Out)"),
                i18n.tr_or("export.output.master", "Master"),
                self.settings.include_mixdown,
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.settings.include_mixdown = !this.settings.include_mixdown;
                        this.refresh_estimate();
                        cx.notify();
                    });
                },
            )
        };

        let mut list = div()
            .id("export-channels")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .gap(px(space::HAIR))
            .px(px(space::SNUG))
            .pb(px(space::BASE))
            .child(mixdown)
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(1.0))
                    .mx(px(space::TIGHT))
                    .my(px(space::HAIR))
                    .bg(Colors::border_subtle()),
            );
        if total == 0 {
            list = list.child(div().px(px(space::TIGHT)).child(note_row(i18n.tr_or(
                "export.output.no-channels",
                "This project has no mixer channels to render on their own.",
            ))));
        }
        for (index, track) in self.defaults.track_targets.iter().enumerate() {
            let id = track.id.clone();
            let target = target.clone();
            list = list.child(channel_row(
                ("export-channel", index),
                track.name.clone(),
                track.kind_label.clone(),
                self.settings.is_track_selected(&track.id),
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.settings.toggle_track(&id);
                        this.refresh_estimate();
                        cx.notify();
                    });
                },
            ));
        }
        if selected > 0 {
            // Channel files come out of the same pass as the mixdown; say how
            // they are taken so nobody expects them to sum differently.
            list = list.child(
                div()
                    .px(px(space::TIGHT))
                    .pt(px(space::SNUG))
                    .child(note_row(i18n.tr_or(
                        "export.note.channels",
                        "Each channel is taken after its inserts and fader, as it reaches \
                         the mix. Channel files are never normalized.",
                    ))),
            );
        }

        let preset_button = |id: &'static str, label: String, preset: ExportChannelPreset| {
            let target = target.clone();
            div().flex_1().min_w_0().child(fb_button(
                id,
                label,
                FbButtonKind::Ghost,
                total > 0,
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.settings.apply_channel_preset(preset, &this.defaults);
                        this.refresh_estimate();
                        cx.notify();
                    });
                },
            ))
        };
        let presets = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::HAIR))
            .px(px(space::SNUG))
            .py(px(space::SNUG))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .child(preset_button(
                "export-channels-all",
                i18n.tr_or("export.preset.all", "All"),
                ExportChannelPreset::All,
            ))
            .child(preset_button(
                "export-channels-source",
                i18n.tr_or("export.preset.source", "Source tracks"),
                ExportChannelPreset::SourceTracks,
            ))
            .child(preset_button(
                "export-channels-none",
                i18n.tr_or("export.preset.none", "None"),
                ExportChannelPreset::None,
            ));

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(CHANNEL_SIDEBAR_WIDTH))
            .min_h_0()
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .child(header)
            .child(list)
            .child(presets)
    }

    /// Offline or realtime.
    fn section_render_mode(&self, i18n: I18n, target: &Entity<Self>) -> impl IntoElement {
        let mode = self.settings.render_mode;
        let realtime_available = self.defaults.live_sample_rate > 0;
        let mut track = fb_segmented_track().w_full();
        for (value, id, label, position, enabled) in [
            (
                ExportRenderMode::Offline,
                "export-render-offline",
                i18n.tr_or("export.render.offline", "Offline"),
                FbSegment::First,
                true,
            ),
            (
                ExportRenderMode::Realtime,
                "export-render-realtime",
                i18n.tr_or("export.render.realtime", "Realtime"),
                FbSegment::Last,
                realtime_available,
            ),
        ] {
            let target = target.clone();
            track = track.child(fb_segment(
                id,
                label,
                mode == value,
                position,
                move |_, _window, cx| {
                    if !enabled {
                        return;
                    }
                    let _ = target.update(cx, |this, cx| {
                        this.settings.render_mode = value;
                        // One pass cannot normalize: follow the mode rather
                        // than fail on Render.
                        if value == ExportRenderMode::Realtime {
                            this.settings.normalize = ExportNormalizeChoice::Off;
                        }
                        this.refresh_estimate();
                        cx.notify();
                    });
                },
            ));
        }
        let note = match (mode, realtime_available) {
            (_, false) => i18n.tr_or(
                "export.render.realtime-unavailable",
                "Realtime needs a running audio device.",
            ),
            (ExportRenderMode::Offline, true) => i18n.tr_or(
                "export.render.offline-note",
                "Renders as fast as the machine allows. Any sample rate.",
            ),
            (ExportRenderMode::Realtime, true) => tr_vars_or(
                i18n,
                "export.render.realtime-note",
                "Plays the project through the live engine and records it — for hardware \
                 inserts, external instruments and plug-ins that only sound right live. \
                 Takes as long as the range, at the device rate ({ $hz } Hz). The \
                 transport is in use until it finishes.",
                &[("hz", self.defaults.live_sample_rate.to_string())],
            ),
        };
        form_section(
            i18n.tr_or("export.section.render", "Render"),
            vec![
                form_row(i18n.tr_or("export.field.render-mode", "Mode"), track),
                note_row(note),
            ],
        )
    }

    /// Which span of the arrangement, and what happens after it ends.
    fn section_range(
        &self,
        window: &Window,
        i18n: I18n,
        target: &Entity<Self>,
    ) -> impl IntoElement {
        let selected = match self.settings.range {
            ExportRangeChoice::EntireArrangement => "entire",
            ExportRangeChoice::TimeSelection { .. } => "selection",
            ExportRangeChoice::LoopRange { .. } => "loop",
            ExportRangeChoice::Custom { .. } => "custom",
        };

        let mut options = vec![SelectOption::new(
            "entire",
            i18n.tr_or("export.range.entire", "Entire arrangement"),
        )];
        // The opener does not carry a time selection today, so the option is
        // simply absent rather than permanently greyed out: a control that can
        // never become available is not a control.
        if self.defaults.time_selection.is_some() {
            options.push(SelectOption::new(
                "selection",
                i18n.tr_or("export.range.selection", "Time selection"),
            ));
        }
        options.push(match self.defaults.loop_range {
            Some((start, end)) => {
                SelectOption::new("loop", i18n.tr_or("export.range.loop", "Loop range"))
                    .description(tr_vars_or(
                        i18n,
                        "export.range.loop-span",
                        "beats { $start } – { $end }",
                        &[("start", format_beats(start)), ("end", format_beats(end))],
                    ))
            }
            None => SelectOption::new(
                "loop",
                i18n.tr_or("export.range.loop-unset", "Loop range (none set)"),
            )
            .disabled(true),
        });
        options.push(SelectOption::new(
            "custom",
            i18n.tr_or("export.range.custom", "Custom range"),
        ));

        let mut rows = vec![form_row(
            i18n.tr_or("export.field.range", "Range"),
            self.dropdown(
                SelectField::Range,
                "export-range",
                selected.to_string(),
                options,
                target,
            ),
        )];

        if matches!(self.settings.range, ExportRangeChoice::Custom { .. }) {
            rows.push(form_row(
                i18n.tr_or("export.field.start-beat", "Start (beats)"),
                beat_field(&self.range_start_input, window, target, |this| {
                    &mut this.range_start_input
                }),
            ));
            rows.push(form_row(
                i18n.tr_or("export.field.end-beat", "End (beats)"),
                beat_field(&self.range_end_input, window, target, |this| {
                    &mut this.range_end_input
                }),
            ));
        }

        // The derived seconds readout: beats are the input, but the file is
        // measured in time, and the tempo map is what connects them.
        rows.push(readout_row(
            i18n.tr_or("export.readout.span", "Span"),
            match &self.estimate {
                Ok(estimate) => format!(
                    "{} → {}",
                    format_duration(estimate.start_seconds),
                    format_duration(estimate.end_seconds)
                ),
                Err(_) => "—".to_string(),
            },
        ));

        let tail_selected = match self.settings.tail {
            ExportTailChoice::None => "none",
            ExportTailChoice::FixedSeconds(_) => "fixed",
            ExportTailChoice::UntilSilence { .. } => "silence",
        };
        let tail_options = vec![
            SelectOption::new("none", i18n.tr_or("export.tail.none", "None")),
            SelectOption::new(
                "fixed",
                tr_vars_or(
                    i18n,
                    "export.tail.fixed",
                    "Fixed { $seconds } s",
                    &[("seconds", format!("{TAIL_FIXED_SECONDS:.1}"))],
                ),
            ),
            SelectOption::new(
                "silence",
                tr_vars_or(
                    i18n,
                    "export.tail.silence",
                    "Until silence (max { $max } s)",
                    &[("max", format!("{TAIL_SILENCE_MAX_SECONDS:.1}"))],
                ),
            )
            .description(tr_vars_or(
                i18n,
                "export.tail.silence-detail",
                "Stops once the block peak falls below { $db } dBFS.",
                &[("db", format!("{TAIL_SILENCE_THRESHOLD_DB:.0}"))],
            )),
        ];
        rows.push(form_row(
            i18n.tr_or("export.field.tail", "Tail"),
            self.dropdown(
                SelectField::Tail,
                "export-tail",
                tail_selected.to_string(),
                tail_options,
                target,
            ),
        ));

        form_section(i18n.tr_or("export.section.range", "Range"), rows)
    }

    /// Container, resolution and gain staging — everything the encoder reads.
    fn section_format(&self, i18n: I18n, target: &Entity<Self>) -> impl IntoElement {
        let mut rows = Vec::new();

        let format_options = vec![
            SelectOption::new("wav", "WAV"),
            SelectOption::new("flac", "FLAC"),
            {
                let option = SelectOption::new("mp3", "MP3");
                if self.defaults.mp3_available {
                    option
                } else {
                    option.disabled(true).description(i18n.tr_or(
                        "export.hint.mp3-unavailable",
                        "Not compiled into this build.",
                    ))
                }
            },
        ];
        rows.push(form_row(
            i18n.tr_or("export.field.format", "Format"),
            self.dropdown(
                SelectField::Format,
                "export-format",
                self.settings.format.as_str().to_string(),
                format_options,
                target,
            ),
        ));

        // Rauf is never selectable, so it contributes no row rather than an
        // empty dropdown.
        if let Some((label, selected, options)) = self.format_option_field(i18n) {
            rows.push(form_row(
                label,
                self.dropdown(
                    SelectField::FormatOption,
                    "export-format-option",
                    selected,
                    options,
                    target,
                ),
            ));
        }

        if self.settings.format == AudioFileFormat::Flac {
            let level = self.settings.flac_compression_level.unwrap_or(5);
            let options = (FLAC_COMPRESSION_RANGE.0..=FLAC_COMPRESSION_RANGE.1)
                .map(|value| {
                    let label = match value {
                        v if v == FLAC_COMPRESSION_RANGE.0 => tr_vars_or(
                            i18n,
                            "export.flac.fastest",
                            "{ $level } — fastest",
                            &[("level", value.to_string())],
                        ),
                        v if v == FLAC_COMPRESSION_RANGE.1 => tr_vars_or(
                            i18n,
                            "export.flac.smallest",
                            "{ $level } — smallest",
                            &[("level", value.to_string())],
                        ),
                        _ => value.to_string(),
                    };
                    SelectOption::new(value.to_string(), label)
                })
                .collect::<Vec<_>>();
            rows.push(form_row(
                i18n.tr_or("export.field.flac-compression", "Compression"),
                self.dropdown(
                    SelectField::FlacCompression,
                    "export-flac-compression",
                    level.to_string(),
                    options,
                    target,
                ),
            ));
        }

        let rate_selected = match self.settings.sample_rate {
            ExportSampleRateChoice::Project => "project",
            ExportSampleRateChoice::Hz44100 => "44100",
            ExportSampleRateChoice::Hz48000 => "48000",
            ExportSampleRateChoice::Hz88200 => "88200",
            ExportSampleRateChoice::Hz96000 => "96000",
        };
        let rate_options = vec![
            SelectOption::new(
                "project",
                tr_vars_or(
                    i18n,
                    "export.rate.project",
                    "Project ({ $hz } Hz)",
                    &[("hz", self.defaults.project_sample_rate.to_string())],
                ),
            ),
            SelectOption::new("44100", "44100 Hz"),
            SelectOption::new("48000", "48000 Hz"),
            SelectOption::new("88200", "88200 Hz"),
            SelectOption::new("96000", "96000 Hz"),
        ];
        if self.settings.render_mode == ExportRenderMode::Realtime {
            rows.push(readout_row(
                i18n.tr_or("export.field.sample-rate", "Sample rate"),
                tr_vars_or(
                    i18n,
                    "export.rate.device",
                    "Device ({ $hz } Hz)",
                    &[("hz", self.defaults.live_sample_rate.to_string())],
                ),
            ));
        } else {
            rows.push(form_row(
                i18n.tr_or("export.field.sample-rate", "Sample rate"),
                self.dropdown(
                    SelectField::SampleRate,
                    "export-rate",
                    rate_selected.to_string(),
                    rate_options,
                    target,
                ),
            ));
        }

        let channels = self.settings.channels;
        let mut channel_track = fb_segmented_track().w_full();
        for (value, id, label, position) in [
            (
                ExportChannelMode::Stereo,
                "export-channels-stereo",
                i18n.tr_or("export.channels.stereo", "Stereo"),
                FbSegment::First,
            ),
            (
                ExportChannelMode::Mono,
                "export-channels-mono",
                i18n.tr_or("export.channels.mono", "Mono"),
                FbSegment::Last,
            ),
        ] {
            let target = target.clone();
            channel_track = channel_track.child(fb_segment(
                id,
                label,
                channels == value,
                position,
                move |_, _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.settings.channels = value;
                        this.refresh_estimate();
                        cx.notify();
                    });
                },
            ));
        }
        rows.push(form_row(
            i18n.tr_or("export.field.channels", "Channels"),
            channel_track,
        ));

        // Normalization is a real two-pass gain stage on the mixdown only, and
        // a realtime render has one pass — so the option carries the reason,
        // not just a grey.
        let normalize_block = if !self.settings.include_mixdown {
            Some(i18n.tr_or(
                "export.hint.normalize-mixdown-only",
                "Applies to the mixdown only.",
            ))
        } else if self.settings.render_mode == ExportRenderMode::Realtime {
            Some(i18n.tr_or(
                "export.hint.normalize-offline-only",
                "Needs an offline render.",
            ))
        } else {
            None
        };
        let normalize_selected = match self.settings.normalize {
            ExportNormalizeChoice::Off => "off".to_string(),
            ExportNormalizeChoice::PeakDb(db) => peak_option_id(db),
        };
        let mut normalize_options = vec![SelectOption::new(
            "off",
            i18n.tr_or("export.normalize.off", "Off"),
        )];
        for db in PEAK_TARGETS_DB {
            let option = SelectOption::new(
                peak_option_id(db),
                tr_vars_or(
                    i18n,
                    "export.normalize.peak",
                    "Peak { $db } dBFS",
                    &[("db", format_db(db))],
                ),
            );
            normalize_options.push(match &normalize_block {
                Some(reason) => option.disabled(true).description(reason.clone()),
                None => option,
            });
        }
        rows.push(form_row(
            i18n.tr_or("export.field.normalize", "Normalize"),
            self.dropdown(
                SelectField::Normalize,
                "export-normalize",
                normalize_selected,
                normalize_options,
                target,
            ),
        ));

        form_section(i18n.tr_or("export.section.format", "Audio format"), rows)
    }

    /// The bit-depth / bitrate row, which changes identity with the container.
    fn format_option_field(&self, i18n: I18n) -> Option<(String, String, Vec<SelectOption>)> {
        match self.settings.format {
            AudioFileFormat::Wav => Some((
                i18n.tr_or("export.field.bit-depth", "Bit depth"),
                match self.settings.wav_sample_format {
                    sphere_encoder::AudioSampleFormat::F32 => "f32",
                    sphere_encoder::AudioSampleFormat::I24 => "i24",
                    _ => "i16",
                }
                .to_string(),
                vec![
                    SelectOption::new("f32", i18n.tr_or("export.depth.f32", "Float 32")),
                    SelectOption::new("i24", i18n.tr_or("export.depth.i24", "PCM 24")),
                    SelectOption::new("i16", i18n.tr_or("export.depth.i16", "PCM 16")),
                ],
            )),
            AudioFileFormat::Flac => Some((
                i18n.tr_or("export.field.bit-depth", "Bit depth"),
                self.settings.flac_bit_depth.to_string(),
                vec![
                    SelectOption::new("16", "16-bit"),
                    SelectOption::new("24", "24-bit"),
                ],
            )),
            AudioFileFormat::Mp3 => Some((
                i18n.tr_or("export.field.bitrate", "Bitrate"),
                self.settings.mp3_bitrate_kbps.to_string(),
                vec![
                    SelectOption::new("128", "128 kbps"),
                    SelectOption::new("192", "192 kbps"),
                    SelectOption::new("256", "256 kbps"),
                    SelectOption::new("320", "320 kbps"),
                ],
            )),
            AudioFileFormat::Rauf => None,
        }
    }

    /// What the export will actually produce, read out of the engine request.
    fn section_summary(&self, i18n: I18n) -> impl IntoElement {
        let rows = match &self.estimate {
            Ok(estimate) => {
                let mut rows = vec![
                    readout_row(
                        i18n.tr_or("export.readout.duration", "Duration"),
                        format_duration(estimate.content_seconds),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.tail", "Tail"),
                        self.tail_summary(i18n, estimate),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.output", "Output"),
                        output_spec(estimate, i18n),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.frames", "Frames"),
                        grouped(estimate.content_frames),
                    ),
                    readout_row(
                        i18n.tr_or("export.readout.files", "Files"),
                        estimate.file_count.to_string(),
                    ),
                ];
                if let Some(bytes) = estimate.uncompressed_bytes {
                    rows.push(readout_row(
                        i18n.tr_or("export.readout.size", "File size"),
                        format!("≈ {}", format_bytes(bytes)),
                    ));
                }
                rows
            }
            // Never a fabricated number: when the request cannot be built there
            // is nothing truthful to show.
            Err(err) => vec![note_row(localized_error(&self.language, err))],
        };
        form_section(i18n.tr_or("export.section.summary", "Summary"), rows)
    }

    fn tail_summary(&self, i18n: I18n, estimate: &ExportEstimate) -> String {
        match self.settings.tail {
            ExportTailChoice::None => i18n.tr_or("export.tail.none", "None"),
            ExportTailChoice::FixedSeconds(seconds) => format!("{seconds:.1} s"),
            // `max_tail_frames` reports the cap; the renderer stops early once
            // the block peak drops, so this is a ceiling, not a duration.
            ExportTailChoice::UntilSilence { .. } => tr_vars_or(
                i18n,
                "export.readout.tail-up-to",
                "up to { $seconds } s",
                &[(
                    "seconds",
                    format!(
                        "{:.1}",
                        estimate.max_tail_frames as f64 / estimate.sample_rate.max(1) as f64
                    ),
                )],
            ),
        }
    }

    /// A literal statement of what lands on disk, in the user's own naming.
    fn destination_summary(&self, i18n: I18n) -> String {
        let stem = self.export_name();
        let mixdown = format!("{stem}.{}", self.settings.format.extension());
        let count = self.settings.batch_target_count(&self.defaults);
        let folder = format!("{} Stems", sanitize_file_stem(&stem));
        let channels = tr_vars_or(
            i18n,
            "export.readout.batch-writes",
            "{ $folder }/ · { $count } file(s)",
            &[("folder", folder), ("count", count.to_string())],
        );
        match (self.settings.include_mixdown, count > 0) {
            (true, true) => format!("{mixdown} + {channels}"),
            (true, false) => mixdown,
            (false, true) => channels,
            (false, false) => "—".to_string(),
        }
    }

    // ── Status strip and footers ─────────────────────────────────────────────

    /// Pinned band between the scrolling form and the action footer.
    ///
    /// It is outside the scroller on purpose: an error the user has scrolled
    /// past is an error they cannot act on. It wraps rather than truncates,
    /// because `OutputDirMissing` carries the path that has to be fixed.
    fn status_strip(&self, i18n: I18n) -> Option<gpui::AnyElement> {
        let (message, tone) = match (&self.failure, &self.confirm_replace) {
            (None, Some(existing)) => {
                let mut names: Vec<String> = existing
                    .iter()
                    .take(3)
                    .map(|path| file_label(path))
                    .collect();
                if existing.len() > 3 {
                    names.push(tr_vars_or(
                        i18n,
                        "export.overwrite.more",
                        "…and { $count } more",
                        &[("count", (existing.len() - 3).to_string())],
                    ));
                }
                (
                    tr_vars_or(
                        i18n,
                        "export.status.replace",
                        "{ $count } file(s) already exist and will be replaced: { $names }. \
                         Press Replace & Render to go ahead.",
                        &[
                            ("count", existing.len().to_string()),
                            ("names", names.join(", ")),
                        ],
                    ),
                    Colors::status_warning(),
                )
            }
            (Some(message), _) => (
                tr_vars_or(
                    i18n,
                    "export.status.failed",
                    "Cannot render — { $reason }",
                    &[("reason", message.clone())],
                ),
                Colors::status_error(),
            ),
            (None, None) => {
                if self.range_draft_invalid {
                    (
                        i18n.tr_or(
                            "export.status.range-draft",
                            "Cannot export — the custom range needs two numbers in beats.",
                        ),
                        Colors::status_warning(),
                    )
                } else {
                    match &self.estimate {
                        Ok(_) => return None,
                        Err(err) => (
                            tr_vars_or(
                                i18n,
                                "export.status.invalid",
                                "Cannot export — { $reason }",
                                &[("reason", localized_error(&self.language, err))],
                            ),
                            Colors::status_warning(),
                        ),
                    }
                }
            }
        };

        Some(
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(space::BASE))
                .px(px(BODY_PAD_X))
                .py(px(space::BASE))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::composite(
                    Colors::surface_base(),
                    Colors::with_alpha(tone, 0.10),
                ))
                // Second channel: the state is carried by a leading rail as well
                // as by colour, so it survives a colour-blind read.
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(STATUS_RAIL_WIDTH))
                        .h(px(size::MICRO))
                        .bg(tone),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_secondary())
                        .child(message),
                )
                .into_any_element(),
        )
    }

    fn footer_editing(&self, i18n: I18n, target: &Entity<Self>) -> gpui::AnyElement {
        let can_export = self.can_export();
        footer_band()
            .child(fb_button(
                "export-cancel",
                i18n.tr_or("export.action.close", "Close"),
                FbButtonKind::Default,
                true,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.close(window, cx));
                    }
                },
            ))
            .child(fb_button(
                "export-start",
                if self.confirm_replace.is_some() {
                    i18n.tr_or("export.action.replace-render", "Replace & Render")
                } else {
                    i18n.tr_or("export.action.render", "Render")
                },
                FbButtonKind::Primary,
                can_export,
                {
                    let target = target.clone();
                    move |_, window, cx| {
                        let _ = target.update(cx, |this, cx| this.start_render(window, cx));
                    }
                },
            ))
            .into_any_element()
    }

    // ── Choice plumbing ──────────────────────────────────────────────────────

    fn dropdown(
        &self,
        field: SelectField,
        id: &'static str,
        selected: String,
        options: Vec<SelectOption>,
        target: &Entity<Self>,
    ) -> impl IntoElement {
        let open = self.open_select == Some(field);
        let toggle_target = target.clone();
        let change_target = target.clone();
        select(
            id,
            Some(selected.as_str()),
            selected.clone(),
            options,
            open,
            false,
            Arc::new(move |_, _window, cx| {
                let _ = toggle_target.update(cx, |this, cx| {
                    this.open_select = if this.open_select == Some(field) {
                        None
                    } else {
                        Some(field)
                    };
                    cx.notify();
                });
            }),
            Arc::new(move |value, _window, cx| {
                let value = value.clone();
                let _ = change_target.update(cx, |this, cx| {
                    this.apply_select(field, &value);
                    this.open_select = None;
                    this.refresh_estimate();
                    cx.notify();
                });
            }),
        )
    }

    fn apply_select(&mut self, field: SelectField, value: &str) {
        match field {
            SelectField::Format => {
                self.settings.format = match value {
                    "flac" => AudioFileFormat::Flac,
                    "mp3" => AudioFileFormat::Mp3,
                    _ => AudioFileFormat::Wav,
                };
                self.sync_output_from_name();
            }
            SelectField::FormatOption => match self.settings.format {
                AudioFileFormat::Wav => {
                    self.settings.wav_sample_format = match value {
                        "f32" => sphere_encoder::AudioSampleFormat::F32,
                        "i16" => sphere_encoder::AudioSampleFormat::I16,
                        _ => sphere_encoder::AudioSampleFormat::I24,
                    };
                }
                AudioFileFormat::Flac => {
                    self.settings.flac_bit_depth = if value == "16" { 16 } else { 24 };
                }
                AudioFileFormat::Mp3 => {
                    self.settings.mp3_bitrate_kbps = value.parse().unwrap_or(256);
                }
                AudioFileFormat::Rauf => {}
            },
            SelectField::FlacCompression => {
                if let Ok(level) = value.parse::<u8>() {
                    self.settings.flac_compression_level =
                        Some(level.clamp(FLAC_COMPRESSION_RANGE.0, FLAC_COMPRESSION_RANGE.1));
                }
            }
            SelectField::Range => {
                let range = match value {
                    "selection" => self
                        .defaults
                        .time_selection
                        .map(|(start_beat, end_beat)| ExportRangeChoice::TimeSelection {
                            start_beat,
                            end_beat,
                        })
                        .unwrap_or(ExportRangeChoice::EntireArrangement),
                    "loop" => self
                        .defaults
                        .loop_range
                        .map(|(start_beat, end_beat)| ExportRangeChoice::LoopRange {
                            start_beat,
                            end_beat,
                        })
                        .unwrap_or(ExportRangeChoice::EntireArrangement),
                    "custom" => {
                        // Seed from whatever the user last had in the fields, so
                        // switching back and forth does not discard an edit.
                        let fallback_end = self.defaults.content_end_beat.max(1.0);
                        let start = self
                            .range_start_input
                            .value
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .unwrap_or(0.0);
                        let end = self
                            .range_end_input
                            .value
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .unwrap_or(fallback_end);
                        self.seed_range_inputs(start, end);
                        ExportRangeChoice::Custom {
                            start_beat: start,
                            end_beat: end,
                        }
                    }
                    _ => ExportRangeChoice::EntireArrangement,
                };
                self.settings.range = range;
                if !matches!(range, ExportRangeChoice::Custom { .. }) {
                    self.range_draft_invalid = false;
                }
            }
            SelectField::SampleRate => {
                self.settings.sample_rate = match value {
                    "44100" => ExportSampleRateChoice::Hz44100,
                    "48000" => ExportSampleRateChoice::Hz48000,
                    "88200" => ExportSampleRateChoice::Hz88200,
                    "96000" => ExportSampleRateChoice::Hz96000,
                    _ => ExportSampleRateChoice::Project,
                };
            }
            SelectField::Normalize => {
                self.settings.normalize = parse_peak_option(value)
                    .map(ExportNormalizeChoice::PeakDb)
                    .unwrap_or(ExportNormalizeChoice::Off);
            }
            SelectField::Tail => {
                self.settings.tail = match value {
                    "fixed" => ExportTailChoice::FixedSeconds(TAIL_FIXED_SECONDS),
                    "silence" => ExportTailChoice::UntilSilence {
                        max_seconds: TAIL_SILENCE_MAX_SECONDS,
                        threshold_db: TAIL_SILENCE_THRESHOLD_DB,
                    },
                    _ => ExportTailChoice::None,
                };
            }
        }
    }
}

// ── IME bridge ───────────────────────────────────────────────────────────────

/// Multi-field IME bridge.
///
/// Every platform text commit — including CJK/Thai composition, which never
/// passes through `handle_key` — has to reach the focused field *and* re-derive
/// the values that depend on it. Missing that is how the output path silently
/// desyncs from the name in the locales this app actually ships.
impl EntityInputHandler for ExportArrangementWindow {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self
                .range_start_input
                .text_for_utf16_range(range, actual_range),
            Some(ExportField::RangeEnd) => self
                .range_end_input
                .text_for_utf16_range(range, actual_range),
            _ => self.name_input.text_for_utf16_range(range, actual_range),
        }
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self
                .range_start_input
                .selected_text_range_utf16(ignore_disabled_input),
            Some(ExportField::RangeEnd) => self
                .range_end_input
                .selected_text_range_utf16(ignore_disabled_input),
            _ => self
                .name_input
                .selected_text_range_utf16(ignore_disabled_input),
        }
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self.range_start_input.marked_text_range_utf16(),
            Some(ExportField::RangeEnd) => self.range_end_input.marked_text_range_utf16(),
            _ => self.name_input.marked_text_range_utf16(),
        }
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self.range_start_input.unmark_text(),
            Some(ExportField::RangeEnd) => self.range_end_input.unmark_text(),
            _ => self.name_input.unmark_text(),
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => {
                self.range_start_input
                    .replace_text_in_utf16_range(range, text);
                self.sync_range_from_inputs();
            }
            Some(ExportField::RangeEnd) => {
                self.range_end_input
                    .replace_text_in_utf16_range(range, text);
                self.sync_range_from_inputs();
            }
            _ => {
                self.name_input.replace_text_in_utf16_range(range, text);
                self.sync_output_from_name();
            }
        }
        self.refresh_estimate();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => {
                self.range_start_input.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                self.sync_range_from_inputs();
            }
            Some(ExportField::RangeEnd) => {
                self.range_end_input.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                self.sync_range_from_inputs();
            }
            _ => {
                self.name_input.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                self.sync_output_from_name();
            }
        }
        self.refresh_estimate();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let field = self.focused_field(window);
        match field {
            Some(ExportField::RangeStart) => self
                .range_start_input
                .bounds_for_utf16_range(range_utf16, element_bounds),
            Some(ExportField::RangeEnd) => self
                .range_end_input
                .bounds_for_utf16_range(range_utf16, element_bounds),
            _ => self
                .name_input
                .bounds_for_utf16_range(range_utf16, element_bounds),
        }
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// One Channel Selection line: checkbox, name, and the channel's kind, all one
/// click target. The name truncates so a long one never pushes the kind out.
fn channel_row(
    id: impl Into<gpui::ElementId>,
    name: String,
    kind: String,
    checked: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());
    let focus = Colors::state_focus_ring();
    div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(name.clone())
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(move |style| style.shadow(elevation::focus_ring(focus)))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .min_h(px(size::DEFAULT))
        .px(px(space::TIGHT))
        .rounded(px(radius::CONTROL))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .on_click(on_toggle)
        .child(
            div()
                .w(px(14.0))
                .h(px(14.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::border_strong()
                })
                .bg(if checked {
                    Colors::accent_primary()
                } else {
                    Colors::surface_input()
                })
                .when(checked, |checkbox| {
                    checkbox.child(
                        svg()
                            .path(assets::ICON_CHECK_PATH)
                            .w(px(10.0))
                            .h(px(10.0))
                            .text_color(Colors::on_accent()),
                    )
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(typography::UI_SM))
                .text_color(if checked {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .child(name),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(kind),
        )
}

// ── Window-only fields ───────────────────────────────────────────────────────

fn beat_field(
    state: &TextInputState,
    window: &Window,
    target: &Entity<ExportArrangementWindow>,
    get: fn(&mut ExportArrangementWindow) -> &mut TextInputState,
) -> impl IntoElement {
    let focused = state.is_focused(window);
    let callbacks = bind_mouse_selection(target.clone(), get);
    div()
        .w(px(BEAT_FIELD_WIDTH))
        .child(text_field_with_callbacks_and_ime(
            state,
            focused,
            callbacks,
            target.clone(),
        ))
}

// ── Destinations ─────────────────────────────────────────────────────────────

/// Destinations this job would replace. The renderer overwrites silently on
/// success, so this is what turns the Render press into a confirmable action.
fn existing_destinations(job: &RenderJob) -> Vec<PathBuf> {
    job.mixdown
        .iter()
        .chain(job.tracks.iter().map(|target| &target.request))
        .map(|request| request.output_path.clone())
        .filter(|path| path.exists())
        .collect()
}

/// Settings for a window opening on `defaults`: the last render's, minus what
/// belongs to that project — its range, its destination, and channel picks
/// this project does not have.
fn remembered_settings(defaults: &ExportProjectDefaults) -> ExportSettings {
    let Some(mut settings) = LAST_SETTINGS.lock().ok().and_then(|last| last.clone()) else {
        return ExportSettings::default();
    };
    settings.output_path = None;
    settings.range = ExportRangeChoice::EntireArrangement;
    settings
        .selected_tracks
        .retain(|id| defaults.track_targets.iter().any(|target| target.id == *id));
    if settings.render_mode == ExportRenderMode::Realtime && defaults.live_sample_rate == 0 {
        settings.render_mode = ExportRenderMode::Offline;
    }
    settings
}

/// Stem for the mixdown file / stems folder prefix. Empty input falls back to
/// "Export" (not "Track" — that default is for unnamed mixer channels).
fn export_stem_from_input(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "Export".to_string();
    }
    sanitize_file_stem(trimmed)
}

// ── Opener ───────────────────────────────────────────────────────────────────

/// Open the external Export Arrangement window centered over `owner_bounds`.
pub fn open_export_arrangement_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    project_name: String,
    snapshot: EngineProjectSnapshot,
    bridge_sinks: PluginBridgeSinkMap,
    audio_engine: Option<DirectAudio::AudioEngine>,
    defaults: ExportProjectDefaults,
    default_output: Option<PathBuf>,
    intent: ExportIntent,
    realtime_hooks: Option<RealtimeTransportHooks>,
    cx: &mut App,
) -> Result<WindowHandle<ExportArrangementWindow>, String> {
    use crate::window_position::{apply_owner_display, centered_window_bounds};
    use gpui::{size, WindowBackgroundAppearance, WindowBounds, WindowKind};

    let height = TITLEBAR_HEIGHT + EXPORT_WINDOW_HEIGHT;
    let window_bounds =
        centered_window_bounds(owner_bounds, size(px(EXPORT_WINDOW_WIDTH), px(height)), cx);

    let mut window_options = crate::platform_chrome::external_dialog_window_options_partial();
    window_options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    window_options.kind = WindowKind::Dialog;
    // Resizable so the form can be validated (and used) at narrow, short and
    // maximized sizes; the body owns the scroll that makes that safe.
    window_options.is_resizable = true;
    window_options.is_minimizable = false;
    window_options.window_background = WindowBackgroundAppearance::Transparent;
    window_options.window_min_size = Some(size(
        px(EXPORT_WINDOW_MIN_WIDTH),
        px(TITLEBAR_HEIGHT + EXPORT_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut window_options, owner_bounds, cx);

    cx.open_window(window_options, move |_window, cx| {
        cx.new(|cx| {
            let mut win = ExportArrangementWindow::new(
                project_name,
                snapshot,
                bridge_sinks,
                audio_engine,
                defaults,
                intent,
                realtime_hooks,
                cx,
            );
            if let Some(path) = default_output {
                win.set_default_output(path);
            }
            win
        })
    })
    .map_err(|e| e.to_string())
}
