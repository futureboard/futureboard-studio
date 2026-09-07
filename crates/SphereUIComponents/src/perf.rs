//! Thread-local UI perf collector.
//!
//! Lets us answer the question "where is the frame time going?" without
//! external profilers. The collector lives in a `thread_local!` so render
//! code can drop `PerfScope::enter("name")` anywhere on the foreground
//! thread without plumbing context. It aggregates by scope name over a
//! 1-second window and dumps once per second to stderr when enabled.
//!
//! Enable with `FUTUREBOARD_UI_PERF=1`, `FUTUREBOARD_UI_PROFILE=1`, or
//! `=verbose` to also dump every repaint instead of every second.
//!
//! Notify diagnostics: `FUTUREBOARD_NOTIFY_DEBUG=1` logs notify reasons
//! at 1 Hz via `record_notify`.
//!
//! Cost when disabled: one thread-local flag check per scope/count call.
//! When enabled: one `Instant::now()` + a small HashMap insert per scope.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Default, Clone, Copy)]
struct ScopeAgg {
    total_ns: u64,
    count: u64,
    max_ns: u64,
}

#[derive(Default)]
struct CounterAgg {
    last: u64,
    max: u64,
}

/// Names of scopes that wrap a single child of `StudioLayout`. Used to
/// compute a "self time" for the root scope by subtracting child sums,
/// so the verdict line attributes time to the right component instead
/// of always blaming `StudioLayout`.
const ROOT_SCOPE: &str = "StudioLayout";
const ROOT_CHILDREN: &[&str] = &[
    "AppChrome",
    "Sidebar",
    "Timeline",
    "Inspector",
    "BottomPanel",
    "StatusBar",
];

/// The single worst frame of a window, and where its time went.
///
/// Per-second averages cannot describe a stutter: a run of 6 ms frames with one
/// 90 ms frame in it averages out to something that looks fine, and the 90 ms
/// frame is the only one the user felt. This keeps that frame — how long the
/// gap was, what asked for it, and which instrumented scopes ran inside it —
/// so the readout names the cost instead of hinting at it.
#[derive(Default, Clone)]
pub struct WorstFrame {
    /// Gap before the frame, in milliseconds.
    pub gap_ms: f32,
    /// The repaint reason recorded for it.
    pub reason: &'static str,
    /// Scopes that ran during it, worst first.
    pub scopes: Vec<ScopeSample>,
}

/// A frame slower than this is a stutter worth attributing.
///
/// Two frames at 60 Hz. Below it a frame is late but not seen; above it the
/// motion visibly breaks, which is the thing being hunted.
const STALL_FRAME_MS: f32 = 33.0;

/// Completed 1-second window, kept so readers get stable per-second figures
/// instead of whatever has accumulated since the last reset.
#[derive(Default, Clone)]
struct WindowSummary {
    scopes: Vec<ScopeSample>,
    /// Instrumented wall time per frame in the window.
    cpu_ms_per_frame: f32,
    /// Measured frame interval in the window.
    frame_ms: f32,
    /// The worst frame of the window, when one crossed [`STALL_FRAME_MS`].
    worst: Option<WorstFrame>,
}

struct Collector {
    enabled: bool,
    /// Whether to also write the once-per-second stderr dump + log file.
    /// Env-driven only: turning the on-screen overlay on collects data but
    /// must not start writing to the terminal behind the user's back.
    dump: bool,
    /// Aggregated time per named scope this window.
    scopes: BTreeMap<&'static str, ScopeAgg>,
    /// Last completed window, for the Profiler overlay.
    last_window: WindowSummary,
    /// Latest-value counters (e.g. visible_browser_rows, grid_lines).
    /// We store the most recent sample plus max-this-window so the log
    /// is meaningful even when the value bounces.
    counters: BTreeMap<&'static str, CounterAgg>,
    /// Per-second frame stats.
    frame_count: u64,
    frame_total_ms: f32,
    frame_min_ms: f32,
    frame_max_ms: f32,
    last_frame: Option<Instant>,
    window_start: Instant,
    last_repaint_reason: &'static str,
    /// Per-reason frame count this window — drives the repaint-source
    /// attribution and the idle-loop detector.
    reason_counts: BTreeMap<&'static str, u64>,
    /// Scope time accumulated since the last frame tick, so a slow frame can be
    /// attributed to what ran inside it. Cleared every frame.
    frame_scopes: BTreeMap<&'static str, u64>,
    /// Worst frame seen this window.
    worst: Option<WorstFrame>,
}

#[derive(Default)]
struct NotifyAgg {
    total: u64,
    reasons: BTreeMap<&'static str, u64>,
    window_start: Option<Instant>,
}

impl NotifyAgg {
    fn enabled() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_NOTIFY_DEBUG").is_some())
    }

    fn record(&mut self, reason: &'static str) {
        if !Self::enabled() {
            return;
        }
        let now = Instant::now();
        if self.window_start.is_none() {
            self.window_start = Some(now);
        }
        self.total = self.total.saturating_add(1);
        *self.reasons.entry(reason).or_insert(0) += 1;
        if self
            .window_start
            .is_some_and(|start| now.duration_since(start) >= Duration::from_secs(1))
        {
            let elapsed = self
                .window_start
                .unwrap()
                .elapsed()
                .as_secs_f32()
                .max(0.001);
            eprintln!("[notify-debug] notify/s={:.0}", self.total as f32 / elapsed);
            let mut reasons: Vec<_> = self.reasons.iter().collect();
            reasons.sort_by(|a, b| b.1.cmp(a.1));
            for (name, count) in reasons {
                eprintln!("[notify-debug]   {}={:.1}/s", name, *count as f32 / elapsed);
            }
            *self = Self::default();
        }
    }
}

impl Collector {
    fn new() -> Self {
        let now = Instant::now();
        // A thread that first touches the collector after the overlay is
        // already open must start collecting too, or its scopes stay invisible.
        let enabled = env_collection_enabled()
            || COLLECT_REQUESTED.load(std::sync::atomic::Ordering::Relaxed);
        Self {
            enabled,
            dump: env_collection_enabled(),
            scopes: BTreeMap::new(),
            last_window: WindowSummary::default(),
            counters: BTreeMap::new(),
            frame_count: 0,
            frame_total_ms: 0.0,
            frame_min_ms: f32::INFINITY,
            frame_max_ms: 0.0,
            last_frame: None,
            window_start: now,
            last_repaint_reason: "init",
            reason_counts: BTreeMap::new(),
            frame_scopes: BTreeMap::new(),
            worst: None,
        }
    }

    fn add_scope(&mut self, name: &'static str, ns: u64) {
        let entry = self.scopes.entry(name).or_default();
        entry.total_ns = entry.total_ns.saturating_add(ns);
        entry.count = entry.count.saturating_add(1);
        if ns > entry.max_ns {
            entry.max_ns = ns;
        }
        // Also against the frame in progress, so a stall can name what ran in
        // it rather than what ran in the second around it.
        let frame = self.frame_scopes.entry(name).or_insert(0);
        *frame = frame.saturating_add(ns);
    }

    fn record_counter(&mut self, name: &'static str, value: u64) {
        let entry = self.counters.entry(name).or_default();
        entry.last = value;
        if value > entry.max {
            entry.max = value;
        }
    }

    fn tick_frame(&mut self, reason: &'static str) -> bool {
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt_ms = now.duration_since(prev).as_secs_f32() * 1000.0;
            // Stall attributor: a long gap with near-zero instrumented CPU means
            // the UI thread was blocked outside the render scopes (sync FS I/O,
            // GPU present, IPC). Log the bounding frame reasons so the next pass
            // can attribute the freeze to a specific interaction.
            if self.enabled && dt_ms > 80.0 && dt_ms < 5000.0 {
                eprintln!(
                    "[ui-stall] {dt_ms:.0}ms gap before frame (this_reason={reason} prev_reason={})",
                    self.last_repaint_reason
                );
            }
            if dt_ms < 1000.0 {
                self.frame_total_ms += dt_ms;
                self.frame_count += 1;
                if dt_ms > self.frame_max_ms {
                    self.frame_max_ms = dt_ms;
                }
                if dt_ms < self.frame_min_ms {
                    self.frame_min_ms = dt_ms;
                }
                // Keep the window's worst frame with what ran inside it. The
                // scopes are the ones recorded since the previous tick, which
                // is exactly the frame this gap measures.
                if dt_ms >= STALL_FRAME_MS
                    && self.worst.as_ref().is_none_or(|worst| dt_ms > worst.gap_ms)
                {
                    let mut scopes: Vec<ScopeSample> = self
                        .frame_scopes
                        .iter()
                        .map(|(name, ns)| ScopeSample {
                            name,
                            total_ms: *ns as f32 / 1_000_000.0,
                            percent: 0.0,
                            count: 0,
                        })
                        .collect();
                    scopes.sort_by(|a, b| {
                        b.total_ms
                            .partial_cmp(&a.total_ms)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    scopes.truncate(4);
                    self.worst = Some(WorstFrame {
                        gap_ms: dt_ms,
                        reason: self.last_repaint_reason,
                        scopes,
                    });
                }
            }
        }
        self.frame_scopes.clear();
        self.last_frame = Some(now);
        self.last_repaint_reason = reason;
        *self.reason_counts.entry(reason).or_insert(0) += 1;
        now.duration_since(self.window_start) >= Duration::from_secs(1)
    }

    fn flush(&mut self) {
        if !self.enabled {
            self.reset_window();
            return;
        }
        let elapsed = self.window_start.elapsed().as_secs_f32().max(0.001);
        let fps = self.frame_count as f32 / elapsed;
        let avg_ms = if self.frame_count > 0 {
            self.frame_total_ms / self.frame_count as f32
        } else {
            0.0
        };
        let min_ms = if self.frame_min_ms.is_finite() {
            self.frame_min_ms
        } else {
            0.0
        };

        // ── Repaint-source attribution ─────────────────────────────
        // Sort reasons by frame count, descending. The top reason is
        // what's driving the repaint cadence this second.
        let mut reasons: Vec<(&&'static str, &u64)> = self.reason_counts.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1));
        let dominant_reason = reasons
            .first()
            .map(|(name, count)| (**name, **count))
            .unwrap_or(("none", 0));
        let reason_pct = if self.frame_count > 0 {
            100.0 * dominant_reason.1 as f32 / self.frame_count as f32
        } else {
            0.0
        };

        eprintln!(
            "[ui-prof] fps={:.1} frame_ms={:.2} min={:.2}ms max={:.2}ms root_renders/s={} dominant_reason={}({:.0}%)",
            fps, avg_ms, min_ms, self.frame_max_ms, self.frame_count, dominant_reason.0, reason_pct
        );

        let mut scopes: Vec<_> = self.scopes.iter().filter(|(_, a)| a.count > 0).collect();
        scopes.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));
        for (name, agg) in scopes.iter().take(12) {
            let calls = agg.count;
            let total_ms = agg.total_ns as f32 / 1_000_000.0;
            let avg_ms = if calls > 0 {
                total_ms / calls as f32
            } else {
                0.0
            };
            let max_ms = agg.max_ns as f32 / 1_000_000.0;
            eprintln!(
                "  {name} calls={calls} avg={avg_ms:.2}ms max={max_ms:.2}ms total={total_ms:.2}ms/s",
                calls = calls,
                avg_ms = avg_ms,
                max_ms = max_ms,
                total_ms = total_ms / elapsed
            );
        }

        if !reasons.is_empty() && reasons.len() > 1 {
            let mut line = String::from("[ui-perf] repaint-sources: ");
            for (name, count) in &reasons {
                line.push_str(&format!("{}={}  ", name, count));
            }
            eprintln!("{}", line.trim_end());
        }

        // ── Scope ranking ───────────────────────────────────────────
        // Subtract instrumented child totals from the root to derive
        // "root self time" — otherwise StudioLayout always wins because
        // it transitively includes every child.
        let mut ranked: Vec<(&'static str, ScopeAgg)> =
            self.scopes.iter().map(|(k, v)| (*k, *v)).collect();
        let children_sum_ns: u64 = ranked
            .iter()
            .filter(|(n, _)| ROOT_CHILDREN.contains(n))
            .map(|(_, a)| a.total_ns)
            .sum();
        if let Some((_, root)) = ranked.iter_mut().find(|(n, _)| *n == ROOT_SCOPE) {
            let self_ns = root.total_ns.saturating_sub(children_sum_ns);
            root.total_ns = self_ns;
            root.max_ns = 0;
        }
        ranked.retain(|(_, a)| a.count > 0 && a.total_ns > 0);
        ranked.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns));

        let total_ns: u64 = ranked.iter().map(|(_, a)| a.total_ns).sum();
        let window_ns = (elapsed * 1_000_000_000.0) as u64;

        // Publish the completed window for the overlay. Readers must never see
        // the partially-filled current window — a fresh reset makes every scope
        // look like it ran once, which reads as "nothing is expensive" whether
        // or not that is true.
        self.last_window = WindowSummary {
            scopes: ranked
                .iter()
                .take(8)
                .map(|(name, agg)| ScopeSample {
                    name: if *name == ROOT_SCOPE {
                        "StudioLayout(self)"
                    } else {
                        name
                    },
                    total_ms: agg.total_ns as f32 / 1_000_000.0,
                    percent: if total_ns > 0 {
                        100.0 * agg.total_ns as f32 / total_ns as f32
                    } else {
                        0.0
                    },
                    count: agg.count,
                })
                .collect(),
            cpu_ms_per_frame: if self.frame_count > 0 {
                total_ns as f32 / 1_000_000.0 / self.frame_count as f32
            } else {
                0.0
            },
            frame_ms: avg_ms,
            worst: self.worst.take(),
        };

        if !self.dump {
            self.reset_window();
            return;
        }

        if !ranked.is_empty() {
            let mut line = String::from("[ui-perf] ranked: ");
            for (name, agg) in ranked.iter().take(8) {
                let total_ms = agg.total_ns as f32 / 1_000_000.0;
                let pct = if total_ns > 0 {
                    100.0 * agg.total_ns as f32 / total_ns as f32
                } else {
                    0.0
                };
                let display_name = if *name == ROOT_SCOPE {
                    "StudioLayout(self)"
                } else {
                    *name
                };
                line.push_str(&format!(
                    "{}={:.2}ms({:.0}%,x{})  ",
                    display_name, total_ms, pct, agg.count
                ));
            }
            eprintln!("{}", line.trim_end());
        }

        // ── Verdict ─────────────────────────────────────────────────
        // The whole point: tell the next pass exactly where to cut,
        // and whether an idle-repaint loop is active.
        let verdict = self.compute_verdict(&ranked, &dominant_reason, fps, window_ns);
        eprintln!("[ui-perf] VERDICT: {}", verdict);

        // Mirror the latest verdict to a small file so the next agent
        // pass (or the user) can read it without re-running. Path is
        // overridable via FUTUREBOARD_UI_PERF_LOG; defaults to
        // `target/ui-perf-last.txt` next to the build artifacts.
        let log_path = std::env::var("FUTUREBOARD_UI_PERF_LOG")
            .unwrap_or_else(|_| "target/ui-perf-last.txt".to_string());
        let payload = format!(
            "fps={:.1}\navg_ms={:.2}\nmax_ms={:.2}\nrepaint_per_s={}\ndominant_reason={} ({}/{} frames)\nverdict={}\n\nranked:\n{}\n\ncounts:\n{}\n",
            fps,
            avg_ms,
            self.frame_max_ms,
            self.frame_count,
            dominant_reason.0,
            dominant_reason.1,
            self.frame_count,
            verdict,
            ranked
                .iter()
                .take(8)
                .map(|(n, a)| format!(
                    "  {} = {:.2}ms/s (x{})",
                    if *n == ROOT_SCOPE { "StudioLayout(self)" } else { *n },
                    a.total_ns as f32 / 1_000_000.0,
                    a.count
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            self.counters
                .iter()
                .map(|(n, c)| format!("  {} = {} (max {})", n, c.last, c.max))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        // Best-effort write — we don't want a missing target/ dir to
        // crash the render thread. log_err equivalent: ignore.
        let _ = std::fs::write(&log_path, payload);

        if !self.counters.is_empty() {
            let mut line = String::from("[ui-perf] counts: ");
            for (name, agg) in &self.counters {
                if agg.last == agg.max {
                    line.push_str(&format!("{}={}  ", name, agg.last));
                } else {
                    line.push_str(&format!("{}={}(max {})  ", name, agg.last, agg.max));
                }
            }
            eprintln!("{}", line.trim_end());
        }

        self.reset_window();
    }

    /// Derive a one-line, action-oriented verdict the next optimization
    /// pass can act on without further measurement. Two axes:
    ///   1. Is there a repaint-rate problem? (idle loop, runaway notify)
    ///   2. Where is the per-frame time going? (top scope by total ms/s)
    fn compute_verdict(
        &self,
        ranked: &[(&'static str, ScopeAgg)],
        dominant_reason: &(&'static str, u64),
        fps: f32,
        window_ns: u64,
    ) -> String {
        // Idle loop: predominantly idle/interaction reason but still
        // hitting > ~12 fps means something is dirty-marking the view
        // even though the user isn't doing anything meaningful.
        let mut parts: Vec<String> = Vec::new();
        let idle_loop = dominant_reason.0 == "idle/interaction"
            && dominant_reason.1 >= 50 * self.frame_count.max(1) / 100
            && fps > 12.0;
        if idle_loop {
            parts.push(format!(
                "IDLE-REPAINT-LOOP ({} idle frames/s, {:.0} fps) — \
                 something is calling cx.notify without user input. \
                 Likely culprits: spawn_audio_poll cadence, \
                 smooth_scroll_towards_target convergence, or a \
                 meter/scroll animation that never terminates.",
                dominant_reason.1, fps
            ));
        }

        if let Some((top_name, top_agg)) = ranked.first() {
            let top_ms_s = top_agg.total_ns as f32 / 1_000_000.0;
            let budget_pct = if window_ns > 0 {
                100.0 * top_agg.total_ns as f32 / window_ns as f32
            } else {
                0.0
            };
            let display = if *top_name == ROOT_SCOPE {
                "StudioLayout(self)"
            } else {
                top_name
            };
            // If a single scope eats > 25% of the 1-second window,
            // it's the obvious cut target.
            if budget_pct > 25.0 {
                parts.push(format!(
                    "TOP-SCOPE {} = {:.1}ms/s ({:.0}% of window) — cut this first.",
                    display, top_ms_s, budget_pct
                ));
            } else {
                parts.push(format!(
                    "TOP-SCOPE {} = {:.1}ms/s ({:.0}% of window).",
                    display, top_ms_s, budget_pct
                ));
            }
        }

        // Frame-time pass/fail vs the 60 Hz budget. We use the average,
        // not the max, so a single hitched frame doesn't dominate.
        let avg_ms = if self.frame_count > 0 {
            self.frame_total_ms / self.frame_count as f32
        } else {
            0.0
        };
        if avg_ms > 16.67 && self.frame_count > 0 {
            parts.push(format!(
                "FRAME-BUDGET FAIL — avg {:.2}ms exceeds 16.67ms (60Hz target).",
                avg_ms
            ));
        } else if self.frame_count > 0 {
            parts.push(format!(
                "FRAME-BUDGET OK — avg {:.2}ms within 16.67ms.",
                avg_ms
            ));
        }

        if parts.is_empty() {
            "no data".to_string()
        } else {
            parts.join(" | ")
        }
    }

    fn reset_window(&mut self) {
        self.scopes.clear();
        self.reason_counts.clear();
        self.worst = None;
        for c in self.counters.values_mut() {
            c.max = c.last;
        }
        self.frame_count = 0;
        self.frame_total_ms = 0.0;
        self.frame_min_ms = f32::INFINITY;
        self.frame_max_ms = 0.0;
        self.window_start = Instant::now();
    }
}

thread_local! {
    static COLLECTOR: RefCell<Collector> = RefCell::new(Collector::new());
    static NOTIFY: RefCell<NotifyAgg> = RefCell::new(NotifyAgg::default());
}

/// Record a UI repaint driver (transport, meter, scroll, etc.). Logged at
/// 1 Hz when `FUTUREBOARD_NOTIFY_DEBUG=1`.
pub fn record_notify(reason: &'static str) {
    let _ = NOTIFY.try_with(|n| n.borrow_mut().record(reason));
}

/// What kind of GPU this machine renders on. Detected once at startup from the
/// adapters wgpu can see (see `render::wgpu_renderer::detect_gpu_class`) and
/// published here so cheap render-path checks never touch wgpu.
///
/// Low-end shared-memory iGPUs — Intel Iris/UHD, older Radeon Vega on Windows
/// laptops, Intel Macs — pay for every extra frame in bandwidth the CPU also
/// needs. Apple Silicon is integrated metal but is *not* that class of
/// device: treating it as low-end would leave ProMotion frames on the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuClass {
    /// No discrete or otherwise capable adapter: classic low-end iGPU only.
    IntegratedOnly,
    /// At least one discrete adapter — or a capable integrated GPU such as
    /// Apple Silicon — is available to the process.
    Discrete,
    /// Not detected (enumeration failed, or the `gpu-renderer` feature is off).
    /// Treated exactly like `Discrete`: never slow the UI down on a guess.
    Unknown,
}

/// Adapter flavour used by [`classify_gpu_adapters`]. Kept as a plain enum so
/// the pure classifier can be unit-tested without linking wgpu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuDeviceKind {
    Discrete,
    Integrated,
    Virtual,
    Cpu,
    Other,
}

/// Classify enumerated adapters into a [`GpuClass`]. Pure policy: discrete
/// wins, then capable integrated (Apple Silicon), then classic iGPU.
pub fn classify_gpu_adapters<'a, I>(adapters: I) -> GpuClass
where
    I: IntoIterator<Item = (GpuDeviceKind, &'a str)>,
{
    let mut saw_any = false;
    let mut saw_discrete = false;
    let mut saw_capable_integrated = false;
    let mut saw_classic_integrated = false;
    for (kind, name) in adapters {
        saw_any = true;
        match kind {
            GpuDeviceKind::Discrete => saw_discrete = true,
            GpuDeviceKind::Integrated if is_capable_integrated_gpu(name) => {
                saw_capable_integrated = true;
            }
            GpuDeviceKind::Integrated => saw_classic_integrated = true,
            GpuDeviceKind::Virtual | GpuDeviceKind::Cpu | GpuDeviceKind::Other => {}
        }
    }
    if !saw_any {
        GpuClass::Unknown
    } else if saw_discrete || saw_capable_integrated {
        GpuClass::Discrete
    } else if saw_classic_integrated {
        GpuClass::IntegratedOnly
    } else {
        // Software / virtual only — do not apply the low-end cap on a guess.
        GpuClass::Unknown
    }
}

/// Integrated GPUs that still have enough shared-memory bandwidth that the
/// LowEnd profile and 60 Hz DisplaySync cap would only waste panel rate.
///
/// Today: Apple Silicon (`"Apple M1"`, `"Apple M4 Pro"`, `"Apple GPU"`, …).
/// Keep this list tight — Iris/UHD/Vega must continue to select LowEnd.
fn is_capable_integrated_gpu(name: &str) -> bool {
    // wgpu Metal reports names like "Apple M3 Pro" / "Apple M1" / "Apple GPU".
    name.to_ascii_lowercase().starts_with("apple ")
}

static DETECTED_GPU_CLASS: std::sync::OnceLock<GpuClass> = std::sync::OnceLock::new();

/// Publish the detected GPU class. Call once during startup, **before** the
/// first `power_mode()` read — that resolves lazily and then caches.
pub fn set_detected_gpu_class(class: GpuClass) {
    let _ = DETECTED_GPU_CLASS.set(class);
}

/// The detected GPU class, or [`GpuClass::Unknown`] before detection runs.
pub fn gpu_class() -> GpuClass {
    DETECTED_GPU_CLASS
        .get()
        .copied()
        .unwrap_or(GpuClass::Unknown)
}

/// Render-cost setting profile applied by the UI to reduce frame cost on
/// low-end GPUs. Selected automatically from [`gpu_class`] and overridable at
/// runtime via `FUTUREBOARD_POWER_MODE` (`lowend` / `balanced` / `performance`);
/// read by render code via the cheap accessors below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    Balanced,
    Performance,
    LowEnd,
}

impl PowerMode {
    /// Cap minor grid line density: drop sub-beat lines on low-end so the
    /// timeline grid stops drawing 100+ thin lines per frame.
    pub fn allow_sub_grid_lines(self) -> bool {
        !matches!(self, PowerMode::LowEnd)
    }

    /// Whether expensive visual effects (shadows, glows, blurs over dense
    /// timeline regions) should be drawn.
    pub fn allow_expensive_effects(self) -> bool {
        !matches!(self, PowerMode::LowEnd)
    }

    /// Multiplier on the maximum grid line count budget for the arrangement
    /// view. < 1.0 caps grid density on low-end GPUs.
    pub fn grid_line_budget_scale(self) -> f32 {
        match self {
            PowerMode::Performance => 1.0,
            PowerMode::Balanced => 1.0,
            PowerMode::LowEnd => 0.5,
        }
    }
}

/// Resolve the power mode from an explicit override and the detected GPU class.
/// Pure, so the policy can be tested without a GPU or an environment.
///
/// An integrated-only machine gets [`PowerMode::LowEnd`] by default: that is
/// the configuration where grid density and expensive effects decide whether
/// the UI keeps up. Meter cadence remains display-synced in every profile.
/// Anything else stays Balanced, and an
/// explicit override always wins — a user who asks for Performance on an iGPU
/// gets it.
pub fn resolve_power_mode(override_label: Option<&str>, class: GpuClass) -> PowerMode {
    match override_label.map(str::to_ascii_lowercase).as_deref() {
        Some("lowend") | Some("low-end") | Some("low_end") | Some("low") => {
            return PowerMode::LowEnd
        }
        Some("performance") | Some("perf") | Some("high") => return PowerMode::Performance,
        Some("balanced") | Some("normal") => return PowerMode::Balanced,
        _ => {}
    }
    match class {
        GpuClass::IntegratedOnly => PowerMode::LowEnd,
        GpuClass::Discrete | GpuClass::Unknown => PowerMode::Balanced,
    }
}

/// Returns the currently active power mode: the `FUTUREBOARD_POWER_MODE`
/// override if set, otherwise the profile for the detected GPU class. Cached on
/// first read, so startup must publish the GPU class (and any override must be
/// in the environment) before the first render.
pub fn power_mode() -> PowerMode {
    static MODE: std::sync::OnceLock<PowerMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let override_label = std::env::var("FUTUREBOARD_POWER_MODE").ok();
        let mode = resolve_power_mode(override_label.as_deref(), gpu_class());
        eprintln!(
            "[perf] power mode {mode:?} (gpu class {:?}, override {:?})",
            gpu_class(),
            override_label
        );
        mode
    })
}

/// Whether the `FUTUREBOARD_PERF_DEBUG` flag is active. Enables per-second
/// perf summary lines covering FPS, visible counts, notify/sec, and meter
/// update rate.
pub fn perf_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PERF_DEBUG").is_some())
}

/// Whether engine-sync tracing is on (`FUTUREBOARD_ENGINE_SYNC_DEBUG`).
///
/// The engine sync runs on the UI thread, and every committed MIDI note edit
/// forces one. Its trace was unconditional and prints a line per insert and per
/// clip plus a six-line route-graph block, so a single click in the piano roll
/// paid for a few dozen synchronous console writes — which on Windows is slow
/// enough to feel, and is charged whether or not anyone is reading stderr.
pub fn engine_sync_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_ENGINE_SYNC_DEBUG").is_some())
}

/// Whether the optional perf HUD overlay is enabled.
pub fn perf_hud_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PERF_HUD").is_some())
}

/// Whether to outline timeline clip rects (ruler markings, tempo / time-signature
/// lane content, automation lanes). Enabled with `FUTUREBOARD_UI_DEBUG_CLIPS=1`;
/// used to confirm marker overlays stay clipped to their content areas and never
/// bleed over the left Arrangement / lane / track headers.
pub fn ui_debug_clips_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_UI_DEBUG_CLIPS").is_some())
}

/// Shared `FUTUREBOARD_UI_DEBUG_CLIPS=1` outline overlay for a content rect.
/// Deliberately an off-palette color (not a theme token) so it reads as a
/// debug marker rather than real UI, and stands out against any theme.
/// Callers `.children(perf::debug_clip_outline())`.
pub fn debug_clip_outline() -> Option<gpui::Div> {
    use gpui::Styled;
    ui_debug_clips_enabled().then(|| {
        gpui::div()
            .absolute()
            .inset_0()
            .border(gpui::px(1.0))
            .border_color(gpui::rgb(0xff00ff))
    })
}

/// Returns whether perf instrumentation is active. Cheap — single TLS
/// read. Callers can use this to skip building counter labels when
/// disabled, though `count` is already a no-op when disabled.
pub fn enabled() -> bool {
    COLLECTOR.try_with(|c| c.borrow().enabled).unwrap_or(false)
}

/// Build identity of the running executable: its own file timestamp.
///
/// Resolved once at runtime, so it needs no build script and can never drift
/// from the binary it describes. Shown in the Profiler so "is this the build I
/// just made?" is answerable from the screen instead of by inference.
pub fn running_build_stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| {
        let modified = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok());
        let Some(modified) = modified else {
            return "unknown".to_string();
        };
        let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
            return "unknown".to_string();
        };
        // Local wall clock is not available without a date crate; seconds since
        // the epoch is enough to tell two builds apart, and the derived
        // day-relative time makes it readable.
        let secs = since_epoch.as_secs();
        let days = secs / 86_400;
        let rem = secs % 86_400;
        format!(
            "d{} {:02}:{:02}:{:02}Z",
            days,
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    })
}

/// Runtime request for scope collection, independent of the env vars.
///
/// Set while the Profiler overlay is on screen so its "where is the frame
/// going" rows have data to show. The env flags stay authoritative for the
/// once-per-second stderr dump; this only turns the in-memory aggregation on.
static COLLECT_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn scope aggregation on/off at runtime. Idempotent and cheap; the cost
/// while on is one `Instant::now()` plus a small map update per scope.
pub fn set_collection_requested(on: bool) {
    COLLECT_REQUESTED.store(on, std::sync::atomic::Ordering::Relaxed);
    let _ = COLLECTOR.try_with(|c| {
        let mut c = c.borrow_mut();
        if on {
            c.enabled = true;
        } else if !env_collection_enabled() {
            c.enabled = false;
            c.scopes.clear();
            c.last_window = WindowSummary::default();
        }
    });
}

fn env_collection_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_UI_PERF").is_some()
            || std::env::var_os("FUTUREBOARD_UI_PROFILE").is_some()
    })
}

/// One ranked scope for the Profiler overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeSample {
    pub name: &'static str,
    /// Wall time spent in this scope during the current window.
    pub total_ms: f32,
    /// Share of all instrumented time in the window.
    pub percent: f32,
    /// Times the scope was entered in the window.
    pub count: u64,
}

/// Top `limit` scopes from the last **completed** one-second window, most
/// expensive first, with the same root self-time correction the log applies.
pub fn top_scopes(limit: usize) -> Vec<ScopeSample> {
    COLLECTOR
        .try_with(|c| {
            let c = c.borrow();
            if !c.enabled {
                return Vec::new();
            }
            c.last_window.scopes.iter().take(limit).cloned().collect()
        })
        .unwrap_or_default()
}

/// The worst frame of the last completed window, when one stuttered.
///
/// `None` when every frame in the window came in under [`STALL_FRAME_MS`] —
/// which is the answer "nothing stuttered", and reads differently from a stall
/// with no attribution.
pub fn worst_frame() -> Option<WorstFrame> {
    COLLECTOR
        .try_with(|c| {
            let c = c.borrow();
            if !c.enabled {
                return None;
            }
            c.last_window.worst.clone()
        })
        .unwrap_or_default()
}

/// Instrumented CPU time per frame from the last completed window.
///
/// Deliberately does **not** report a frame duration: the overlay already has
/// an authoritative one from its own frame diagnostics, and gating this on the
/// collector's internal frame counter meant a window that rolled over with too
/// few samples silently hid the whole accounting block — exactly when it is
/// most needed. Returns 0.0 before the first completed window rather than
/// `None`, so callers can always render the row.
pub fn instrumented_cpu_ms_per_frame() -> f32 {
    COLLECTOR
        .try_with(|c| {
            let c = c.borrow();
            if !c.enabled {
                return 0.0;
            }
            c.last_window.cpu_ms_per_frame
        })
        .unwrap_or(0.0)
}

/// Record a named counter (visible row count, grid line count, etc.).
/// Cheap no-op when disabled. Last write wins for the log line; the
/// per-window max is also retained.
pub fn count(name: &'static str, value: u64) {
    let _ = COLLECTOR.try_with(|c| {
        let mut c = c.borrow_mut();
        if !c.enabled {
            return;
        }
        c.record_counter(name, value);
    });
}

/// RAII timing scope. Records elapsed wall time into the named bucket
/// on drop. No-op when perf is disabled.
pub struct PerfScope {
    name: &'static str,
    start: Option<Instant>,
}

impl PerfScope {
    pub fn enter(name: &'static str) -> Self {
        let start = COLLECTOR
            .try_with(|c| c.borrow().enabled.then(Instant::now))
            .ok()
            .flatten();
        Self { name, start }
    }
}

impl Drop for PerfScope {
    fn drop(&mut self) {
        let Some(start) = self.start.take() else {
            return;
        };
        let ns = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let name = self.name;
        // During process exit the main thread TLS may already be destroyed; `with`
        // panics with AccessError — use `try_with` and skip recording instead.
        let _ = COLLECTOR.try_with(|c| c.borrow_mut().add_scope(name, ns));
    }
}

/// `FUTUREBOARD_MIXER_TREE_DEBUG=1` — log tree cache rebuilds.
pub fn mixer_tree_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIXER_TREE_DEBUG").is_some())
}

/// `FUTUREBOARD_MIXER_GPU_DEBUG=1` — log mixer GPU primitive static-batch
/// rebuilds and backend selection.
pub fn mixer_gpu_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIXER_GPU_DEBUG").is_some())
}

/// Called from the root render once per frame. `reason` is a short
/// label (e.g. "transport", "menu", "idle/interaction"). Returns the
/// formatted HUD string and flushes the per-second log when the
/// window rolls over.
pub fn tick_root_frame(reason: &'static str) {
    let Ok(should_flush) = COLLECTOR.try_with(|c| c.borrow_mut().tick_frame(reason)) else {
        return;
    };
    if should_flush {
        let _ = COLLECTOR.try_with(|c| c.borrow_mut().flush());
    }
}

/// Resident memory of this process, in bytes, or `None` where the platform has
/// no cheap way to ask.
///
/// Cached for a second because the caller is the transport's load meter, which
/// ticks with the audio meters: the number moves in megabytes over seconds, and
/// asking the kernel thirty times a second would cost more than it reports.
/// `None` surfaces as a hidden readout rather than a zero — a RAM meter reading
/// 0 MB is a lie, an absent one is not.
/// Disk traffic attributable to Futureboard, in bytes per second.
///
/// Replaces the UI-thread load that used to sit here. A DAW's other resource
/// question is whether the disk is keeping up: streaming audio, sampler
/// libraries paging in, and peak files being written all land here, and a
/// stream that cannot be fed is heard as a dropout rather than seen as a stall.
///
/// Counts the same processes the memory readout does — Studio plus its plugin
/// hosts — because a sampler streaming from disk does it in the host, not here.
///
/// `None` until two samples exist and on any platform that cannot report it: an
/// unmeasured rate is not a rate of zero.
pub fn disk_io_bytes_per_sec() -> Option<f64> {
    let usage = resource_usage();
    usage.studio_known.then(|| usage.total().disk_bytes_per_sec)
}

// ── Whole-session resource usage ────────────────────────────────────────────
//
// Studio hosts plug-ins out of process, so "what is Futureboard costing this
// machine" is never one process's numbers. Memory, disk and CPU are all read
// for the Studio *and* every live plug-in host, on one shared one-second tick:
// three separate samplers on three cadences would have read the same handles
// three times and still disagreed with each other about which second they were
// describing.

/// What one side of the session (Studio, or the plug-in hosts together) costs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessLoad {
    /// Working set — the number Task Manager calls "Memory".
    pub memory_bytes: u64,
    /// Share of the whole machine's CPU over the last window, 0..100. Scaled by
    /// core count, so 100 means every core, not one of them.
    pub cpu_percent: f32,
    /// Bytes through the I/O manager per second over the last window.
    pub disk_bytes_per_sec: f64,
}

impl ProcessLoad {
    fn plus(self, other: Self) -> Self {
        Self {
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            cpu_percent: self.cpu_percent + other.cpu_percent,
            disk_bytes_per_sec: self.disk_bytes_per_sec + other.disk_bytes_per_sec,
        }
    }
}

/// The session's cost, split the way the user can act on it: the app, and the
/// plug-ins they chose to load.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceUsage {
    pub studio: ProcessLoad,
    /// False before the first sample, and on platforms with no reader — which is
    /// not the same as "zero", and must not print as it.
    pub studio_known: bool,
    /// Every live plug-in host, summed.
    pub plugin_hosts: ProcessLoad,
    /// How many hosts answered. Zero is a real answer: no plug-in loaded yet.
    pub plugin_host_count: usize,
}

impl ResourceUsage {
    pub fn total(&self) -> ProcessLoad {
        self.studio.plus(self.plugin_hosts)
    }
}

/// How often the counters are re-read. One second: these are syscalls per
/// process, and a rate averaged over less than that reads as noise.
const RESOURCE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Cumulative counters from the previous tick, per process.
///
/// CPU and disk are both *counters*, not gauges: the machine reports totals
/// since the process started, and a rate only exists as the difference between
/// two readings. Keyed by pid, with `0` standing for the Studio itself.
#[derive(Clone, Copy, Default)]
struct ProcessCounters {
    cpu_100ns: u64,
    io_bytes: u64,
}

/// The session's CPU, memory and disk, as of the last background reading.
///
/// Free to call: it returns a published snapshot, so the meter tick, the
/// profiler overlay and the Performance Monitor all read the same second's
/// numbers without any of them touching a process handle. The syscalls happen
/// on [`start_resource_sampler`]'s thread — see there for why they must not
/// happen here.
pub fn resource_usage() -> ResourceUsage {
    start_resource_sampler();
    let Ok(latest) = published_resource_usage().lock() else {
        return ResourceUsage::default();
    };
    *latest
}

/// The last reading published by the sampler thread.
fn published_resource_usage() -> &'static std::sync::Mutex<ResourceUsage> {
    static LATEST: std::sync::OnceLock<std::sync::Mutex<ResourceUsage>> =
        std::sync::OnceLock::new();
    LATEST.get_or_init(|| std::sync::Mutex::new(ResourceUsage::default()))
}

/// Start the once-a-second sampler, if it is not already running.
///
/// Opening a handle on every plug-in host and asking each for four counters is
/// a burst of syscalls, and it used to happen on whichever meter tick crossed
/// the poll interval — on the UI thread, once a second, growing with the number
/// of hosts loaded. That is a dropped frame a second on a big session, and it
/// is invisible in an average.
fn start_resource_sampler() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("fb-resource-sampler".into())
            .spawn(|| {
                let mut sampler = ResourceSampler::default();
                loop {
                    let value = sampler.sample();
                    if let Ok(mut latest) = published_resource_usage().lock() {
                        *latest = value;
                    }
                    std::thread::sleep(RESOURCE_POLL);
                }
            });
    });
}

/// Previous counters, so the rates are a difference between two readings.
#[derive(Default)]
struct ResourceSampler {
    counters: std::collections::HashMap<u32, ProcessCounters>,
    at: Option<std::time::Instant>,
}

impl ResourceSampler {
    fn sample(&mut self) -> ResourceUsage {
        use std::collections::HashMap;
        use std::time::Instant;

        let now = Instant::now();
        let elapsed = self
            .at
            .map(|at| now.duration_since(at).as_secs_f64())
            .filter(|seconds| *seconds > 0.0);
        self.at = Some(now);

        let previous = std::mem::take(&mut self.counters);
        let mut counters: HashMap<u32, ProcessCounters> = HashMap::new();
        let mut sample = |pid: u32, raw: Option<(u64, ProcessCounters)>| -> ProcessLoad {
            let Some((memory_bytes, current)) = raw else {
                return ProcessLoad::default();
            };
            counters.insert(pid, current);
            // No previous reading, or a process that restarted onto the same
            // pid: report the gauge and leave the rates at zero until there are
            // two readings to subtract. A counter that went backwards is a new
            // process, never negative work.
            let (cpu_percent, disk_bytes_per_sec) = match (previous.get(&pid).copied(), elapsed) {
                (Some(previous), Some(elapsed)) => {
                    let cpu_delta = current.cpu_100ns.saturating_sub(previous.cpu_100ns) as f64;
                    let io_delta = current.io_bytes.saturating_sub(previous.io_bytes) as f64;
                    let cores = cpu_core_count() as f64;
                    // 100 ns units against wall-clock seconds, spread over
                    // the cores the work could have used.
                    let percent = (cpu_delta / 1.0e7) / elapsed / cores * 100.0;
                    (percent.clamp(0.0, 100.0) as f32, io_delta / elapsed)
                }
                _ => (0.0, 0.0),
            };
            ProcessLoad {
                memory_bytes,
                cpu_percent,
                disk_bytes_per_sec,
            }
        };

        let studio_raw = read_studio_counters();
        let studio_known = studio_raw.is_some();
        let studio = sample(0, studio_raw);

        let mut plugin_hosts = ProcessLoad::default();
        let mut plugin_host_count = 0usize;
        for pid in running_plugin_host_pids() {
            if let Some(raw) = read_pid_counters(pid) {
                plugin_hosts = plugin_hosts.plus(sample(pid, Some(raw)));
                plugin_host_count += 1;
            }
        }

        drop(sample);
        self.counters = counters;
        ResourceUsage {
            studio,
            studio_known,
            plugin_hosts,
            plugin_host_count,
        }
    }
}

/// Cores to divide CPU time by. Queried once — it does not change.
fn cpu_core_count() -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CORES.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1)
    })
}

#[cfg(windows)]
fn read_studio_counters() -> Option<(u64, ProcessCounters)> {
    use windows::Win32::System::Threading::GetCurrentProcess;
    counters_for_handle(unsafe { GetCurrentProcess() })
}

#[cfg(not(windows))]
fn read_studio_counters() -> Option<(u64, ProcessCounters)> {
    None
}

#[cfg(windows)]
fn read_pid_counters(pid: u32) -> Option<(u64, ProcessCounters)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
    }
    .ok()?;
    let out = counters_for_handle(handle);
    unsafe {
        let _ = CloseHandle(handle);
    }
    out
}

#[cfg(not(windows))]
fn read_pid_counters(_pid: u32) -> Option<(u64, ProcessCounters)> {
    None
}

/// Working set, CPU time and I/O totals for one open process handle.
#[cfg(windows)]
fn counters_for_handle(
    handle: windows::Win32::Foundation::HANDLE,
) -> Option<(u64, ProcessCounters)> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{GetProcessIoCounters, GetProcessTimes};

    let mut memory = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    unsafe { GetProcessMemoryInfo(handle, &mut memory, size) }.ok()?;

    // Kernel *and* user: a DAW's file and device work is kernel time, and
    // charging it to nobody would make the readout disagree with Task Manager
    // exactly when the machine is busiest.
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }.ok()?;
    let filetime_100ns =
        |time: FILETIME| ((time.dwHighDateTime as u64) << 32) | (time.dwLowDateTime as u64);

    let mut io = Default::default();
    // Missing I/O counters are not a reason to lose the memory and CPU readings
    // that came back fine.
    let io_bytes = unsafe { GetProcessIoCounters(handle, &mut io) }
        .ok()
        .map(|()| io.ReadTransferCount.saturating_add(io.WriteTransferCount))
        .unwrap_or(0);

    Some((
        memory.WorkingSetSize as u64,
        ProcessCounters {
            cpu_100ns: filetime_100ns(kernel).saturating_add(filetime_100ns(user)),
            io_bytes,
        },
    ))
}

/// Working set attributable to Futureboard, split by process.
///
/// Studio hosts plug-ins out of process, and that is where the memory goes: a
/// session with a sampler loaded can hold a couple of gigabytes in
/// `FutureboardPluginHostX64` while Studio's own process sits near 1.5 GB.
/// Reporting only Studio's process made the readout disagree with Task Manager
/// by more than it agreed, and understated the number that actually decides
/// whether this session still fits in RAM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    /// Studio's own working set. `None` where the platform cannot report it —
    /// the readout hides itself rather than printing a partial total.
    pub studio_bytes: Option<u64>,
    /// Summed working set of the plugin-host children that answered.
    pub plugin_host_bytes: u64,
    /// How many host processes contributed. Zero is a real answer: no plug-in
    /// has been loaded yet.
    pub plugin_hosts: usize,
}

impl MemoryUsage {
    /// What the machine is actually holding for this session.
    pub fn total_bytes(&self) -> Option<u64> {
        self.studio_bytes
            .map(|studio| studio.saturating_add(self.plugin_host_bytes))
    }
}

/// Cached memory reading for the transport readout.
///
/// Polled at the meter cadence but sampled once a second: `GetProcessMemoryInfo`
/// on a handful of processes is a syscall each, and the readout only shows
/// megabytes.
pub fn memory_usage() -> MemoryUsage {
    let usage = resource_usage();
    MemoryUsage {
        studio_bytes: usage.studio_known.then_some(usage.studio.memory_bytes),
        plugin_host_bytes: usage.plugin_hosts.memory_bytes,
        plugin_hosts: usage.plugin_host_count,
    }
}

/// Studio's own working set, uncached. Prefer [`memory_usage`].
pub fn process_memory_bytes() -> Option<u64> {
    memory_usage().studio_bytes
}

/// PIDs of the plugin hosts that are actually alive.
///
/// The registry keeps `Exited` records around, so only `Running` hosts are
/// opened — a recycled PID would otherwise charge this session for a stranger's
/// memory or disk traffic. A host that exits between the listing and the
/// `OpenProcess` simply does not contribute; it is not an error worth reporting
/// on a readout.
#[cfg(windows)]
fn running_plugin_host_pids() -> Vec<u32> {
    use SpherePluginHost::plugin_host_lifecycle::{BridgeHostManager, HostLifecycleState};

    BridgeHostManager::global()
        .host_records()
        .into_iter()
        .filter(|record| record.state == HostLifecycleState::Running)
        .map(|record| record.pid)
        .collect()
}

// The plugin-host registry is a Windows bridge; there are no host processes to
// charge anything to elsewhere, and `read_pid_counters` would return `None` for
// each of them anyway.
#[cfg(not(windows))]
fn running_plugin_host_pids() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod power_mode_tests {
    use super::*;

    #[test]
    fn an_integrated_only_machine_defaults_to_the_low_end_profile() {
        assert_eq!(
            resolve_power_mode(None, GpuClass::IntegratedOnly),
            PowerMode::LowEnd
        );
    }

    #[test]
    fn a_discrete_or_undetected_gpu_keeps_the_balanced_profile() {
        assert_eq!(
            resolve_power_mode(None, GpuClass::Discrete),
            PowerMode::Balanced
        );
        // Undetected must never cost the user frames on a guess.
        assert_eq!(
            resolve_power_mode(None, GpuClass::Unknown),
            PowerMode::Balanced
        );
    }

    #[test]
    fn an_explicit_override_wins_over_the_detected_hardware() {
        assert_eq!(
            resolve_power_mode(Some("performance"), GpuClass::IntegratedOnly),
            PowerMode::Performance,
            "asking for Performance on an iGPU must be honoured"
        );
        assert_eq!(
            resolve_power_mode(Some("Balanced"), GpuClass::IntegratedOnly),
            PowerMode::Balanced
        );
        assert_eq!(
            resolve_power_mode(Some("lowend"), GpuClass::Discrete),
            PowerMode::LowEnd
        );
        // Unrecognized values fall through to the hardware default rather than
        // silently pinning a profile.
        assert_eq!(
            resolve_power_mode(Some("turbo"), GpuClass::IntegratedOnly),
            PowerMode::LowEnd
        );
    }

    #[test]
    fn the_low_end_profile_only_ever_reduces_work() {
        assert!(PowerMode::LowEnd.grid_line_budget_scale() < 1.0);
        assert!(!PowerMode::LowEnd.allow_sub_grid_lines());
        assert!(!PowerMode::LowEnd.allow_expensive_effects());
    }

    #[test]
    fn classic_igpu_is_low_end_but_apple_silicon_is_not() {
        assert_eq!(
            classify_gpu_adapters([(GpuDeviceKind::Integrated, "Intel(R) UHD Graphics 620")]),
            GpuClass::IntegratedOnly
        );
        assert_eq!(
            classify_gpu_adapters([(GpuDeviceKind::Integrated, "AMD Radeon(TM) Graphics")]),
            GpuClass::IntegratedOnly
        );
        assert_eq!(
            classify_gpu_adapters([(GpuDeviceKind::Integrated, "Apple M3 Pro")]),
            GpuClass::Discrete,
            "Apple Silicon must not inherit the Iris/UHD LowEnd profile"
        );
        assert_eq!(
            classify_gpu_adapters([(GpuDeviceKind::Discrete, "NVIDIA GeForce RTX 3070")]),
            GpuClass::Discrete
        );
        assert_eq!(
            classify_gpu_adapters([(GpuDeviceKind::Cpu, "llvmpipe")]),
            GpuClass::Unknown
        );
        assert_eq!(classify_gpu_adapters([]), GpuClass::Unknown);
    }
}
