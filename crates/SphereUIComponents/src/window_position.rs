//! Window placement helpers (main studio + external dialogs).
//!
//! All math stays in GPUI logical [`Pixels`]. Win32 `RECT` values are never mixed
//! into these helpers — the platform converts logical bounds when creating HWNDs.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    bounds, point, px, size, App, Bounds, DisplayId, Pixels, Point, Size, Window, WindowBounds,
    WindowOptions,
};
use serde::{Deserialize, Serialize};

use crate::paths::FutureboardPaths;
use crate::platform_chrome;

/// Default main studio window size (logical pixels).
pub const STUDIO_WINDOW_WIDTH: f32 = 1400.0;
pub const STUDIO_WINDOW_HEIGHT: f32 = 900.0;

const SAVED_MIN_WIDTH: f32 = 640.0;
const SAVED_MIN_HEIGHT: f32 = 480.0;
const MIN_WORK_AREA_INTERSECT: f32 = 64.0;

static STUDIO_PLACEMENT_CONFIRMED: AtomicBool = AtomicBool::new(false);

pub fn window_position_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_WINDOW_POSITION_DEBUG").is_some())
}

fn log_position(message: &str) {
    if window_position_debug_enabled() {
        eprintln!("[window-position] {message}");
    }
}

#[cfg(target_os = "windows")]
fn platform_tag() -> &'static str {
    "windows"
}

#[cfg(target_os = "macos")]
fn platform_tag() -> &'static str {
    "macos"
}

#[cfg(target_os = "linux")]
fn platform_tag() -> &'static str {
    "linux"
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_tag() -> &'static str {
    "other"
}

fn bounds_area(bounds: Bounds<Pixels>) -> f32 {
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    (w * h).max(0.0)
}

fn format_bounds(bounds: Bounds<Pixels>) -> String {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    format!("({ox:.0},{oy:.0},{w:.0}x{h:.0})")
}

/// Reject degenerate owner rectangles that would produce 0,0 child placement.
pub fn is_valid_owner_bounds(bounds: Bounds<Pixels>) -> bool {
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    if !w.is_finite() || !h.is_finite() {
        return false;
    }
    if w <= 1.0 || h <= 1.0 {
        return false;
    }
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    ox.is_finite() && oy.is_finite()
}

fn active_window_bounds(cx: &mut App) -> Option<Bounds<Pixels>> {
    cx.active_window()
        .and_then(|handle| handle.update(cx, |_, window, _| window.bounds()).ok())
}

/// Largest valid window on screen — avoids menu/tiny popup HWNDs on Windows.
fn best_window_bounds(cx: &mut App) -> Option<Bounds<Pixels>> {
    let mut best: Option<Bounds<Pixels>> = None;
    for handle in cx.windows() {
        let Ok(bounds) = handle.update(cx, |_, window, _| window.bounds()) else {
            continue;
        };
        if !is_valid_owner_bounds(bounds) {
            continue;
        }
        if best
            .map(|b| bounds_area(bounds) > bounds_area(b))
            .unwrap_or(true)
        {
            best = Some(bounds);
        }
    }
    best
}

fn point_in_bounds(center: Point<Pixels>, area: Bounds<Pixels>) -> bool {
    let px: f32 = center.x.into();
    let py: f32 = center.y.into();
    let ox: f32 = area.origin.x.into();
    let oy: f32 = area.origin.y.into();
    let ow: f32 = area.size.width.into();
    let oh: f32 = area.size.height.into();
    px >= ox && py >= oy && px < ox + ow && py < oy + oh
}

/// Prefer explicit owner bounds; otherwise active, then largest studio-like window.
pub fn resolve_owner_bounds(
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    resolve_owner_bounds_with_preferred(owner_bounds, None, cx)
}

/// Like [`resolve_owner_bounds`] but prefers `studio_bounds` (main workspace HWND) before active window.
pub fn resolve_owner_bounds_with_preferred(
    owner_bounds: Option<Bounds<Pixels>>,
    studio_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    if let Some(bounds) = owner_bounds {
        if is_valid_owner_bounds(bounds) {
            log_position(&format!(
                "resolve_owner: explicit valid bounds={} platform={}",
                format_bounds(bounds),
                platform_tag()
            ));
            return Some(bounds);
        }
        log_position(&format!(
            "resolve_owner: explicit INVALID bounds={} — trying fallbacks platform={}",
            format_bounds(bounds),
            platform_tag()
        ));
    }

    if let Some(bounds) = studio_bounds {
        if is_valid_owner_bounds(bounds) {
            log_position(&format!(
                "resolve_owner: studio_window valid bounds={} platform={}",
                format_bounds(bounds),
                platform_tag()
            ));
            return Some(bounds);
        }
        log_position(&format!(
            "resolve_owner: studio_window INVALID bounds={} platform={}",
            format_bounds(bounds),
            platform_tag()
        ));
    }

    if let Some(bounds) = active_window_bounds(cx) {
        if is_valid_owner_bounds(bounds) {
            log_position(&format!(
                "resolve_owner: active_window valid bounds={} platform={}",
                format_bounds(bounds),
                platform_tag()
            ));
            return Some(bounds);
        }
        log_position(&format!(
            "resolve_owner: active_window INVALID bounds={} platform={}",
            format_bounds(bounds),
            platform_tag()
        ));
    }

    if let Some(bounds) = best_window_bounds(cx) {
        log_position(&format!(
            "resolve_owner: largest_window fallback bounds={} platform={}",
            format_bounds(bounds),
            platform_tag()
        ));
        return Some(bounds);
    }

    log_position(&format!(
        "resolve_owner: no valid owner (monitor center fallback) platform={}",
        platform_tag()
    ));
    None
}

/// Resolve owner bounds from an open [`Window`] when dispatching from UI callbacks.
pub fn resolve_owner_bounds_from_window(
    owner_bounds: Option<Bounds<Pixels>>,
    window: &Window,
) -> Bounds<Pixels> {
    owner_bounds
        .filter(|b| is_valid_owner_bounds(*b))
        .unwrap_or_else(|| window.bounds())
}

/// Monitor work area containing `parent_bounds` center, else primary visible area.
fn monitor_work_area(parent_bounds: Option<Bounds<Pixels>>, cx: &App) -> Bounds<Pixels> {
    let fallback = || {
        cx.primary_display()
            .map(|display| display.visible_bounds())
            .unwrap_or_else(|| Bounds::centered(None, size(px(1400.0), px(900.0)), cx))
    };

    let Some(parent) = parent_bounds.filter(|b| is_valid_owner_bounds(*b)) else {
        let work = fallback();
        log_position(&format!(
            "monitor_work_area: primary fallback work={}",
            format_bounds(work)
        ));
        return work;
    };

    let center = parent.center();
    if let Some(display) = cx.displays().into_iter().find(|display| {
        let b = display.bounds();
        let px: f32 = center.x.into();
        let py: f32 = center.y.into();
        let ox: f32 = b.origin.x.into();
        let oy: f32 = b.origin.y.into();
        let ow: f32 = b.size.width.into();
        let oh: f32 = b.size.height.into();
        px >= ox && py >= oy && px < ox + ow && py < oy + oh
    }) {
        let work = display.visible_bounds();
        log_position(&format!(
            "monitor_work_area: display_for_parent center=({:.0},{:.0}) work={}",
            f32::from(center.x),
            f32::from(center.y),
            format_bounds(work)
        ));
        return work;
    }

    let work = fallback();
    log_position(&format!(
        "monitor_work_area: parent center not on any display — primary work={}",
        format_bounds(work)
    ));
    work
}

/// Center `requested_size` over `parent_bounds`, clamped inside the monitor work area.
pub fn centered_window_bounds(
    parent_bounds: Option<Bounds<Pixels>>,
    requested_size: Size<Pixels>,
    cx: &mut App,
) -> Bounds<Pixels> {
    let parent = parent_bounds.filter(|b| is_valid_owner_bounds(*b));
    let work = monitor_work_area(parent, cx);
    let scale = cx
        .active_window()
        .and_then(|h| h.update(cx, |_, w, _| w.scale_factor()).ok())
        .unwrap_or(1.0);

    let req_w: f32 = requested_size.width.into();
    let req_h: f32 = requested_size.height.into();
    let work_x: f32 = work.origin.x.into();
    let work_y: f32 = work.origin.y.into();
    let work_w: f32 = work.size.width.into();
    let work_h: f32 = work.size.height.into();

    let (mut x, mut y) = if let Some(parent) = parent {
        let px: f32 = parent.origin.x.into();
        let py: f32 = parent.origin.y.into();
        let pw: f32 = parent.size.width.into();
        let ph: f32 = parent.size.height.into();
        (px + (pw - req_w) / 2.0, py + (ph - req_h) / 2.0)
    } else {
        (
            work_x + (work_w - req_w) / 2.0,
            work_y + (work_h - req_h) / 2.0,
        )
    };

    let margin = 8.0;
    if x + req_w + margin > work_x + work_w {
        x = work_x + work_w - req_w - margin;
    }
    if y + req_h + margin > work_y + work_h {
        y = work_y + work_h - req_h - margin;
    }
    if x < work_x + margin {
        x = work_x + margin;
    }
    if y < work_y + margin {
        y = work_y + margin;
    }

    let result = bounds(point(px(x), px(y)), requested_size);
    log_position(&format!(
        "centered: platform={} scale_factor={scale:.2} parent={} work={} requested=({req_w:.0}x{req_h:.0}) final=({x:.0},{y:.0})",
        platform_tag(),
        parent.map(format_bounds).unwrap_or_else(|| "none".into()),
        format_bounds(work),
    ));
    result
}

/// GPUI display for the monitor containing the owner window center (Windows HWND placement).
pub fn display_id_for_owner_bounds(
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) -> Option<DisplayId> {
    let parent = owner_bounds.filter(|b| is_valid_owner_bounds(*b))?;
    let center = parent.center();
    let id = cx.displays().into_iter().find_map(|display| {
        if point_in_bounds(center, display.bounds()) {
            Some(display.id())
        } else {
            None
        }
    });
    log_position(&format!(
        "display_id_for_owner: parent={} center=({:.0},{:.0}) display={:?} platform={}",
        format_bounds(parent),
        f32::from(center.x),
        f32::from(center.y),
        id,
        platform_tag()
    ));
    id
}

/// Attach the monitor that contains the owner so Win32 `retrieve_window_placement` validates bounds.
pub fn apply_owner_display(
    options: &mut gpui::WindowOptions,
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) {
    if let Some(display_id) = display_id_for_owner_bounds(owner_bounds, cx) {
        options.display_id = Some(display_id);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SavedPoint {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SavedSize {
    width: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SavedStudioWindowBounds {
    origin: SavedPoint,
    size: SavedSize,
}

impl From<Bounds<Pixels>> for SavedStudioWindowBounds {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            origin: SavedPoint {
                x: bounds.origin.x.into(),
                y: bounds.origin.y.into(),
            },
            size: SavedSize {
                width: bounds.size.width.into(),
                height: bounds.size.height.into(),
            },
        }
    }
}

impl SavedStudioWindowBounds {
    fn to_bounds(self) -> Bounds<Pixels> {
        bounds(
            point(px(self.origin.x), px(self.origin.y)),
            size(px(self.size.width), px(self.size.height)),
        )
    }
}

fn studio_window_file() -> std::path::PathBuf {
    FutureboardPaths::resolve().studio_window_file
}

fn rects_intersect(a: Bounds<Pixels>, b: Bounds<Pixels>) -> bool {
    let ax: f32 = a.origin.x.into();
    let ay: f32 = a.origin.y.into();
    let aw: f32 = a.size.width.into();
    let ah: f32 = a.size.height.into();
    let bx: f32 = b.origin.x.into();
    let by: f32 = b.origin.y.into();
    let bw: f32 = b.size.width.into();
    let bh: f32 = b.size.height.into();
    ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by
}

fn intersection_size(a: Bounds<Pixels>, b: Bounds<Pixels>) -> (f32, f32) {
    let ax: f32 = a.origin.x.into();
    let ay: f32 = a.origin.y.into();
    let aw: f32 = a.size.width.into();
    let ah: f32 = a.size.height.into();
    let bx: f32 = b.origin.x.into();
    let by: f32 = b.origin.y.into();
    let bw: f32 = b.size.width.into();
    let bh: f32 = b.size.height.into();
    let left = ax.max(bx);
    let top = ay.max(by);
    let right = (ax + aw).min(bx + bw);
    let bottom = (ay + ah).min(by + bh);
    ((right - left).max(0.0), (bottom - top).max(0.0))
}

/// True when bounds look like an unstaged Win32 spawn at the default size (0,0).
fn is_unpositioned_spawn_snapshot(bounds: Bounds<Pixels>) -> bool {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    ox.abs() < 0.5
        && oy.abs() < 0.5
        && (w - STUDIO_WINDOW_WIDTH).abs() < 1.0
        && (h - STUDIO_WINDOW_HEIGHT).abs() < 1.0
}

/// Validate persisted / candidate main studio bounds.
pub fn validate_saved_studio_bounds(bounds: Bounds<Pixels>, cx: &App) -> Result<(), &'static str> {
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    if !w.is_finite() || !h.is_finite() || !ox.is_finite() || !oy.is_finite() {
        return Err("non-finite");
    }
    if w < SAVED_MIN_WIDTH || h < SAVED_MIN_HEIGHT {
        return Err("too_small");
    }
    if is_unpositioned_spawn_snapshot(bounds) {
        return Err("unpositioned_spawn");
    }
    let mut visible_on_display = false;
    for display in cx.displays() {
        let work = display.visible_bounds();
        let (iw, ih) = intersection_size(bounds, work);
        if iw >= MIN_WORK_AREA_INTERSECT && ih >= MIN_WORK_AREA_INTERSECT {
            visible_on_display = true;
            break;
        }
    }
    if !visible_on_display {
        return Err("off_screen");
    }
    Ok(())
}

/// Clamp `bounds` inside the work area of the monitor containing its center (else primary).
pub fn clamp_bounds_to_work_area(window_bounds: Bounds<Pixels>, cx: &App) -> Bounds<Pixels> {
    let work = monitor_work_area(Some(window_bounds), cx);
    fit_bounds_in_work_area(window_bounds, work)
}

/// `window` moved — and, when it is larger than `work`, shrunk — so all of it
/// lies inside `work`.
///
/// A size saved on a larger monitor is shrunk to this one rather than placed
/// with its right and bottom edges, and the resize corner with them, off
/// screen. The usual 8 px margin is kept on any side the window leaves room
/// for, and given up rather than pushing the window past the opposite edge.
fn fit_bounds_in_work_area(window: Bounds<Pixels>, work: Bounds<Pixels>) -> Bounds<Pixels> {
    const MARGIN: f32 = 8.0;
    let work_x: f32 = work.origin.x.into();
    let work_y: f32 = work.origin.y.into();
    let work_w: f32 = work.size.width.into();
    let work_h: f32 = work.size.height.into();
    if !(work_w > 0.0 && work_h > 0.0) {
        return window;
    }
    let w = f32::from(window.size.width).min(work_w);
    let h = f32::from(window.size.height).min(work_h);
    let margin_x = MARGIN.min((work_w - w) / 2.0);
    let margin_y = MARGIN.min((work_h - h) / 2.0);
    let x = f32::from(window.origin.x)
        .min(work_x + work_w - w - margin_x)
        .max(work_x + margin_x);
    let y = f32::from(window.origin.y)
        .min(work_y + work_h - h - margin_y)
        .max(work_y + margin_y);
    gpui::bounds(point(px(x), px(y)), size(px(w), px(h)))
}

/// Read saved studio bounds from `path`; `None` if missing or unparsable.
fn read_saved_bounds_file(path: &std::path::Path) -> Option<Bounds<Pixels>> {
    let content = fs::read_to_string(path).ok()?;
    let saved: SavedStudioWindowBounds = serde_json::from_str(&content).ok()?;
    Some(saved.to_bounds())
}

/// Write `bounds` to `path` atomically (temp file in the same folder, synced,
/// then renamed over it): an interrupted save keeps the previous bounds
/// instead of leaving a torn file the next launch has to ignore.
fn write_saved_bounds_file(path: &std::path::Path, bounds: Bounds<Pixels>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&SavedStudioWindowBounds::from(bounds))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::keymap::storage::write_file_atomically(path, json.as_bytes())
}

fn load_saved_studio_bounds() -> Option<Bounds<Pixels>> {
    let path = studio_window_file();
    let bounds = read_saved_bounds_file(&path)?;
    log_position(&format!(
        "main_window: loaded saved bounds={} from {}",
        format_bounds(bounds),
        path.display()
    ));
    Some(bounds)
}

/// Initial main studio bounds: restored + clamped, or centered on primary work area.
pub fn studio_window_initial_bounds(cx: &mut App) -> Bounds<Pixels> {
    let requested = size(px(STUDIO_WINDOW_WIDTH), px(STUDIO_WINDOW_HEIGHT));
    if let Some(saved) = load_saved_studio_bounds() {
        match validate_saved_studio_bounds(saved, cx) {
            Ok(()) => {
                let clamped = clamp_bounds_to_work_area(saved, cx);
                log_position(&format!(
                    "main_window: using saved bounds={} clamped={} platform={}",
                    format_bounds(saved),
                    format_bounds(clamped),
                    platform_tag()
                ));
                STUDIO_PLACEMENT_CONFIRMED.store(true, Ordering::Relaxed);
                return clamped;
            }
            Err(reason) => {
                log_position(&format!(
                    "main_window: saved INVALID ({reason}) bounds={} platform={}",
                    format_bounds(saved),
                    platform_tag()
                ));
            }
        }
    } else {
        log_position(&format!(
            "main_window: no saved bounds file platform={}",
            platform_tag()
        ));
    }

    let centered = centered_window_bounds(None, requested, cx);
    log_position(&format!(
        "main_window: centered initial bounds={} platform={}",
        format_bounds(centered),
        platform_tag()
    ));
    STUDIO_PLACEMENT_CONFIRMED.store(true, Ordering::Relaxed);
    centered
}

/// [`WindowOptions`] for the main Futureboard Studio workspace window.
pub fn studio_window_options(cx: &mut App) -> WindowOptions {
    let bounds = studio_window_initial_bounds(cx);
    let work = monitor_work_area(Some(bounds), cx);

    log_position(&format!(
        "main_window: create platform={} work={} final={}",
        platform_tag(),
        format_bounds(work),
        format_bounds(bounds)
    ));

    let mut options = platform_chrome::studio_window_options();
    platform_chrome::apply_studio_window_style(&mut options);
    options.window_bounds = Some(WindowBounds::Windowed(bounds));
    apply_owner_display(&mut options, Some(bounds), cx);
    options
}

/// Persist normal (non-maximized) studio window bounds after the window is placed.
pub fn save_studio_window_bounds(bounds: Bounds<Pixels>, cx: &App) {
    if let Err(reason) = validate_saved_studio_bounds(bounds, cx) {
        log_position(&format!(
            "main_window: skip save ({reason}) bounds={}",
            format_bounds(bounds)
        ));
        return;
    }
    if is_unpositioned_spawn_snapshot(bounds) && !STUDIO_PLACEMENT_CONFIRMED.load(Ordering::Relaxed)
    {
        log_position(&format!(
            "main_window: skip save (placement not confirmed) bounds={}",
            format_bounds(bounds)
        ));
        return;
    }

    let path = studio_window_file();
    match write_saved_bounds_file(&path, bounds) {
        Ok(()) => log_position(&format!(
            "main_window: saved bounds={} path={}",
            format_bounds(bounds),
            path.display()
        )),
        Err(err) => log_position(&format!("main_window: save failed: {err}")),
    }
}

/// Read the platform window state and persist normal restore bounds when appropriate.
pub fn persist_studio_window_from_window(window: &Window, cx: &App) {
    let bounds = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => bounds,
        WindowBounds::Maximized(restore) | WindowBounds::Fullscreen(restore) => {
            log_position(&format!(
                "main_window: save restore bounds from maximized/fullscreen restore={}",
                format_bounds(restore)
            ));
            restore
        }
    };
    save_studio_window_bounds(bounds, cx);
}

/// Mark that the studio window has received a real placement (move/resize).
pub fn confirm_studio_window_placement() {
    STUDIO_PLACEMENT_CONFIRMED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_bounds_round_trip_and_replace_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "fb-studio-window-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("studio_window.json");
        let first = gpui::bounds(point(px(10.0), px(20.0)), size(px(1200.0), px(800.0)));
        let second = gpui::bounds(point(px(-5.0), px(40.0)), size(px(900.0), px(600.0)));
        write_saved_bounds_file(&path, first).unwrap();
        assert_eq!(read_saved_bounds_file(&path), Some(first));
        write_saved_bounds_file(&path, second).unwrap();
        assert_eq!(read_saved_bounds_file(&path), Some(second));
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("studio_window.json")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn centered_over_parent_math() {
        let parent = bounds(point(px(100.0), px(50.0)), size(px(1000.0), px(800.0)));
        let size = size(px(780.0), px(560.0));
        let req_w: f32 = size.width.into();
        let req_h: f32 = size.height.into();
        let px: f32 = parent.origin.x.into();
        let py: f32 = parent.origin.y.into();
        let pw: f32 = parent.size.width.into();
        let ph: f32 = parent.size.height.into();
        let x = px + (pw - req_w) / 2.0;
        let y = py + (ph - req_h) / 2.0;
        assert!((x - 210.0).abs() < 0.1);
        assert!((y - 170.0).abs() < 0.1);
    }

    #[test]
    fn rejects_tiny_owner() {
        let tiny = bounds(point(px(0.0), px(0.0)), size(px(1.0), px(1.0)));
        assert!(!is_valid_owner_bounds(tiny));
        let ok = bounds(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        assert!(is_valid_owner_bounds(ok));
    }

    #[test]
    fn detects_unpositioned_spawn_snapshot() {
        let spawn = bounds(
            point(px(0.0), px(0.0)),
            size(px(STUDIO_WINDOW_WIDTH), px(STUDIO_WINDOW_HEIGHT)),
        );
        assert!(is_unpositioned_spawn_snapshot(spawn));
        let moved = bounds(
            point(px(120.0), px(80.0)),
            size(px(STUDIO_WINDOW_WIDTH), px(STUDIO_WINDOW_HEIGHT)),
        );
        assert!(!is_unpositioned_spawn_snapshot(moved));
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        bounds(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn parts(b: Bounds<Pixels>) -> (f32, f32, f32, f32) {
        (
            b.origin.x.into(),
            b.origin.y.into(),
            b.size.width.into(),
            b.size.height.into(),
        )
    }

    /// A size saved on a 2560 px monitor restored on a 1920x1080 one fits the
    /// work area instead of hanging off its right and bottom edges.
    #[test]
    fn a_window_larger_than_the_work_area_is_shrunk_to_fit() {
        let work = rect(0.0, 0.0, 1920.0, 1040.0);
        let fitted = fit_bounds_in_work_area(rect(0.0, 0.0, 2560.0, 1400.0), work);
        assert_eq!(parts(fitted), (0.0, 0.0, 1920.0, 1040.0));

        // Wider only: the height and its margin are kept.
        let fitted = fit_bounds_in_work_area(rect(300.0, 100.0, 2400.0, 800.0), work);
        assert_eq!(parts(fitted), (0.0, 100.0, 1920.0, 800.0));

        // A work area that does not start at the origin (a second monitor, a
        // taskbar on the left).
        let work = rect(1920.0, 40.0, 1280.0, 984.0);
        let fitted = fit_bounds_in_work_area(rect(1900.0, 0.0, 1400.0, 1000.0), work);
        assert_eq!(parts(fitted), (1920.0, 40.0, 1280.0, 984.0));
    }

    #[test]
    fn a_window_that_fits_keeps_its_size_and_only_moves_inside() {
        let work = rect(0.0, 0.0, 1920.0, 1040.0);
        let inside = rect(100.0, 80.0, 1400.0, 900.0);
        assert_eq!(parts(fit_bounds_in_work_area(inside, work)), parts(inside));

        let overhanging = rect(800.0, 500.0, 1400.0, 900.0);
        assert_eq!(
            parts(fit_bounds_in_work_area(overhanging, work)),
            (512.0, 132.0, 1400.0, 900.0)
        );

        let above_left = rect(-300.0, -50.0, 1400.0, 900.0);
        assert_eq!(
            parts(fit_bounds_in_work_area(above_left, work)),
            (8.0, 8.0, 1400.0, 900.0)
        );
    }

    #[test]
    fn rects_intersect_math() {
        let a = bounds(point(px(0.0), px(0.0)), size(px(100.0), px(100.0)));
        let b = bounds(point(px(50.0), px(50.0)), size(px(100.0), px(100.0)));
        assert!(rects_intersect(a, b));
        let c = bounds(point(px(200.0), px(0.0)), size(px(50.0), px(50.0)));
        assert!(!rects_intersect(a, c));
    }
}
