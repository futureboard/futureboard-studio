//! Performance Monitor — what the session and the machine are costing, and the
//! one control that recovers an engine that has stopped coping.
//!
//! Four readings, in the order a dropout is diagnosed:
//!
//! ```txt
//! Audio Engine   device, buffer, round-trip and PDC latency, callback budget
//! Session        what Studio and its plug-in hosts cost
//! Processor      every logical core, with kernel time split out
//! Memory & Disk  physical RAM, and every local volume's space and traffic
//! ```
//!
//! The split matters more than the numbers. "Studio is at 40%" explains
//! nothing on its own — a take glitches because *one* core is pinned, or
//! because the callback is overrunning its budget, or because the drive the
//! session streams from is saturated, and those are three different fixes. Each
//! is a separate row here rather than folded into one meter.
//!
//! Every figure that cannot be read is drawn as `—`. A number is a measurement
//! or it is absent; it is never a zero standing in for "did not ask".

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBounds,
    WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::title_bar::external_window_titlebar;
use crate::system_load::{DriveLoad, SystemLoad};
use crate::theme::{self, radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const PERFORMANCE_WINDOW_WIDTH: f32 = 560.0;
pub const PERFORMANCE_WINDOW_HEIGHT: f32 = 720.0;
const PERFORMANCE_WINDOW_MIN_WIDTH: f32 = 420.0;
const PERFORMANCE_WINDOW_MIN_HEIGHT: f32 = 440.0;

/// Repaint cadence.
///
/// The underlying samplers run on their own one-second tick, so anything faster
/// than this would redraw the same numbers. Twice a second is fast enough that
/// a spike is visible while the user is still looking at it.
const REFRESH: Duration = Duration::from_millis(500);

/// What the audio engine reports about itself, read by the owner and handed
/// over whole so this window never touches the engine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioEngineReading {
    /// False when no engine is open — every other field is then meaningless.
    pub present: bool,
    pub running: bool,
    pub device_state: String,
    pub backend_name: String,
    pub output_device: Option<String>,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub output_latency_ms: f64,
    pub input_latency_ms: f64,
    pub round_trip_latency_ms: f64,
    /// Longest plug-in path to master — the delay compensation actually applied.
    pub pdc_samples: u32,
    pub pdc_ms: f64,
    /// Plug-in latency on the master chain alone.
    pub master_latency_samples: u32,
    pub pdc_enabled: bool,
    pub callback_last_us: u32,
    pub callback_max_us: u32,
    pub callback_deadline_us: u32,
    pub glitch_count: u64,
    pub dropout_count: u64,
    pub dropout_last_reason: String,
    pub dropout_protection_mode: String,
    pub active_voices: u32,
    pub last_error: Option<String>,
}

impl AudioEngineReading {
    /// Share of the per-block deadline the last callback used, 0..1+. `None`
    /// when there is no deadline to measure against (no open stream).
    pub fn callback_load(&self) -> Option<f32> {
        (self.callback_deadline_us > 0)
            .then(|| self.callback_last_us as f32 / self.callback_deadline_us as f32)
    }

    /// The worst callback since the stream opened, as a share of the deadline.
    /// This is the number that says whether the buffer size is survivable —
    /// the average never is.
    pub fn peak_callback_load(&self) -> Option<f32> {
        (self.callback_deadline_us > 0)
            .then(|| self.callback_max_us as f32 / self.callback_deadline_us as f32)
    }
}

/// Takes a fresh reading from the audio engine.
///
/// **Called only from the refresh task, never from `render`.** The engine lives
/// on the Studio entity, and this window is opened from inside a Studio update
/// — reading that entity while it is leased is a double-lease panic, and with
/// `panic = "abort"` in the release profile that is the whole application. The
/// refresh task runs outside any lease, which is what makes the read safe;
/// `render` uses the snapshot it left behind.
pub type AudioEngineReader = Arc<dyn Fn(&mut App) -> AudioEngineReading + Send + Sync>;

/// Tears the audio device down and brings it back up, rebuilding the project
/// runtime against it.
pub type RestartEngineCb = Arc<dyn Fn(&mut App) + Send + Sync>;

pub struct PerformanceWindow {
    focus_handle: FocusHandle,
    /// The last reading the refresh task took. `render` draws this and nothing
    /// else — see [`AudioEngineReader`].
    engine: AudioEngineReading,
    on_restart: RestartEngineCb,
    /// Set the moment Restart is pressed and cleared when the engine reports a
    /// running stream again, so the button cannot be pressed twice into the
    /// same teardown.
    restarting: bool,
}

impl PerformanceWindow {
    fn new(
        engine: AudioEngineReading,
        read_engine: AudioEngineReader,
        on_restart: RestartEngineCb,
        cx: &mut Context<Self>,
    ) -> Self {
        // The machine's load and the engine's counters both move whether or not
        // anything in Studio is being edited, so this window drives its own
        // refresh. Taking the engine reading here rather than in `render` is
        // what keeps it off a leased entity.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH).await;
                // Two steps, deliberately not nested: take the reading with nothing
                // leased, then apply it. Reading the Studio from inside this
                // window's own update would be a nested entity update, which is the
                // shape this project avoids everywhere else for the same reason.
                let reading = cx.update(|cx| read_engine(cx));
                let updated = this.update(cx, |view, cx| {
                    view.engine = reading;
                    if view.restarting && view.engine.running {
                        view.restarting = false;
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            engine,
            on_restart,
            restarting: false,
        }
    }
}

impl Render for PerformanceWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let engine = self.engine.clone();
        let system = crate::system_load::system_load();
        let session = crate::perf::resource_usage();
        let restart_enabled = engine.present && !self.restarting;
        let on_restart = self.on_restart.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .text_size(px(typography::UI_SM))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_this, event: &KeyDownEvent, window, _cx| {
                if event.keystroke.key.as_str() == "escape" {
                    window.remove_window();
                }
            }))
            .child(external_window_titlebar(
                "Performance Monitor",
                "performance-window-close",
                move |window, _cx| window.remove_window(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id("performance-body")
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(space::LOOSE))
                    .p(px(space::SECTION))
                    .child(audio_engine_section(
                        &engine,
                        restart_enabled,
                        self.restarting,
                        cx.listener(move |this, _event: &gpui::ClickEvent, _window, cx| {
                            this.restarting = true;
                            on_restart(cx);
                            cx.notify();
                        }),
                    ))
                    .child(session_section(&session))
                    .child(processor_section(&system))
                    .child(memory_section(&system))
                    .child(disk_section(&system)),
            )
    }
}

// ── Sections ──────────────────────────────────────────────────────────────────

fn audio_engine_section(
    engine: &AudioEngineReading,
    restart_enabled: bool,
    restarting: bool,
    on_restart: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let restart_label = if restarting {
        "Restarting…"
    } else {
        "Restart Audio Engine"
    };

    let body = if !engine.present {
        vec![note_row("No audio engine is open.")]
    } else {
        let device = engine
            .output_device
            .clone()
            .unwrap_or_else(|| "system default".to_string());
        let mut rows = vec![
            reading_row("Status", &engine.device_state),
            reading_row("Backend", &engine.backend_name),
            reading_row("Output", &device),
            reading_row(
                "Stream",
                &format!(
                    "{} Hz · {} frames",
                    engine.sample_rate.max(0),
                    engine.buffer_frames
                ),
            ),
            reading_row(
                "Latency",
                &format!(
                    "out {:.2} ms · in {:.2} ms · round trip {:.2} ms",
                    engine.output_latency_ms, engine.input_latency_ms, engine.round_trip_latency_ms
                ),
            ),
            reading_row(
                "PDC",
                &if engine.pdc_enabled {
                    format!(
                        "{} samples · {:.2} ms (master {} samples)",
                        engine.pdc_samples, engine.pdc_ms, engine.master_latency_samples
                    )
                } else {
                    format!("off — {} samples would be compensated", engine.pdc_samples)
                },
            ),
        ];
        // The callback budget, as a share of the block deadline. A peak over
        // 100% is a dropout that already happened, so it is drawn as a meter
        // rather than buried in a number.
        if let (Some(last), Some(peak)) = (engine.callback_load(), engine.peak_callback_load()) {
            rows.push(meter_row(
                "Callback",
                last,
                &format!(
                    "{:.0}% now · {:.0}% peak ({} µs of {} µs)",
                    last * 100.0,
                    peak * 100.0,
                    engine.callback_last_us,
                    engine.callback_deadline_us
                ),
                load_color(peak),
            ));
        }
        rows.push(reading_row(
            "Dropouts",
            &format!(
                "{} glitches · {} at-risk blocks · protection {}",
                engine.glitch_count, engine.dropout_count, engine.dropout_protection_mode
            ),
        ));
        if !engine.dropout_last_reason.is_empty() {
            rows.push(reading_row("Last reason", &engine.dropout_last_reason));
        }
        rows.push(reading_row("Voices", &engine.active_voices.to_string()));
        if let Some(error) = engine.last_error.as_ref() {
            rows.push(alert_row(error));
        }
        rows
    };

    section(
        "Audio Engine",
        Some(
            div()
                .flex_none()
                .child(fb_button(
                    "performance-restart-engine",
                    restart_label,
                    FbButtonKind::Default,
                    restart_enabled,
                    on_restart,
                ))
                .into_any_element(),
        ),
        body,
    )
}

fn session_section(usage: &crate::perf::ResourceUsage) -> impl IntoElement {
    if !usage.studio_known {
        return section(
            "Session",
            None,
            vec![note_row(
                "Per-process counters are unavailable on this platform.",
            )],
        );
    }
    let total = usage.total();
    let hosts = match usage.plugin_host_count {
        0 => "no plug-in hosts".to_string(),
        1 => "1 plug-in host".to_string(),
        n => format!("{n} plug-in hosts"),
    };
    section(
        "Session",
        None,
        vec![
            reading_row(
                "CPU",
                &format!(
                    "{:.1}% total · {:.1}% app · {:.1}% hosts",
                    total.cpu_percent, usage.studio.cpu_percent, usage.plugin_hosts.cpu_percent
                ),
            ),
            reading_row(
                "Memory",
                &format!(
                    "{} total · {} app · {} hosts",
                    bytes_label(total.memory_bytes),
                    bytes_label(usage.studio.memory_bytes),
                    bytes_label(usage.plugin_hosts.memory_bytes)
                ),
            ),
            reading_row("Disk", &rate_label(Some(total.disk_bytes_per_sec))),
            reading_row("Hosts", &hosts),
        ],
    )
}

fn processor_section(system: &SystemLoad) -> impl IntoElement {
    if !system.known || system.cores.is_empty() {
        return section(
            "Processor",
            None,
            vec![note_row("Per-core load is unavailable on this platform.")],
        );
    }
    let mut rows = vec![reading_row(
        "Load",
        &format!(
            "{:.0}% average · {:.0}% busiest core · {} cores",
            system.cpu_percent(),
            system.peak_core_percent(),
            system.cores.len()
        ),
    )];
    for (index, core) in system.cores.iter().enumerate() {
        let fraction = core.busy_percent / 100.0;
        rows.push(meter_row(
            &format!("Core {index}"),
            fraction,
            &format!(
                "{:.0}%  ({:.0}% kernel)",
                core.busy_percent, core.kernel_percent
            ),
            load_color(fraction),
        ));
    }
    section("Processor", None, rows)
}

fn memory_section(system: &SystemLoad) -> impl IntoElement {
    if !system.known || system.memory.total_bytes == 0 {
        return section(
            "Memory",
            None,
            vec![note_row("Physical memory is unavailable on this platform.")],
        );
    }
    let memory = system.memory;
    let fraction = memory.used_fraction();
    section(
        "Memory",
        None,
        vec![meter_row(
            "Physical",
            fraction,
            &format!(
                "{} of {} used · {} available",
                bytes_label(memory.used_bytes()),
                bytes_label(memory.total_bytes),
                bytes_label(memory.available_bytes)
            ),
            load_color(fraction),
        )],
    )
}

fn disk_section(system: &SystemLoad) -> impl IntoElement {
    if !system.known {
        return section(
            "Disks",
            None,
            vec![note_row(
                "Volume readings are unavailable on this platform.",
            )],
        );
    }
    if system.drives.is_empty() {
        return section("Disks", None, vec![note_row("No local volumes found.")]);
    }
    let rows = system.drives.iter().map(drive_row).collect();
    section("Disks", None, rows)
}

fn drive_row(drive: &DriveLoad) -> gpui::AnyElement {
    let name = if drive.label.is_empty() {
        drive.root.clone()
    } else {
        format!("{} ({})", drive.root, drive.label)
    };
    let filesystem = if drive.filesystem.is_empty() {
        String::new()
    } else {
        format!(" · {}", drive.filesystem)
    };
    let fraction = drive.used_fraction();
    let space = if drive.total_bytes == 0 {
        "capacity unavailable".to_string()
    } else {
        format!(
            "{} free of {}{filesystem}",
            bytes_label(drive.free_bytes),
            bytes_label(drive.total_bytes)
        )
    };
    let traffic = format!(
        "read {} · write {}{}",
        rate_label(drive.read_bytes_per_sec),
        rate_label(drive.write_bytes_per_sec),
        drive
            .busy_fraction
            .map(|busy| format!(" · {:.0}% busy", busy * 100.0))
            .unwrap_or_default()
    );

    div()
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .child(meter_row(&name, fraction, &space, load_color(fraction)))
        .child(
            div()
                .pl(px(140.0))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(traffic),
        )
        .into_any_element()
}

// ── Building blocks ───────────────────────────────────────────────────────────

fn section(
    title: &str,
    action: Option<gpui::AnyElement>,
    rows: Vec<gpui::AnyElement>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(space::BASE))
                .child(
                    div()
                        .text_size(px(typography::UI_TITLE))
                        .text_color(Colors::text_primary())
                        .child(title.to_string()),
                )
                .children(action),
        )
        .children(rows)
}

/// A label and its value. The label column is fixed so every value in the
/// window starts on the same x — a table of readings is read down, not across.
fn reading_row(label: &str, value: &str) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(space::BASE))
        .child(
            div()
                .w(px(132.0))
                .flex_none()
                .text_color(Colors::text_muted())
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(Colors::text_secondary())
                .child(value.to_string()),
        )
        .into_any_element()
}

/// A reading with a bar behind it. Used wherever the value has a ceiling worth
/// seeing the distance to — a core, a volume, the callback budget.
fn meter_row(label: &str, fraction: f32, value: &str, color: gpui::Rgba) -> gpui::AnyElement {
    let fraction = fraction.clamp(0.0, 1.0);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(
            div()
                .w(px(132.0))
                .flex_none()
                .text_color(Colors::text_muted())
                .child(label.to_string()),
        )
        .child(
            div()
                .w(px(96.0))
                .h(px(8.0))
                .flex_none()
                .rounded(px(radius::PILL))
                .bg(Colors::meter_bg())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction))
                        .rounded(px(radius::PILL))
                        .bg(color),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(Colors::text_secondary())
                .child(value.to_string()),
        )
        .into_any_element()
}

fn note_row(text: &str) -> gpui::AnyElement {
    div()
        .text_color(Colors::text_muted())
        .child(text.to_string())
        .into_any_element()
}

fn alert_row(text: &str) -> gpui::AnyElement {
    div()
        .px(px(space::BASE))
        .py(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .bg(Colors::with_alpha(Colors::status_error(), 0.12))
        .text_color(Colors::status_error())
        .child(text.to_string())
        .into_any_element()
}

/// Green until it matters, amber where it starts to, red where it already has.
///
/// The thresholds are deliberately low for a DAW: a core at 75% is not
/// comfortable when one block's overrun is an audible click.
fn load_color(fraction: f32) -> gpui::Rgba {
    if fraction >= 0.9 {
        Colors::status_error()
    } else if fraction >= 0.7 {
        Colors::status_warning()
    } else {
        Colors::status_success()
    }
}

/// Bytes at a scale a person reads, never more precision than the number has.
pub fn bytes_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;
    let value = bytes as f64;
    if value >= TB {
        format!("{:.2} TB", value / TB)
    } else if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.0} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// A throughput. `None` is "not measured" and prints as `—`; a measured zero is
/// "idle", and the two must not look the same.
pub fn rate_label(bytes_per_sec: Option<f64>) -> String {
    let Some(rate) = bytes_per_sec else {
        return "—".to_string();
    };
    if rate < 1024.0 {
        return "idle".to_string();
    }
    format!("{}/s", bytes_label(rate as u64))
}

// ── Window ────────────────────────────────────────────────────────────────────

/// Open the Performance Monitor.
///
/// `engine` is the reading to draw until the refresh task takes its own. It is
/// passed in rather than read here because this is called from inside a Studio
/// update: the window's first frame can land before that update returns, and
/// reading the Studio entity then is a double-lease panic.
pub fn open_performance_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    engine: AudioEngineReading,
    read_engine: AudioEngineReader,
    on_restart: RestartEngineCb,
    cx: &mut App,
) -> Result<WindowHandle<PerformanceWindow>, String> {
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        owner_bounds,
        size(px(PERFORMANCE_WINDOW_WIDTH), px(PERFORMANCE_WINDOW_HEIGHT)),
        cx,
    )));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_min_size = Some(size(
        px(PERFORMANCE_WINDOW_MIN_WIDTH),
        px(PERFORMANCE_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| PerformanceWindow::new(engine, read_engine, on_restart, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rate that was never measured and a disk that is genuinely idle are
    /// different facts. Printing both as "idle" would tell the user their
    /// streaming drive is quiet when nothing ever asked it.
    #[test]
    fn an_unmeasured_rate_is_not_an_idle_one() {
        assert_eq!(rate_label(None), "—");
        assert_eq!(rate_label(Some(0.0)), "idle");
        assert_eq!(rate_label(Some(5.0 * 1024.0 * 1024.0)), "5 MB/s");
    }

    #[test]
    fn byte_labels_scale_without_inventing_precision() {
        assert_eq!(bytes_label(512), "512 B");
        assert_eq!(bytes_label(2 * 1024), "2 KB");
        assert_eq!(bytes_label(3 * 1024 * 1024), "3 MB");
        assert_eq!(bytes_label(4 * 1024 * 1024 * 1024), "4.0 GB");
    }

    /// The callback budget is the one audio reading that predicts a click, and
    /// it is a ratio — without a deadline there is nothing to compare against
    /// and the answer is "unknown", not "fine".
    #[test]
    fn callback_load_is_absent_without_a_deadline() {
        let mut engine = AudioEngineReading {
            present: true,
            callback_last_us: 900,
            callback_max_us: 1_400,
            callback_deadline_us: 0,
            ..Default::default()
        };
        assert!(engine.callback_load().is_none());
        assert!(engine.peak_callback_load().is_none());

        engine.callback_deadline_us = 1_000;
        assert!((engine.callback_load().unwrap() - 0.9).abs() < 1.0e-6);
        // A peak past the deadline is a dropout that already happened, and it
        // must be reported as over 100% rather than clamped out of sight.
        assert!(engine.peak_callback_load().unwrap() > 1.0);
    }

    #[test]
    fn the_load_colour_escalates_at_the_thresholds() {
        assert_eq!(load_color(0.5), Colors::status_success());
        assert_eq!(load_color(0.75), Colors::status_warning());
        assert_eq!(load_color(0.95), Colors::status_error());
    }
}
