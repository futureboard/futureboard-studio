use gpui::{div, px, InteractiveElement, IntoElement, ParentElement, Styled};

use crate::theme::Colors;

#[derive(Debug, Clone)]
pub struct PerformanceOverlaySnapshot {
    pub renderer: String,
    pub display_sync: String,
    pub fps: f32,
    pub frame_ms: f32,
    pub peak_ms: f32,
    pub has_sample: bool,
    pub repaint_reason: String,
    pub audio: String,
    /// Most expensive instrumented scopes this window, worst first. Answers
    /// "where is the frame going" directly on screen, instead of requiring an
    /// env var and a log file.
    pub top_scopes: Vec<crate::perf::ScopeSample>,
    /// Instrumented CPU per frame from the last completed perf window.
    pub ui_cpu_ms: f32,
    /// File timestamp of the running executable, so the panel proves which
    /// build produced the numbers next to it.
    pub build_stamp: String,
}

pub fn performance_overlay(snapshot: &PerformanceOverlaySnapshot) -> impl IntoElement {
    let fps = if snapshot.has_sample {
        format!("{:.1}", snapshot.fps)
    } else {
        "—".to_string()
    };
    let frame = if snapshot.has_sample {
        format!("{:.2} ms", snapshot.frame_ms)
    } else {
        "—".to_string()
    };
    let peak = if snapshot.has_sample {
        format!("{:.2} ms", snapshot.peak_ms)
    } else {
        "—".to_string()
    };

    div()
        .absolute()
        .top(px(36.0))
        .right(px(12.0))
        .w(px(280.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(10.0))
        .rounded(px(crate::theme::radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .shadow_lg()
        .occlude()
        .child(overlay_title("Profiler"))
        .children([
            overlay_line("Audio", &snapshot.audio),
            overlay_line("Renderer", &snapshot.renderer),
            overlay_line("Display Sync", &snapshot.display_sync),
            overlay_line("FPS", &fps),
            overlay_line("Frame", &frame),
            overlay_line("Peak", &peak),
            overlay_line("Repaint", &snapshot.repaint_reason),
            overlay_line("Build", &snapshot.build_stamp),
        ])
        .children(frame_accounting_rows(snapshot.ui_cpu_ms, snapshot.frame_ms))
        .children(worst_frame_rows())
        .children(resource_rows())
        .children(hot_scope_rows(&snapshot.top_scopes))
}

/// The worst single frame of the last second, and what ran inside it.
///
/// Every other number on this panel is a per-second average, which is exactly
/// the shape that hides a stutter: a run of 6 ms frames with one 90 ms frame in
/// it averages to something that looks healthy, and the 90 ms frame is the only
/// one anybody felt. This names that frame.
fn worst_frame_rows() -> Vec<gpui::AnyElement> {
    let Some(worst) = crate::perf::worst_frame() else {
        return vec![
            section_label("Worst frame").into_any_element(),
            overlay_line("Gap", "none over 33 ms").into_any_element(),
        ];
    };
    let mut rows = vec![
        section_label("Worst frame").into_any_element(),
        overlay_scope_row(
            "Gap",
            &format!("{:.1} ms  ({})", worst.gap_ms, worst.reason),
            100.0,
        )
        .into_any_element(),
    ];
    if worst.scopes.is_empty() {
        // A long gap with nothing instrumented inside it is itself the finding:
        // the time went somewhere this build does not measure — a blocking
        // call, another window's draw, or the frame simply not being scheduled.
        rows.push(overlay_line("  in frame", "no instrumented work").into_any_element());
    }
    let share = |ms: f32| {
        if worst.gap_ms > 0.0 {
            100.0 * ms / worst.gap_ms
        } else {
            0.0
        }
    };
    for scope in &worst.scopes {
        rows.push(
            overlay_scope_row(
                &format!("  {}", scope.name),
                &format!("{:.2} ms  {:.0}%", scope.total_ms, share(scope.total_ms)),
                share(scope.total_ms),
            )
            .into_any_element(),
        );
    }
    rows
}

/// What the session is costing the machine, split into the app and the plug-ins
/// it is hosting.
///
/// Both halves, always. Studio runs plug-ins out of process, so its own numbers
/// are not the session's: a sampler holding two gigabytes and pinning a core
/// shows up nowhere in the Studio process, and "the DAW is using 400 MB" would
/// be true and useless. Read on one shared one-second tick — see
/// [`crate::perf::resource_usage`] — so the three figures always describe the
/// same second.
fn resource_rows() -> Vec<gpui::AnyElement> {
    let usage = crate::perf::resource_usage();
    if !usage.studio_known {
        return Vec::new();
    }
    let total = usage.total();
    let mb = |bytes: u64| format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0));
    let rate = |bytes_per_sec: f64| {
        if bytes_per_sec >= 1024.0 * 1024.0 {
            format!("{:.1} MB/s", bytes_per_sec / (1024.0 * 1024.0))
        } else if bytes_per_sec >= 1024.0 {
            format!("{:.0} KB/s", bytes_per_sec / 1024.0)
        } else {
            "idle".to_string()
        }
    };
    let hosts = match usage.plugin_host_count {
        0 => "no plug-in hosts".to_string(),
        1 => "1 plug-in host".to_string(),
        n => format!("{n} plug-in hosts"),
    };
    vec![
        section_label("Session load").into_any_element(),
        overlay_line("CPU", &format!("{:.1}%", total.cpu_percent)).into_any_element(),
        overlay_line(
            "  app / hosts",
            &format!(
                "{:.1}% / {:.1}%",
                usage.studio.cpu_percent, usage.plugin_hosts.cpu_percent
            ),
        )
        .into_any_element(),
        overlay_line("RAM", &mb(total.memory_bytes)).into_any_element(),
        overlay_line(
            "  app / hosts",
            &format!(
                "{} / {}",
                mb(usage.studio.memory_bytes),
                mb(usage.plugin_hosts.memory_bytes)
            ),
        )
        .into_any_element(),
        overlay_line("Disk", &rate(total.disk_bytes_per_sec)).into_any_element(),
        overlay_line(
            "  app / hosts",
            &format!(
                "{} / {}",
                rate(usage.studio.disk_bytes_per_sec),
                rate(usage.plugin_hosts.disk_bytes_per_sec)
            ),
        )
        .into_any_element(),
        overlay_line("Hosts", &hosts).into_any_element(),
    ]
}

/// Split the frame into "code we measured" and "everything else".
///
/// Without this the scope list is easy to misread: four scopes at 0.1 ms next
/// to a 40 ms frame looks like the profiler is broken, when it is actually the
/// finding — the thread is stalled outside instrumented code.
///
/// `frame_ms` comes from the overlay's own frame diagnostics, never from the
/// perf collector: this block must appear even when the collector has thin
/// data, because that is exactly the case it explains.
fn frame_accounting_rows(cpu_ms: f32, frame_ms: f32) -> Vec<gpui::AnyElement> {
    let profile = gpui::frame_profile::frame_profile();
    let pct = |ms: f32| {
        if frame_ms > 0.0 {
            100.0 * ms / frame_ms
        } else {
            0.0
        }
    };
    let draw_ms = profile.draw_ms();
    let present_ms = profile.present_ms();
    let unaccounted = (frame_ms - cpu_ms - draw_ms - present_ms).max(0.0);

    vec![
        section_label("Frame breakdown"),
        overlay_scope_row(
            "UI CPU",
            &format!("{cpu_ms:.2} ms  {:.0}%", pct(cpu_ms)),
            pct(cpu_ms),
        )
        .into_any_element(),
        // `draw` covers building, laying out, and painting the element tree;
        // `present` is handing the finished scene to the GPU.
        overlay_scope_row(
            "GPUI draw",
            &format!("{draw_ms:.2} ms  {:.0}%", pct(draw_ms)),
            pct(draw_ms),
        )
        .into_any_element(),
        overlay_scope_row(
            "GPUI present",
            &format!("{present_ms:.2} ms  {:.0}%", pct(present_ms)),
            pct(present_ms),
        )
        .into_any_element(),
        overlay_scope_row(
            "Unaccounted",
            &format!("{unaccounted:.2} ms  {:.0}%", pct(unaccounted)),
            pct(unaccounted),
        )
        .into_any_element(),
        // The draw split. `prepaint` builds the element tree and lays it out
        // (the app's own render functions run inside it); `paint` walks the
        // laid-out tree emitting primitives; `a11y` rebuilds the accessibility
        // tree when a client has switched it on.
        section_label("Draw split"),
        overlay_scope_row(
            "prepaint+layout",
            &format!(
                "{:.2} ms  {:.0}%",
                profile.prepaint_ms(),
                pct(profile.prepaint_ms())
            ),
            pct(profile.prepaint_ms()),
        )
        .into_any_element(),
        overlay_scope_row(
            "paint",
            &format!(
                "{:.2} ms  {:.0}%",
                profile.paint_ms(),
                pct(profile.paint_ms())
            ),
            pct(profile.paint_ms()),
        )
        .into_any_element(),
        overlay_scope_row(
            "a11y tree",
            &format!(
                "{:.2} ms  {:.0}%",
                profile.a11y_ms(),
                pct(profile.a11y_ms())
            ),
            pct(profile.a11y_ms()),
        )
        .into_any_element(),
        overlay_scope_row(
            "Text shape",
            &format!(
                "{:.2} ms  {:.0}%  x{}",
                profile.shape_ms(),
                pct(profile.shape_ms()),
                profile.shape_misses
            ),
            pct(profile.shape_ms()),
        )
        .into_any_element(),
        // Inside prepaint: how much is the layout solve itself, and how much of
        // that is measure callbacks. Taffy runs several sizing passes, so a
        // measure count far above the node count is the thing to fix.
        overlay_scope_row(
            "layout solve",
            &format!(
                "{:.2} ms  {:.0}%",
                profile.layout_solve_ms(),
                pct(profile.layout_solve_ms())
            ),
            pct(profile.layout_solve_ms()),
        )
        .into_any_element(),
        overlay_scope_row(
            "measure",
            &format!(
                "{:.2} ms  {:.0}%  x{}",
                profile.measure_ms(),
                pct(profile.measure_ms()),
                profile.measure_calls
            ),
            pct(profile.measure_ms()),
        )
        .into_any_element(),
        // Layout nodes, not primitives, is what prepaint cost scales with.
        overlay_line(
            "Nodes / prims",
            &format!("{} / {}", profile.layout_nodes, profile.scene_primitives),
        )
        .into_any_element(),
    ]
}

fn section_label(text: &'static str) -> gpui::AnyElement {
    div()
        .pt(px(6.0))
        .mt(px(4.0))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(10.0))
        .text_color(Colors::text_muted())
        .child(text)
        .into_any_element()
}

/// "Where the frame went" rows. Hidden entirely until the collector has
/// something, so the panel keeps its compact shape on an idle window.
fn hot_scope_rows(scopes: &[crate::perf::ScopeSample]) -> Vec<gpui::AnyElement> {
    if scopes.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(scopes.len() + 1);
    rows.push(
        div()
            .pt(px(6.0))
            .mt(px(4.0))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(10.0))
            .text_color(Colors::text_muted())
            .child("Hot scopes (ms/s)")
            .into_any_element(),
    );
    for scope in scopes {
        rows.push(
            overlay_scope_row(
                scope.name,
                &format!(
                    "{:.1} ms  {:.0}%  x{}",
                    scope.total_ms, scope.percent, scope.count
                ),
                scope.percent,
            )
            .into_any_element(),
        );
    }
    rows
}

fn overlay_scope_row(label: &str, value: &str, percent: f32) -> impl IntoElement {
    // A scope taking most of the window is the answer, so colour it like one.
    let value_color = if percent >= 50.0 {
        Colors::status_error()
    } else if percent >= 25.0 {
        Colors::accent_warning()
    } else {
        Colors::text_secondary()
    };
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .w(px(112.0))
                .text_size(px(10.0))
                .text_color(Colors::text_muted())
                .truncate()
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(10.0))
                .text_color(value_color)
                .child(value.to_string()),
        )
}

fn overlay_title(text: &'static str) -> impl IntoElement {
    div()
        .pb(px(4.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
        .child(text)
}

fn overlay_line(label: &'static str, value: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_start()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .w(px(88.0))
                .text_size(px(10.0))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(10.0))
                .text_color(Colors::text_secondary())
                .child(value.to_string()),
        )
}

#[cfg(test)]
mod tests {
    use super::{frame_accounting_rows, hot_scope_rows};
    use crate::perf::ScopeSample;

    /// The breakdown is the block that explains a profiler which otherwise
    /// looks broken, so it must render unconditionally — including before any
    /// perf window has completed (cpu 0.0) and on a degenerate frame time.
    #[test]
    fn breakdown_always_renders_every_row() {
        // Frame breakdown: label + UI CPU + draw + present + unaccounted.
        // Draw split: label + prepaint + paint + a11y + text + solve + measure
        // + node counts.
        const EXPECTED_ROWS: usize = 13;
        assert_eq!(frame_accounting_rows(0.2, 40.0).len(), EXPECTED_ROWS);
        assert_eq!(frame_accounting_rows(0.0, 0.0).len(), EXPECTED_ROWS);
        // A frame cheaper than the measured CPU (clock jitter) must not
        // produce a negative remainder or panic.
        assert_eq!(frame_accounting_rows(5.0, 1.0).len(), EXPECTED_ROWS);
    }

    #[test]
    fn no_scope_rows_when_the_collector_has_nothing() {
        assert!(hot_scope_rows(&[]).is_empty(), "panel stays compact");
    }

    /// One header row plus one row per scope, so the panel height is
    /// predictable and the rows cannot silently disappear.
    #[test]
    fn scope_rows_are_header_plus_one_per_scope() {
        let scopes = vec![
            ScopeSample {
                name: "poll_native_audio",
                total_ms: 480.0,
                percent: 62.0,
                count: 140,
            },
            ScopeSample {
                name: "Timeline",
                total_ms: 40.0,
                percent: 5.0,
                count: 60,
            },
        ];
        assert_eq!(hot_scope_rows(&scopes).len(), scopes.len() + 1);
    }
}
