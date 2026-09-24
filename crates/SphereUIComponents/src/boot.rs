//! Startup diagnostics and interrupted-startup recovery.
//!
//! A per-process log is created under `%LOCALAPPDATA%/Futureboard/Logs` before
//! GPUI or CEF starts. Each line is flushed so a forced close still leaves the
//! last completed startup stage on disk. `FUTUREBOARD_BOOT_DEBUG=1` also mirrors
//! the compact boot milestones to stderr; `--gpu-diagnostics` enables detailed
//! `log` records from the Windows renderer and CEF host.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct BootState {
    started: Instant,
    file: Mutex<File>,
    log_path: PathBuf,
    startup_marker: Option<PathBuf>,
    safe_graphics_mode: bool,
    startup_completed: AtomicBool,
}

static STATE: OnceLock<BootState> = OnceLock::new();
static FILE_LOGGER: FileLogger = FileLogger;

struct FileLogger;

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        let target = metadata.target().to_ascii_lowercase();
        let gpu_or_cef = target.contains("gpui_windows")
            || target.contains("directx")
            || target.contains("dxgi")
            || target.contains("sphere_webview")
            || target.contains("cef");
        gpu_or_cef
            && metadata.level()
                <= if diagnostics_enabled() {
                    log::Level::Debug
                } else {
                    log::Level::Info
                }
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let category = if record.target().contains("cef")
            || record.target().contains("sphere_webview")
            || record.target().contains("SphereWebView")
        {
            "CEF"
        } else if record.target().contains("gpui_windows")
            || record.target().contains("directx")
            || record.target().contains("dxgi")
        {
            "GPU"
        } else {
            "LOG"
        };
        if let Some(state) = STATE.get() {
            let elapsed = state.started.elapsed().as_millis();
            let line = format!(
                "[{category} +{elapsed:06}ms][T:{:?}][PID:{}][{}] {}: {}",
                std::thread::current().id(),
                std::process::id(),
                record.level(),
                record.target(),
                record.args()
            );
            write_line(state, &line);
        }
    }

    fn flush(&self) {
        if let Some(state) = STATE.get() {
            if let Ok(mut file) = state.file.lock() {
                let _ = file.flush();
            }
        }
    }
}

/// Initialize file logging before dispatching CEF helper processes or creating
/// the native GPUI platform. The browser process also records an interrupted
/// startup marker; helper processes only get their own log file.
pub fn init_process() {
    if STATE.get().is_some() {
        return;
    }

    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    let subprocess = args.iter().any(|arg| {
        arg.eq_ignore_ascii_case("--type") || arg.to_ascii_lowercase().starts_with("--type=")
    });
    let log_root = log_root();
    let _ = fs::create_dir_all(&log_root);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let preferred_log_path =
        log_root.join(format!("boot-{timestamp}-pid-{}.log", std::process::id()));
    let fallback_log_path = std::env::temp_dir().join(format!(
        "futureboard-boot-{timestamp}-pid-{}.log",
        std::process::id()
    ));
    let (log_path, file) = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&preferred_log_path)
    {
        Ok(file) => (preferred_log_path, file),
        Err(_) => match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fallback_log_path)
        {
            Ok(file) => (fallback_log_path, file),
            Err(_) => {
                eprintln!(
                    "[BOOT] Could not open a startup log under {} or {}",
                    preferred_log_path.display(),
                    fallback_log_path.display()
                );
                return;
            }
        },
    };

    let startup_marker = (!subprocess).then(|| log_root.join("startup-in-progress.marker"));
    let interrupted_startup = startup_marker
        .as_ref()
        .is_some_and(|marker| marker.exists());
    let safe_graphics_mode = safe_graphics_mode_for(subprocess, interrupted_startup);
    let _ = STATE.set(BootState {
        started: Instant::now(),
        file: Mutex::new(file),
        log_path: log_path.clone(),
        startup_marker: startup_marker.clone(),
        safe_graphics_mode,
        startup_completed: AtomicBool::new(false),
    });

    let max_level = if diagnostics_enabled() {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    let _ = log::set_logger(&FILE_LOGGER);
    log::set_max_level(max_level);

    if let Some(marker) = startup_marker {
        if safe_graphics_mode {
            std::env::set_var("FUTUREBOARD_SAFE_GRAPHICS_MODE", "1");
            std::env::set_var("FUTUREBOARD_FORCE_WARP", "1");
            std::env::set_var("FUTUREBOARD_DISABLE_CEF_GPU", "1");
            std::env::set_var("FUTUREBOARD_DISABLE_CEF_WARMUP", "1");
            std::env::set_var("FUTUREBOARD_DISABLE_SHARED_TEXTURE", "1");
            log(
                "previous startup did not reach its first stable app frame; safe graphics mode enabled with WARP and CEF acceleration disabled",
            );
        }
        if let Err(error) = File::create(&marker) {
            log(&format!(
                "could not write startup marker {}: {error}",
                marker.display()
            ));
        } else {
            log(&format!("startup marker active: {}", marker.display()));
        }
    }

    eprintln!("[BOOT] diagnostics log: {}", log_path.display());
    log(&format!(
        "process entry role={} pid={} args={:?}",
        if subprocess {
            "cef-subprocess"
        } else {
            "browser"
        },
        std::process::id(),
        args
    ));
    log("FutureboardNative start");
}

fn safe_graphics_mode_for(subprocess: bool, interrupted_startup: bool) -> bool {
    !subprocess && interrupted_startup
}

fn log_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Futureboard")
        .join("Logs")
}

/// Exact command-line flag check shared by startup and the CEF host.
pub fn has_flag(flag: &str) -> bool {
    std::env::args_os()
        .skip(1)
        .any(|arg| arg.to_string_lossy().eq_ignore_ascii_case(flag))
}

/// Whether verbose GPU/CEF diagnostics were requested.
pub fn diagnostics_enabled() -> bool {
    has_flag("--gpu-diagnostics")
        || std::env::var_os("FUTUREBOARD_GPU_DIAGNOSTICS").is_some()
        || std::env::var_os("FUTUREBOARD_BOOT_DEBUG").is_some()
}

/// Whether a previous process exited before it reached a stable Welcome or
/// Studio frame. This recovery mode disables optional CEF acceleration; the
/// DirectX starts on WARP for recovery; the first stable app frame clears the
/// marker so the next launch returns to the normal hardware path.
pub fn safe_graphics_mode() -> bool {
    STATE.get().is_some_and(|state| state.safe_graphics_mode)
}

/// Log a startup milestone using a monotonic clock and flush it immediately.
pub fn log(msg: &str) {
    if let Some(state) = STATE.get() {
        let elapsed = state.started.elapsed().as_millis();
        let line = format!(
            "[BOOT +{elapsed:06}ms][T:{:?}][PID:{}] {msg}",
            std::thread::current().id(),
            std::process::id()
        );
        if std::env::var_os("FUTUREBOARD_BOOT_DEBUG").is_some() {
            eprintln!("{line}");
        }
        write_line(state, &line);
    } else if std::env::var_os("FUTUREBOARD_BOOT_DEBUG").is_some() {
        eprintln!(
            "[BOOT +000000ms][T:{:?}][PID:{}] {msg}",
            std::thread::current().id(),
            std::process::id()
        );
    }
}

fn write_line(state: &BootState, line: &str) {
    if let Ok(mut file) = state.file.lock() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// Clear the recovery marker after the first real application surface has
/// painted. Idempotent in case both splash handoff and a main window race here.
pub fn complete_startup() {
    let Some(state) = STATE.get() else { return };
    if state.startup_completed.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(marker) = &state.startup_marker {
        if let Err(error) = fs::remove_file(marker) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log(&format!(
                    "could not clear startup marker {}: {error}",
                    marker.display()
                ));
            }
        }
    }
    log(&format!(
        "first stable application frame; startup complete log={}",
        state.log_path.display()
    ));
}

#[cfg(test)]
mod tests {
    use super::safe_graphics_mode_for;

    #[test]
    fn interrupted_browser_startup_enables_safe_graphics_mode() {
        assert!(safe_graphics_mode_for(false, true));
    }

    #[test]
    fn first_launch_and_cef_helpers_do_not_enable_safe_graphics_mode() {
        assert!(!safe_graphics_mode_for(false, false));
        assert!(!safe_graphics_mode_for(true, true));
    }
}
