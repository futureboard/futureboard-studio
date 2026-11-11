//! Process Explorer — what each plug-in in the session is costing.
//!
//! The Performance Monitor answers "is the machine coping"; this answers
//! "which plug-in is it". One row per loaded plug-in — built-in, VST3, VST2,
//! CLAP — with:
//!
//! ```txt
//! DSP      its share of the audio callback's deadline, measured around its
//!          own process() (by the plug-in host for a bridged one)
//! Latency  what it adds to delay compensation
//! Process  where it runs: inside Studio, or which plug-in host
//! Memory   / CPU of that host process, which several plug-ins can share
//! ```
//!
//! and below them, the plug-in host processes themselves.
//!
//! Every figure that cannot be measured is drawn as `—`: a plug-in running
//! inside Studio has no memory of its own to report, and a number standing in
//! for that would be invented.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBounds,
    WindowHandle,
};

use crate::components::performance_window::bytes_label;
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{self, radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const PROCESS_EXPLORER_WIDTH: f32 = 920.0;
pub const PROCESS_EXPLORER_HEIGHT: f32 = 560.0;
const MIN_WIDTH: f32 = 680.0;
const MIN_HEIGHT: f32 = 360.0;

/// Repaint and sampling cadence. The DSP figures are already smoothed in the
/// engine; twice a second shows a spike while the user is still looking.
const REFRESH: Duration = Duration::from_millis(500);

const ROW_H: f32 = 30.0;
/// Tallest the host list gets before it scrolls: about five hosts.
const HOST_PANEL_MAX_H: f32 = 210.0;
const COL_TRACK: f32 = 132.0;
const COL_KIND: f32 = 64.0;
const COL_PROCESS: f32 = 104.0;
const COL_DSP: f32 = 140.0;
const COL_LATENCY: f32 = 76.0;
const COL_MEMORY: f32 = 92.0;
const COL_CPU: f32 = 64.0;

/// What a plug-in is, as the filter and the type column show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginKind {
    BuiltIn,
    Vst3,
    Vst2,
    Clap,
    Other,
}

impl PluginKind {
    pub fn label(self) -> &'static str {
        match self {
            PluginKind::BuiltIn => "Built-in",
            PluginKind::Vst3 => "VST3",
            PluginKind::Vst2 => "VST",
            PluginKind::Clap => "CLAP",
            PluginKind::Other => "Other",
        }
    }
}

/// One loaded plug-in, as the Studio reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginProcessRow {
    pub insert_id: String,
    pub plugin_name: String,
    pub vendor: Option<String>,
    pub track_name: String,
    pub track_color: gpui::Rgba,
    pub kind: PluginKind,
    /// The plug-in host process it runs in; `None` inside Studio.
    pub host_pid: Option<u32>,
    /// Share of the callback deadline, 0..1+. `None` while the engine has no
    /// such insert (not yet in the graph, or no stream open).
    pub dsp_load: Option<f32>,
    pub latency_samples: u32,
    pub bypassed: bool,
    /// Load or runtime trouble worth a word: "Loading", "Crashed", "Missing".
    pub problem: Option<String>,
}

/// What the Studio hands over each refresh. Read by the owner, never by this
/// window, so it never touches the engine or a leased entity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessExplorerReading {
    pub rows: Vec<PluginProcessRow>,
    pub sample_rate: u32,
}

pub type ProcessExplorerReader = Arc<dyn Fn(&mut App) -> ProcessExplorerReading + Send + Sync>;

/// One plug-in host process, measured here.
#[derive(Clone, Debug, Default, PartialEq)]
struct HostSample {
    pid: u32,
    memory_bytes: Option<u64>,
    /// Share of the whole machine since the last sample, 0..1.
    cpu: Option<f32>,
    /// Plug-ins this host carries, as the host registry lists them (insert
    /// ids).
    instance_ids: Vec<String>,
}

impl HostSample {
    fn instances(&self) -> usize {
        self.instance_ids.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KindFilter {
    All,
    Only(PluginKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    Dsp,
    Memory,
    Name,
}

pub struct ProcessExplorerWindow {
    focus_handle: FocusHandle,
    reading: ProcessExplorerReading,
    hosts: HashMap<u32, HostSample>,
    /// The previous CPU-time reading of each host, for its share since then.
    cpu_marks: HashMap<u32, (u64, Instant)>,
    filter: KindFilter,
    sort: SortKey,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
}

impl ProcessExplorerWindow {
    fn new(
        reading: ProcessExplorerReading,
        read: ProcessExplorerReader,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The same split as the Performance Monitor: take the Studio's reading
        // with nothing leased, then apply it; the process syscalls run here too
        // and never in `render`.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH).await;
            let reading = cx.update(|cx| read(cx));
            let updated = this.update(cx, |view, cx| {
                view.reading = reading;
                view.sample_hosts();
                cx.notify();
            });
            if updated.is_err() {
                break;
            }
        })
        .detach();

        let mut view = Self {
            focus_handle: cx.focus_handle(),
            reading,
            hosts: HashMap::new(),
            cpu_marks: HashMap::new(),
            filter: KindFilter::All,
            sort: SortKey::Dsp,
            on_close,
        };
        view.sample_hosts();
        view
    }

    /// Memory and CPU of every running plug-in host.
    fn sample_hosts(&mut self) {
        use SpherePluginHost::plugin_host_lifecycle::{BridgeHostManager, HostLifecycleState};
        let now = Instant::now();
        let cores = crate::perf::logical_cores().max(1) as f64;
        let mut hosts = HashMap::new();
        for record in BridgeHostManager::global().host_records() {
            if record.state != HostLifecycleState::Running {
                continue;
            }
            let counters = crate::perf::process_counters(record.pid);
            let cpu = counters.and_then(|(_, cpu_100ns)| {
                let previous = self.cpu_marks.insert(record.pid, (cpu_100ns, now));
                let (before, at) = previous?;
                let wall_100ns = now.duration_since(at).as_nanos() as f64 / 100.0;
                (wall_100ns > 0.0)
                    .then(|| (cpu_100ns.saturating_sub(before) as f64 / wall_100ns / cores) as f32)
            });
            hosts.insert(
                record.pid,
                HostSample {
                    pid: record.pid,
                    memory_bytes: counters.map(|(memory, _)| memory),
                    cpu,
                    instance_ids: record.instance_ids.clone(),
                },
            );
        }
        // A host that has gone takes its CPU mark with it, so a recycled PID
        // starts from a fresh reading.
        self.cpu_marks.retain(|pid, _| hosts.contains_key(pid));
        self.hosts = hosts;
    }

    /// The host `row` runs in: by its slot's pid, else by the registry's
    /// instance list.
    fn host_of(&self, row: &PluginProcessRow) -> Option<&HostSample> {
        row.host_pid
            .and_then(|pid| self.hosts.get(&pid))
            .or_else(|| {
                self.hosts
                    .values()
                    .find(|host| host.instance_ids.contains(&row.insert_id))
            })
    }

    fn visible_rows(&self) -> Vec<&PluginProcessRow> {
        let mut rows: Vec<&PluginProcessRow> = self
            .reading
            .rows
            .iter()
            .filter(|row| match self.filter {
                KindFilter::All => true,
                KindFilter::Only(kind) => row.kind == kind,
            })
            .collect();
        let memory = |row: &PluginProcessRow| {
            self.host_of(row)
                .and_then(|host| host.memory_bytes)
                .unwrap_or(0)
        };
        match self.sort {
            SortKey::Dsp => rows.sort_by(|a, b| {
                b.dsp_load
                    .unwrap_or(-1.0)
                    .total_cmp(&a.dsp_load.unwrap_or(-1.0))
                    .then_with(|| a.plugin_name.cmp(&b.plugin_name))
            }),
            SortKey::Memory => rows.sort_by(|a, b| {
                memory(b)
                    .cmp(&memory(a))
                    .then_with(|| a.plugin_name.cmp(&b.plugin_name))
            }),
            SortKey::Name => rows.sort_by(|a, b| {
                a.plugin_name
                    .to_lowercase()
                    .cmp(&b.plugin_name.to_lowercase())
                    .then_with(|| a.track_name.cmp(&b.track_name))
            }),
        }
        rows
    }
}

impl Render for ProcessExplorerWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.visible_rows();
        // The heaviest single plug-in, not a sum: bridged plug-ins run in
        // parallel host processes, so their shares add past 100 % with nothing
        // overloaded.
        let heaviest: Option<f32> = self
            .reading
            .rows
            .iter()
            .filter_map(|row| row.dsp_load)
            .reduce(f32::max);
        let host_memory: u64 = self
            .hosts
            .values()
            .filter_map(|host| host.memory_bytes)
            .sum();
        let host_cpu: f32 = self.hosts.values().filter_map(|host| host.cpu).sum();
        let counts = kind_counts(&self.reading.rows);

        let titlebar = (!crate::platform_chrome::native_tool_window_frame()).then(|| {
            let on_close = self.on_close.clone();
            external_window_titlebar(
                "Process Explorer",
                "process-explorer-close",
                move |window, cx| {
                    on_close(window, cx);
                    window.remove_window();
                },
            )
        });

        let summary = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(space::BASE))
            .child(stat_tile(
                "Plug-ins",
                self.reading.rows.len().to_string(),
                None,
            ))
            .child(stat_tile(
                "Heaviest plug-in",
                percent(heaviest),
                heaviest.map(load_color),
            ))
            .child(stat_tile(
                "Host processes",
                self.hosts.len().to_string(),
                None,
            ))
            .child(stat_tile("Host memory", bytes_label(host_memory), None))
            .child(stat_tile(
                "Host CPU",
                percent(Some(host_cpu)),
                Some(load_color(host_cpu)),
            ));

        let filters = [
            (KindFilter::All, "All", self.reading.rows.len()),
            (KindFilter::Only(PluginKind::BuiltIn), "Built-in", counts.0),
            (KindFilter::Only(PluginKind::Vst3), "VST3", counts.1),
            (KindFilter::Only(PluginKind::Vst2), "VST", counts.2),
            (KindFilter::Only(PluginKind::Clap), "CLAP", counts.3),
        ];
        let filter_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .children(
                filters
                    .into_iter()
                    .enumerate()
                    .map(|(i, (filter, label, n))| {
                        let active = self.filter == filter;
                        filter_pill(("process-filter", i), label, n, active).on_click(cx.listener(
                            move |this, _event: &gpui::ClickEvent, _window, cx| {
                                this.filter = filter;
                                cx.notify();
                            },
                        ))
                    }),
            );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(26.0))
            .px(px(space::BASE))
            .gap(px(space::BASE))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(typography::UI_XS))
            .text_color(Colors::text_muted())
            .child(
                sort_header("process-sort-name", "Plug-in", self.sort == SortKey::Name)
                    .flex_1()
                    .min_w_0()
                    .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                        this.sort = SortKey::Name;
                        cx.notify();
                    })),
            )
            .child(column("Track", COL_TRACK))
            .child(column("Type", COL_KIND))
            .child(column("Runs in", COL_PROCESS))
            .child(
                sort_header("process-sort-dsp", "DSP", self.sort == SortKey::Dsp)
                    .w(px(COL_DSP))
                    .flex_none()
                    .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                        this.sort = SortKey::Dsp;
                        cx.notify();
                    })),
            )
            .child(column("Latency", COL_LATENCY))
            .child(
                sort_header(
                    "process-sort-memory",
                    "Memory",
                    self.sort == SortKey::Memory,
                )
                .w(px(COL_MEMORY))
                .flex_none()
                .on_click(cx.listener(|this, _e: &gpui::ClickEvent, _w, cx| {
                    this.sort = SortKey::Memory;
                    cx.notify();
                })),
            )
            .child(column("CPU", COL_CPU));

        let body: Vec<gpui::AnyElement> = if rows.is_empty() {
            vec![empty_state(if self.reading.rows.is_empty() {
                "No plug-ins are loaded in this project."
            } else {
                "No plug-ins of this type are loaded."
            })]
        } else {
            rows.iter()
                .enumerate()
                .map(|(i, row)| plugin_row(i, row, self.host_of(row), self.reading.sample_rate))
                .collect()
        };

        let mut hosts: Vec<&HostSample> = self.hosts.values().collect();
        hosts.sort_by_key(|host| std::cmp::Reverse(host.memory_bytes.unwrap_or(0)));
        let host_rows: Vec<gpui::AnyElement> = hosts
            .into_iter()
            .map(|host| host_row(host, &self.reading.rows))
            .collect();

        // One scroll owner per list: the plug-in table scrolls under its
        // header, the host list under its title, and the summary stays put.
        // The window used to scroll as one body, but its panels shrank to fit
        // instead of overflowing, so a long session was cut off with no way to
        // reach the rest.
        let plugin_panel = panel()
            .flex_1()
            .min_h(px(160.0))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(space::BASE))
                    .py(px(space::SNUG))
                    .child(div().text_size(px(typography::UI_TITLE)).child("Plug-ins"))
                    .child(filter_bar),
            )
            .child(header.flex_none())
            .child(
                div()
                    .id("process-explorer-plugins")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(body),
            );
        let host_panel = panel()
            .flex_none()
            .max_h(px(HOST_PANEL_MAX_H))
            .child(
                div()
                    .flex_none()
                    .px(px(space::BASE))
                    .py(px(space::SNUG))
                    .text_size(px(typography::UI_TITLE))
                    .child("Plug-in host processes"),
            )
            .child(
                div()
                    .id("process-explorer-hosts")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .when(host_rows.is_empty(), |list| {
                        list.child(empty_state(
                            "No plug-in host is running: every loaded plug-in runs inside Studio.",
                        ))
                    })
                    .children(host_rows),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .text_size(px(typography::UI_SM))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key.as_str() == "escape" {
                    (this.on_close)(window, cx);
                    window.remove_window();
                }
            }))
            .children(titlebar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(space::LOOSE))
                    .p(px(space::SECTION))
                    .child(summary.flex_none())
                    .child(plugin_panel)
                    .child(host_panel)
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(
                                "DSP is each plug-in's share of the audio deadline. Memory and CPU \
                                 belong to the host process, which several plug-ins can share.",
                            ),
                    ),
            )
    }
}

/// Plug-ins of each kind: built-in, VST3, VST, CLAP.
fn kind_counts(rows: &[PluginProcessRow]) -> (usize, usize, usize, usize) {
    let n = |kind| rows.iter().filter(|row| row.kind == kind).count();
    (
        n(PluginKind::BuiltIn),
        n(PluginKind::Vst3),
        n(PluginKind::Vst2),
        n(PluginKind::Clap),
    )
}

fn panel() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .overflow_hidden()
}

fn stat_tile(label: &str, value: String, accent: Option<gpui::Rgba>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .min_w(px(120.0))
        .px(px(space::LOOSE))
        .py(px(space::BASE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(px(typography::UI_TITLE))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(accent.unwrap_or(Colors::text_primary()))
                .child(value),
        )
}

fn filter_pill(
    id: impl Into<gpui::ElementId>,
    label: &str,
    count: usize,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    let rest = Colors::surface_panel();
    let (bg, text) = if active {
        Colors::latched(rest, Colors::accent_primary())
    } else {
        (rest, Colors::text_secondary())
    };
    let hover = Colors::composite(bg, Colors::state_hover());
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .h(px(22.0))
        .px(px(space::BASE))
        .rounded(px(radius::PILL))
        .bg(bg)
        .border(px(1.0))
        .border_color(if active {
            Colors::with_alpha(Colors::accent_primary(), 0.6)
        } else {
            Colors::border_subtle()
        })
        .text_size(px(typography::UI_XS))
        .text_color(text)
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .child(label.to_string())
        .child(
            div()
                .text_color(Colors::text_muted())
                .child(count.to_string()),
        )
}

fn sort_header(id: &'static str, label: &str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::HAIR))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_muted()
        })
        .hover(|s| s.text_color(Colors::text_primary()))
        .child(label.to_string())
        .when(active, |header| header.child("\u{25BE}"))
}

fn column(label: &str, width: f32) -> impl IntoElement {
    div()
        .w(px(width))
        .flex_none()
        .truncate()
        .child(label.to_string())
}

fn empty_state(text: &str) -> gpui::AnyElement {
    div()
        .px(px(space::BASE))
        .py(px(space::LOOSE))
        .text_color(Colors::text_muted())
        .child(text.to_string())
        .into_any_element()
}

fn plugin_row(
    index: usize,
    row: &PluginProcessRow,
    host: Option<&HostSample>,
    sample_rate: u32,
) -> gpui::AnyElement {
    let dim = row.bypassed;
    let name_color = if dim {
        Colors::text_muted()
    } else {
        Colors::text_primary()
    };
    let shared = host.map_or(0, HostSample::instances);
    let pid = host.map(|host| host.pid).or(row.host_pid);
    let memory = match (pid, host.and_then(|host| host.memory_bytes)) {
        (None, _) => "in Studio".to_string(),
        (Some(_), Some(bytes)) if shared > 1 => format!("{} ·{shared}", bytes_label(bytes)),
        (Some(_), Some(bytes)) => bytes_label(bytes),
        (Some(_), None) => "—".to_string(),
    };
    let cpu = if pid.is_some() {
        percent(host.and_then(|host| host.cpu))
    } else {
        "—".to_string()
    };
    let process = match pid {
        Some(pid) => format!("Host {pid}"),
        None => "Studio".to_string(),
    };
    let latency = if row.latency_samples == 0 {
        "0".to_string()
    } else if sample_rate > 0 {
        format!(
            "{:.1} ms",
            row.latency_samples as f64 * 1000.0 / sample_rate as f64
        )
    } else {
        format!("{} smp", row.latency_samples)
    };
    let hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());

    div()
        .id(("process-row", index))
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_H))
        .flex_none()
        .px(px(space::BASE))
        .gap(px(space::BASE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .hover(move |s| s.bg(hover))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                // The name keeps its room; the vendor gives way first.
                .child(
                    div()
                        .flex_none()
                        .max_w(px(170.0))
                        .truncate()
                        .text_color(name_color)
                        .child(row.plugin_name.clone()),
                )
                .children(row.vendor.as_ref().filter(|v| !v.is_empty()).map(|vendor| {
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(vendor.clone())
                }))
                .when(row.bypassed, |cell| {
                    cell.child(tag("Bypassed", Colors::text_muted()))
                })
                .children(
                    row.problem
                        .as_ref()
                        .map(|problem| tag(problem, Colors::status_warning())),
                ),
        )
        .child(
            div()
                .w(px(COL_TRACK))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .child(
                    div()
                        .flex_none()
                        .w(px(3.0))
                        .h(px(14.0))
                        .rounded(px(radius::MICRO))
                        .bg(row.track_color),
                )
                .child(
                    div()
                        .truncate()
                        .text_color(Colors::text_secondary())
                        .child(row.track_name.clone()),
                ),
        )
        .child(
            div()
                .w(px(COL_KIND))
                .flex_none()
                .flex()
                .child(kind_badge(row.kind)),
        )
        .child(
            div()
                .w(px(COL_PROCESS))
                .flex_none()
                .truncate()
                .text_color(Colors::text_secondary())
                .child(process),
        )
        .child(dsp_cell(row.dsp_load))
        .child(
            div()
                .w(px(COL_LATENCY))
                .flex_none()
                .text_color(Colors::text_secondary())
                .child(latency),
        )
        .child(
            div()
                .w(px(COL_MEMORY))
                .flex_none()
                .truncate()
                .text_color(Colors::text_secondary())
                .child(memory),
        )
        .child(
            div()
                .w(px(COL_CPU))
                .flex_none()
                .text_color(Colors::text_secondary())
                .child(cpu),
        )
        .into_any_element()
}

/// The DSP share as a bar and a number.
fn dsp_cell(load: Option<f32>) -> impl IntoElement {
    let fraction = load.unwrap_or(0.0).clamp(0.0, 1.0);
    div()
        .w(px(COL_DSP))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SNUG))
        .child(
            div()
                .w(px(72.0))
                .h(px(6.0))
                .flex_none()
                .rounded(px(radius::PILL))
                .bg(Colors::meter_bg())
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction))
                        .rounded(px(radius::PILL))
                        .bg(load_color(load.unwrap_or(0.0))),
                ),
        )
        .child(
            div()
                .text_color(Colors::text_secondary())
                .child(percent(load)),
        )
}

fn kind_badge(kind: PluginKind) -> impl IntoElement {
    let hue = match kind {
        PluginKind::BuiltIn => Colors::accent_primary(),
        _ => Colors::text_secondary(),
    };
    div()
        .flex_none()
        .px(px(space::TIGHT))
        .rounded(px(radius::MICRO))
        .border(px(1.0))
        .border_color(Colors::with_alpha(hue, 0.45))
        .bg(Colors::with_alpha(hue, 0.10))
        .text_size(px(typography::UI_XS))
        .text_color(hue)
        .child(kind.label())
}

fn tag(text: &str, hue: gpui::Rgba) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(space::TIGHT))
        .rounded(px(radius::MICRO))
        .bg(Colors::with_alpha(hue, 0.14))
        .text_size(px(typography::UI_XS))
        .text_color(hue)
        .child(text.to_string())
}

fn host_row(host: &HostSample, rows: &[PluginProcessRow]) -> gpui::AnyElement {
    // The registry names its plug-ins by insert id; a slot that has not
    // reported its host's pid is still found that way.
    let names: Vec<&str> = rows
        .iter()
        .filter(|row| host.instance_ids.contains(&row.insert_id) || row.host_pid == Some(host.pid))
        .map(|row| row.plugin_name.as_str())
        .collect();
    let carried = match (names.is_empty(), host.instances()) {
        (false, _) => names.join(", "),
        (true, 1) => "1 plug-in".to_string(),
        (true, n) => format!("{n} plug-ins"),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_H))
        .flex_none()
        .px(px(space::BASE))
        .gap(px(space::BASE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .w(px(COL_PROCESS))
                .flex_none()
                .text_color(Colors::text_primary())
                .child(format!("Host {}", host.pid)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(Colors::text_secondary())
                .child(carried),
        )
        .child(
            div()
                .w(px(COL_MEMORY))
                .flex_none()
                .text_color(Colors::text_secondary())
                .child(host.memory_bytes.map_or("—".to_string(), bytes_label)),
        )
        .child(
            div()
                .w(px(COL_CPU))
                .flex_none()
                .text_color(Colors::text_secondary())
                .child(percent(host.cpu)),
        )
        .into_any_element()
}

/// A share as a percentage, `—` when unmeasured. Over 100 % stays visible: a
/// plug-in past the deadline is the dropout that already happened.
fn percent(share: Option<f32>) -> String {
    match share {
        None => "—".to_string(),
        Some(share) if share < 0.001 => "0 %".to_string(),
        Some(share) if share < 0.1 => format!("{:.1} %", share * 100.0),
        Some(share) => format!("{:.0} %", share * 100.0),
    }
}

/// The Performance Monitor's thresholds: amber at 70 %, red at 90 %.
fn load_color(fraction: f32) -> gpui::Rgba {
    if fraction >= 0.9 {
        Colors::status_error()
    } else if fraction >= 0.7 {
        Colors::status_warning()
    } else {
        Colors::status_success()
    }
}

/// Open the Process Explorer. `reading` is drawn until the refresh task takes
/// its own (see the Performance Monitor for why it is passed in).
pub fn open_process_explorer_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    reading: ProcessExplorerReading,
    read: ProcessExplorerReader,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + 'static>,
    cx: &mut App,
) -> Result<WindowHandle<ProcessExplorerWindow>, String> {
    let mut options =
        crate::platform_chrome::native_tool_window_options_partial("Process Explorer");
    if let Some(titlebar) = options.titlebar.as_mut() {
        titlebar.title = Some("Process Explorer".into());
    }
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        owner_bounds,
        size(px(PROCESS_EXPLORER_WIDTH), px(PROCESS_EXPLORER_HEIGHT)),
        cx,
    )));
    options.window_background = gpui::WindowBackgroundAppearance::Opaque;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_min_size = Some(size(px(MIN_WIDTH), px(MIN_HEIGHT)));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |window, cx| {
        let closing = on_close.clone();
        window.on_window_should_close(cx, move |window, cx| {
            closing(window, cx);
            true
        });
        cx.new(|cx| ProcessExplorerWindow::new(reading, read, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

impl ProcessExplorerWindow {
    /// A window drawing `reading` with no refresh task, for preview renders.
    pub fn preview(reading: ProcessExplorerReading, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            reading,
            hosts: HashMap::new(),
            cpu_marks: HashMap::new(),
            filter: KindFilter::All,
            sort: SortKey::Dsp,
            on_close: Arc::new(|_, _| {}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmeasured_is_a_dash_and_overload_stays_visible() {
        assert_eq!(percent(None), "—");
        assert_eq!(percent(Some(0.0)), "0 %");
        assert_eq!(percent(Some(0.034)), "3.4 %");
        assert_eq!(percent(Some(1.4)), "140 %");
    }

    #[test]
    fn kinds_are_counted_for_the_filter() {
        let row = |kind| PluginProcessRow {
            insert_id: String::new(),
            plugin_name: String::new(),
            vendor: None,
            track_name: String::new(),
            track_color: gpui::Rgba::default(),
            kind,
            host_pid: None,
            dsp_load: None,
            latency_samples: 0,
            bypassed: false,
            problem: None,
        };
        let rows = vec![
            row(PluginKind::BuiltIn),
            row(PluginKind::Vst3),
            row(PluginKind::Vst3),
            row(PluginKind::Clap),
        ];
        assert_eq!(kind_counts(&rows), (1, 2, 0, 1));
    }
}
