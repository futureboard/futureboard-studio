//! Project Settings window.
//!
//! Everything here belongs to *the open project*, not to the application:
//! tempo, meter, key, and the sample rate the project is worked at. Application
//! preferences (themes, keymaps, plugin folders, audio devices, the defaults
//! used when creating a *new* project) stay in the Settings window — the two
//! were previously reached through the same surface, which made "Project
//! Settings" open a dialog whose contents were mostly not about the project.
//!
//! The window renders a snapshot pushed by `StudioLayout` and sends edits back
//! through callbacks, so `TimelineState` remains the owner of the project values
//! shown. Nothing here mutates project state directly and
//! nothing here caches an edit: a value changes on screen because the studio
//! accepted it and pushed a new snapshot.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, DragMoveEvent, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind,
};

use crate::components::app_chrome::{next_bpm_drag_id, BpmDrag, BpmDragSample};
use crate::components::controls::{
    fb_button, fb_checkbox, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::form::{select, select_dismiss_backdrop, SelectOption};
use crate::components::settings_components::settings_segmented;
use crate::components::settings_layout::{
    settings_daw_row_with_description, settings_section_card, settings_section_hint,
    settings_section_title, settings_status_badge, settings_value_readout,
};
use crate::components::timeline::timeline_state::{
    MidiScale, ScaleKind, ScaleRoot, TimeDisplayFormat, TimecodeRate,
};
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{self, radius, size, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const PROJECT_SETTINGS_WINDOW_WIDTH: f32 = 600.0;
pub const PROJECT_SETTINGS_WINDOW_HEIGHT: f32 = 720.0;
/// Width of a dropdown or value field in the control column.
const CONTROL_WIDTH: f32 = 180.0;

/// Sample rates the project can be worked at. Same list the audio settings
/// offer, because this control routes through the same engine-restart flow.
const SAMPLE_RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 192_000];
const SAMPLE_RATE_LABELS: [&str; 5] = ["44.1", "48", "88.2", "96", "192"];

/// Time signatures offered for the project's base meter.
const TIME_SIGNATURES: [(u32, u32); 8] = [
    (4, 4),
    (3, 4),
    (2, 4),
    (6, 8),
    (5, 4),
    (7, 8),
    (9, 8),
    (12, 8),
];

/// The live project state this window shows. Built by `StudioLayout` from
/// `TimelineState` plus the active engine-rate diagnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSettingsSnapshot {
    pub name: String,
    /// Project file on disk; `None` for an unsaved project.
    pub path: Option<PathBuf>,
    pub is_dirty: bool,
    /// Base project tempo (the tempo at beat 0).
    pub bpm: f32,
    /// Base time signature (the meter at beat 0).
    pub time_signature: (u32, u32),
    /// `true` when the project has tempo automation beyond the base tempo, so
    /// the base value is not the tempo everywhere.
    pub has_tempo_markers: bool,
    /// `true` when the project has meter changes beyond the base signature.
    pub has_time_signature_markers: bool,
    /// The project key; `None` when no key is set.
    pub project_key: Option<MidiScale>,
    pub sample_rate: u32,
    /// Rate the audio engine is actually running at, when a stream is open.
    pub engine_sample_rate: Option<u32>,
    /// Unit the ruler and every position readout are shown in.
    pub time_display_format: TimeDisplayFormat,
    /// Frame rate Timecode is counted at.
    pub timecode_rate: TimecodeRate,
    pub track_count: usize,
}

impl Default for ProjectSettingsSnapshot {
    fn default() -> Self {
        Self {
            name: "Untitled Project".to_string(),
            path: None,
            is_dirty: false,
            bpm: 120.0,
            time_signature: (4, 4),
            has_tempo_markers: false,
            has_time_signature_markers: false,
            project_key: None,
            sample_rate: 48_000,
            engine_sample_rate: None,
            time_display_format: TimeDisplayFormat::default(),
            timecode_rate: TimecodeRate::default(),
            track_count: 0,
        }
    }
}

/// Edits the window sends back to the studio. Each one is applied through the
/// studio's existing command path (tempo → transport, meter → the time
/// signature map, sample rate → the project-owned engine-reopen flow), so this
/// window adds no second way to change them.
#[derive(Clone)]
pub struct ProjectSettingsCallbacks {
    pub on_bpm_drag: Arc<dyn Fn(BpmDragSample, &mut App) + Send + Sync>,
    /// Pointer release after a BPM scrub — closes the gesture as one undo entry.
    pub on_bpm_drag_end: Arc<dyn Fn(&mut App) + Send + Sync>,
    pub on_set_time_signature: Arc<dyn Fn(u32, u32, &mut App) + Send + Sync>,
    /// Set or clear (`None`) the project key.
    pub on_set_project_key: Arc<dyn Fn(Option<MidiScale>, &mut App) + Send + Sync>,
    pub on_set_sample_rate: Arc<dyn Fn(u32, &mut App) + Send + Sync>,
    pub on_set_time_display_format: Arc<dyn Fn(TimeDisplayFormat, &mut App) + Send + Sync>,
    pub on_set_timecode_rate: Arc<dyn Fn(TimecodeRate, &mut App) + Send + Sync>,
    pub on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
}

/// Which dropdown is open. Only one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMenu {
    TimeSignature,
    KeyScale,
    TimecodeRate,
}

pub struct ProjectSettingsWindow {
    focus_handle: FocusHandle,
    snapshot: ProjectSettingsSnapshot,
    callbacks: ProjectSettingsCallbacks,
    open_menu: Option<OpenMenu>,
}

impl ProjectSettingsWindow {
    fn new(
        snapshot: ProjectSettingsSnapshot,
        callbacks: ProjectSettingsCallbacks,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            snapshot,
            callbacks,
            open_menu: None,
        }
    }

    /// Adopt a snapshot pushed by the studio. Returns `true` when anything
    /// changed, so the caller only notifies on a real update.
    pub fn set_snapshot(&mut self, snapshot: ProjectSettingsSnapshot) -> bool {
        if self.snapshot == snapshot {
            return false;
        }
        self.snapshot = snapshot;
        true
    }

    fn toggle_menu(&mut self, menu: OpenMenu, cx: &mut Context<Self>) {
        self.open_menu = if self.open_menu == Some(menu) {
            None
        } else {
            Some(menu)
        };
        cx.notify();
    }
}

impl Render for ProjectSettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot.clone();
        let on_close = self.callbacks.on_close.clone();

        let mut root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key.as_str() != "escape" {
                    return;
                }
                // Escape cancels the transient dropdown before the window.
                if this.open_menu.take().is_some() {
                    cx.notify();
                } else {
                    window.remove_window();
                }
            }))
            .child(external_window_titlebar(
                "Project Settings",
                "project-settings-close",
                move |window, cx| on_close(window, cx),
            ))
            .child(project_header(&snapshot))
            .child(
                div()
                    .id("project-settings-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .gap(px(space::LOOSE))
                    .px(px(space::SECTION))
                    .py(px(space::LOOSE))
                    .child(self.tempo_section(&snapshot, cx))
                    .child(self.key_section(&snapshot, cx))
                    .child(self.timebase_section(&snapshot, cx))
                    .child(self.audio_section(&snapshot, cx)),
            )
            .child(footer(self.callbacks.on_close.clone()));

        if self.open_menu.is_some() {
            let target = cx.entity().clone();
            root = root.child(select_dismiss_backdrop(Arc::new(
                move |_: &(), _window, cx| {
                    let _ = target.update(cx, |this, cx| {
                        this.open_menu = None;
                        cx.notify();
                    });
                },
            )));
        }
        root
    }
}

impl ProjectSettingsWindow {
    /// A dropdown bound to one [`OpenMenu`]. `on_pick` receives the chosen
    /// option id; the menu closes before it runs.
    #[allow(clippy::too_many_arguments)]
    fn dropdown(
        &self,
        id: &'static str,
        menu: OpenMenu,
        selected: Option<&str>,
        placeholder: &'static str,
        options: Vec<SelectOption>,
        disabled: bool,
        on_pick: impl Fn(&str, &mut Self, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let toggle = cx.entity().clone();
        let change = cx.entity().clone();
        div()
            .w(px(CONTROL_WIDTH))
            .child(select(
                id,
                selected,
                placeholder,
                options,
                self.open_menu == Some(menu),
                disabled,
                Arc::new(move |_: &(), _w, cx| {
                    let _ = toggle.update(cx, |this, cx| this.toggle_menu(menu, cx));
                }),
                // The studio callbacks defer their own work, so calling them
                // from inside this window's update cannot re-enter it.
                Arc::new(move |value: &String, _w, cx| {
                    let _ = change.update(cx, |this, cx| {
                        this.open_menu = None;
                        cx.notify();
                        on_pick(value, this, cx);
                    });
                }),
            ))
            .into_any_element()
    }

    fn tempo_section(
        &self,
        snapshot: &ProjectSettingsSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected_ts = format!(
            "{}/{}",
            snapshot.time_signature.0, snapshot.time_signature.1
        );
        let meter = self.dropdown(
            "project-settings-time-signature",
            OpenMenu::TimeSignature,
            Some(selected_ts.as_str()),
            "-",
            TIME_SIGNATURES
                .iter()
                .map(|(num, den)| {
                    let label = format!("{num}/{den}");
                    SelectOption::new(label.clone(), label)
                })
                .collect(),
            false,
            |value, this, cx| {
                let Some((num, den)) = value.split_once('/') else {
                    return;
                };
                if let (Ok(num), Ok(den)) = (num.parse::<u32>(), den.parse::<u32>()) {
                    (this.callbacks.on_set_time_signature)(num, den, cx);
                }
            },
            cx,
        );

        section(
            "Tempo & Meter",
            "The project's base tempo and meter at bar 1.",
        )
        .child(settings_daw_row_with_description(
            "Tempo",
            Some("Drag up or down · Shift for fine".to_string()),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(self.bpm_scrub_field(snapshot.bpm))
                .when(snapshot.has_tempo_markers, |row| {
                    row.child(settings_status_badge("Tempo map active", false))
                }),
        ))
        .child(settings_daw_row_with_description(
            "Time signature",
            None,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .child(meter)
                .when(snapshot.has_time_signature_markers, |row| {
                    row.child(settings_status_badge("Meter changes", false))
                }),
        ))
    }

    /// Key — the project's root and scale. Metadata for editors and tools;
    /// nothing plays differently when it changes.
    fn key_section(
        &self,
        snapshot: &ProjectSettingsSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = snapshot.project_key;
        let on_set_key = self.callbacks.on_set_project_key.clone();

        // Twelve roots as two rows of six: every choice visible, no menu.
        let root_row = |roots: &[ScaleRoot]| {
            let last = roots.len() - 1;
            fb_segmented_track().children(roots.iter().enumerate().map(|(index, root)| {
                let root = *root;
                let on_set_key = on_set_key.clone();
                let position = match index {
                    0 => FbSegment::First,
                    i if i == last => FbSegment::Last,
                    _ => FbSegment::Middle,
                };
                fb_segment(
                    ("project-settings-key-root", root.pitch_class() as usize),
                    root.label(),
                    key.is_some_and(|key| key.root == root),
                    position,
                    move |_, _, cx| {
                        let kind = key.map(|key| key.kind).unwrap_or(ScaleKind::Major);
                        on_set_key(Some(MidiScale::new(root, kind)), cx);
                    },
                )
            }))
        };
        let roots = div()
            .flex()
            .flex_col()
            .gap(px(space::TIGHT))
            .child(root_row(&ScaleRoot::ALL[..6]))
            .child(root_row(&ScaleRoot::ALL[6..]));

        let selected_scale = key.map(|key| key.kind.to_tag().to_string());
        let scale = self.dropdown(
            "project-settings-key-scale",
            OpenMenu::KeyScale,
            selected_scale.as_deref(),
            "Pick a root first",
            MidiScale::KEY_KINDS
                .iter()
                .map(|kind| SelectOption::new(kind.to_tag().to_string(), kind.label()))
                .collect(),
            key.is_none(),
            |value, this, cx| {
                let Some(kind) = value.parse::<u8>().ok().and_then(ScaleKind::from_tag) else {
                    return;
                };
                if let Some(key) = this.snapshot.project_key {
                    (this.callbacks.on_set_project_key)(Some(MidiScale::new(key.root, kind)), cx);
                }
            },
            cx,
        );

        let clear = {
            let on_set_key = on_set_key.clone();
            fb_checkbox(
                "project-settings-no-key",
                "No key",
                key.is_none(),
                true,
                move |_, _, cx| {
                    // Unticking "No key" sets the key the transport shows by
                    // default, so the control always does something visible.
                    if key.is_some() {
                        on_set_key(None, cx);
                    } else {
                        on_set_key(Some(MidiScale::new(ScaleRoot::C, ScaleKind::Major)), cx);
                    }
                },
            )
        };

        section(
            "Key",
            "Used by the chord tools and scale-aware editing. Playback is unchanged.",
        )
        .child(settings_daw_row_with_description(
            "Root",
            Some(
                key.map(|key| key.label())
                    .unwrap_or_else(|| "No key set".to_string()),
            ),
            roots,
        ))
        .child(settings_daw_row_with_description(
            "Scale",
            None,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::LOOSE))
                .child(scale)
                .child(clear),
        ))
    }

    /// Timebase — the unit the ruler and every position readout are shown in.
    ///
    /// Display only. The arrangement stays in musical coordinates whatever is
    /// picked here, so switching timebase never moves a clip.
    fn timebase_section(
        &self,
        snapshot: &ProjectSettingsSnapshot,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_format = self.callbacks.on_set_time_display_format.clone();
        let formats: Vec<(TimeDisplayFormat, &'static str)> = TimeDisplayFormat::ALL
            .iter()
            .map(|format| (*format, format.label()))
            .collect();
        let shows_timecode = snapshot.time_display_format == TimeDisplayFormat::Timecode;
        let selected_rate = snapshot.timecode_rate.label();
        let frame_rate = self.dropdown(
            "project-settings-timecode-rate",
            OpenMenu::TimecodeRate,
            Some(selected_rate),
            "-",
            TimecodeRate::ALL
                .iter()
                .map(|rate| SelectOption::new(rate.label().to_string(), rate.label()))
                .collect(),
            !shows_timecode,
            |value, this, cx| {
                if let Some(rate) = TimecodeRate::ALL
                    .iter()
                    .copied()
                    .find(|candidate| candidate.label() == value)
                {
                    (this.callbacks.on_set_timecode_rate)(rate, cx);
                }
            },
            cx,
        );

        section(
            "Timebase",
            "How the ruler and position readouts count. Clips never move.",
        )
        .child(settings_daw_row_with_description(
            "Display",
            None,
            settings_segmented(
                "project-settings-timebase",
                &formats,
                snapshot.time_display_format,
                Arc::new(move |format, _window, cx| on_format(format, cx)),
            ),
        ))
        .child(settings_daw_row_with_description(
            "Frame rate",
            Some(if shows_timecode {
                "Counting Timecode frames".to_string()
            } else {
                "Used when counting Timecode".to_string()
            }),
            frame_rate,
        ))
    }

    fn audio_section(
        &self,
        snapshot: &ProjectSettingsSnapshot,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_rate = self.callbacks.on_set_sample_rate.clone();
        let rates: Vec<(u32, &'static str)> = SAMPLE_RATES
            .iter()
            .zip(SAMPLE_RATE_LABELS)
            .map(|(rate, label)| (*rate, label))
            .collect();
        let running = snapshot.engine_sample_rate;
        let matches = running.is_some_and(|rate| rate == snapshot.sample_rate);

        section("Audio", "The rate this project is recorded and mixed at.")
            .child(settings_daw_row_with_description(
                "Sample rate",
                Some("kHz".to_string()),
                settings_segmented(
                    "project-settings-sample-rate",
                    &rates,
                    snapshot.sample_rate,
                    Arc::new(move |rate, _window, cx| on_rate(rate, cx)),
                ),
            ))
            .child(settings_daw_row_with_description(
                "Engine",
                None,
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .child(settings_value_readout(
                        running
                            .map(format_sample_rate)
                            .unwrap_or_else(|| "Stopped".to_string()),
                    ))
                    .children(running.map(|_| {
                        settings_status_badge(
                            if matches {
                                "Running at the project rate"
                            } else {
                                "Restart audio to apply"
                            },
                            matches,
                        )
                    })),
            ))
    }

    fn bpm_scrub_field(&self, bpm: f32) -> gpui::AnyElement {
        let on_bpm_drag = self.callbacks.on_bpm_drag.clone();
        let on_bpm_drag_end_up = self.callbacks.on_bpm_drag_end.clone();
        let on_bpm_drag_end_out = self.callbacks.on_bpm_drag_end.clone();
        let rest = Colors::surface_input();
        let hover = Colors::composite(rest, Colors::state_hover());

        div()
            .id("project-settings-bpm")
            .w(px(CONTROL_WIDTH * 0.6))
            .h(px(size::COMFORTABLE))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::SNUG))
            .px(px(space::BASE))
            .rounded(px(radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(rest)
            .cursor(gpui::CursorStyle::ResizeUpDown)
            .hover(move |style| style.bg(hover))
            .child(
                div()
                    .flex_1()
                    .text_size(px(typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(format!("{bpm:.2}")),
            )
            .child(
                div()
                    .text_size(px(typography::DENSE_CAPTION))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_faint())
                    .child("BPM"),
            )
            .occlude()
            .on_drag(
                BpmDrag {
                    drag_id: 0,
                    start_bpm: bpm,
                },
                move |drag, _offset, _window, cx| {
                    cx.new(|_| BpmDrag {
                        drag_id: next_bpm_drag_id(),
                        start_bpm: drag.start_bpm,
                    })
                },
            )
            .on_drag_move::<BpmDrag>(move |event: &DragMoveEvent<BpmDrag>, _window, cx| {
                let drag = event.drag(cx);
                let modifiers = event.event.modifiers;
                on_bpm_drag(
                    BpmDragSample {
                        drag_id: drag.drag_id,
                        start_bpm: drag.start_bpm,
                        cur_y: event.event.position.y.into(),
                        shift: modifiers.shift,
                        control: modifiers.control,
                        platform: modifiers.platform,
                        alt: modifiers.alt,
                    },
                    cx,
                );
            })
            // Release closes the scrub as one undo entry. Wired on and off the
            // element for the same reason as the transport BPM box; a release
            // with no scrub in flight is a no-op.
            .on_mouse_up(
                gpui::MouseButton::Left,
                move |_: &gpui::MouseUpEvent, _window, cx| {
                    on_bpm_drag_end_up(cx);
                },
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                move |_: &gpui::MouseUpEvent, _window, cx| {
                    on_bpm_drag_end_out(cx);
                },
            )
            .into_any_element()
    }
}

/// A settings card: title, one-line purpose, then rows.
fn section(title: &'static str, hint: &'static str) -> gpui::Div {
    settings_section_card()
        .flex_shrink_0()
        .child(settings_section_title(title))
        .child(settings_section_hint(hint))
}

/// Project identity band under the titlebar: name, save state, location and
/// a one-line summary of what the sections below hold.
fn project_header(snapshot: &ProjectSettingsSnapshot) -> impl IntoElement {
    let location = snapshot
        .path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Not saved yet".to_string());
    let (status, saved) = if snapshot.is_dirty {
        ("Unsaved changes", false)
    } else if snapshot.path.is_none() {
        ("Not saved", false)
    } else {
        ("Saved", true)
    };
    let tracks = match snapshot.track_count {
        1 => "1 track".to_string(),
        count => format!("{count} tracks"),
    };
    let key = snapshot
        .project_key
        .map(|key| key.label())
        .unwrap_or_else(|| "No key".to_string());
    let summary = format!(
        "{:.2} BPM · {}/{} · {} · {} · {}",
        snapshot.bpm,
        snapshot.time_signature.0,
        snapshot.time_signature.1,
        key,
        format_sample_rate(snapshot.sample_rate),
        tracks
    );

    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(space::HAIR))
        .px(px(space::SECTION))
        .py(px(space::LOOSE))
        .bg(Colors::surface_panel())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::LOOSE))
                .child(
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(snapshot.name.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .child(settings_status_badge(status, saved)),
                ),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_faint())
                .child(location),
        )
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(summary),
        )
}

fn format_sample_rate(rate: u32) -> String {
    if rate % 1_000 == 0 {
        format!("{} kHz", rate / 1_000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1_000.0)
    }
}

fn footer(on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .flex_none()
        .px(px(space::SECTION))
        .py(px(space::BASE))
        .bg(Colors::surface_panel())
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .child(fb_button(
            "project-settings-done",
            "Done",
            FbButtonKind::Primary,
            true,
            move |_, window, cx| on_close(window, cx),
        ))
}

pub fn open_project_settings_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    snapshot: ProjectSettingsSnapshot,
    callbacks: ProjectSettingsCallbacks,
    cx: &mut App,
) -> Result<WindowHandle<ProjectSettingsWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(
            px(PROJECT_SETTINGS_WINDOW_WIDTH),
            px(PROJECT_SETTINGS_WINDOW_HEIGHT),
        ),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Opaque;
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| ProjectSettingsWindow::new(snapshot, callbacks, cx))
    })
    .map_err(|error| error.to_string())
}
