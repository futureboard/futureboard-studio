//! Application startup state machine and lightweight boot tasks.
//!
//! Splash covers early boot only — no VST scanning, no project restore I/O.
//! Heavy session loads use [`crate::loading_session`] later.

use std::path::PathBuf;
use std::time::Duration;

use gpui::AsyncApp;

use crate::paths::FutureboardPaths;
use crate::project::RecentProjectsStore;
use crate::settings::SettingsSchema;

/// Where the app should route once splash boot completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupRoute {
    Welcome,
    /// Blank unsaved studio workspace (start screen disabled, no restore target).
    EmptyWorkspace,
    OpenProject(PathBuf),
    RestoreLastProject(PathBuf),
}

/// Coarse startup phases for logging and future splash status hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupPhase {
    Starting,
    LoadingConfig,
    LoadingTheme,
    PreparingUserData,
    ResolvingStartupRoute,
    OpeningWelcome,
    OpeningStudio,
    Done,
}

impl StartupPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::LoadingConfig => "Loading configuration",
            Self::LoadingTheme => "Loading theme",
            Self::PreparingUserData => "Preparing user data",
            Self::ResolvingStartupRoute => "Resolving startup route",
            Self::OpeningWelcome => "Opening welcome",
            Self::OpeningStudio => "Opening studio",
            Self::Done => "Done",
        }
    }
}

/// Resolved boot destination plus whether the Welcome start screen is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupPlan {
    pub route: StartupRoute,
    /// When false the app skips Welcome and opens an empty studio workspace.
    pub show_welcome_screen: bool,
}

impl StartupPlan {
    pub fn resolve() -> Self {
        let schema = SettingsSchema::load_from_disk();
        let show_welcome_screen = schema.general.show_start_screen;
        // A project handed over on the command line — the shell's file
        // association (`FutureboardNative.exe "%1"`), a drag onto the
        // executable, or a terminal — always wins over the saved start route:
        // the user asked for *that* file, not for Welcome or the last session.
        let route = if let Some(path) = project_path_from_args(std::env::args_os().skip(1)) {
            StartupRoute::OpenProject(path)
        } else if show_welcome_screen {
            StartupRoute::Welcome
        } else if let Some(path) = restore_last_project_candidate() {
            StartupRoute::RestoreLastProject(path)
        } else {
            StartupRoute::EmptyWorkspace
        };
        Self {
            route,
            show_welcome_screen,
        }
    }
}

/// The first argument that names an existing Futureboard project file, if
/// any. Anything else on the command line (flags, a stray path to a WAV) is
/// ignored rather than treated as a project, so a bad association can never
/// send the app looking for a project inside a sample.
pub fn project_path_from_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Option<PathBuf> {
    args.into_iter().map(PathBuf::from).find(|path| {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("fbproj"))
            && path.is_file()
    })
}

fn restore_last_project_candidate() -> Option<PathBuf> {
    let mut recent = RecentProjectsStore::load();
    recent.refresh_missing();
    recent
        .entries()
        .iter()
        .find(|entry| !entry.missing)
        .map(|entry| entry.path.clone())
}

pub fn log_startup_phase(phase: StartupPhase) {
    crate::boot::log(&format!("startup phase: {}", phase.label()));
}

/// Result of the startup GPU enumeration.
#[derive(Debug, Clone)]
pub struct GpuProbe {
    /// Names of detected hardware GPUs (software/CPU adapters excluded).
    pub devices: Vec<String>,
    /// One-line status for the boot log. Deliberately not shown on the
    /// splash: the probe finishes after boot has moved on, so the line would
    /// either arrive too late to read or hold the splash open waiting for it.
    pub summary: String,
    pub has_gpu: bool,
}

/// Environment variable through which the saved GPU choice reaches the
/// platform renderer (D3D11 adapter selection on Windows, Metal device
/// selection on macOS). Format: `vendor=0x10de;device=0x2520;name=…`, or
/// `high-performance` / `low-power`. A value already set in the environment
/// wins, so the variable doubles as a manual override.
pub const GPU_ADAPTER_ENV: &str = "FUTUREBOARD_GPU_ADAPTER";

/// Hand the Preferences → Performance → GPU Device choice to the platform
/// renderer. Must run in `main`, before `application()`: GPUI creates its
/// D3D11/Metal device while the platform is constructed, long before the
/// settings model exists.
///
/// Reads `settings.json` directly and only the one field — no defaults are
/// written, no backup is made, nothing is created — because this runs before
/// logging, the crash reporter and the settings model are up.
pub fn export_gpu_adapter_preference() {
    if std::env::var_os(GPU_ADAPTER_ENV).is_some() {
        return;
    }
    let path = FutureboardPaths::resolve().settings_file;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(device) = json.pointer("/performance/gpu_device").and_then(|value| {
        serde_json::from_value::<crate::settings::GpuDevicePreference>(value.clone()).ok()
    }) else {
        return;
    };
    if let Some(value) = gpu_adapter_env_value(&device) {
        std::env::set_var(GPU_ADAPTER_ENV, &value);
        crate::boot::log(&format!("GPU adapter preference {GPU_ADAPTER_ENV}={value}"));
    }
}

/// The saved preference as the platform renderer reads it. `None` for Auto:
/// the OS default adapter, exactly as before.
///
/// The saved id is wgpu's `"{backend}:{vendor:x}:{device:x}:{name}"`. The PCI
/// vendor/device pair is what DXGI reports too, so Windows matches on it; the
/// name is what Metal reports, and wgpu gives Metal devices a zero vendor.
pub fn gpu_adapter_env_value(preference: &crate::settings::GpuDevicePreference) -> Option<String> {
    let crate::settings::GpuDevicePreference::DeviceId(id) = preference else {
        return None;
    };
    let mut parts = id.splitn(4, ':');
    let _backend = parts.next()?;
    let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = u32::from_str_radix(parts.next()?, 16).ok()?;
    let name = parts.next().unwrap_or("").replace(';', " ");
    Some(format!(
        "vendor=0x{vendor:04x};device=0x{device:04x};name={name}"
    ))
}

/// Enumerate GPU adapters (wgpu) and record availability for the audio stack so
/// stem extraction can automatically prefer GPU inference. Safe to call without
/// the `gpu-renderer` feature (returns "no GPU"). Never panics — enumeration is
/// already `catch_unwind`-guarded.
///
/// Call this off the main thread. Enumeration constructs a Vulkan/DX12/GL
/// instance and can take seconds; nothing in boot depends on the answer, so it
/// must never sit in front of a window that is trying to paint.
pub fn probe_gpus() -> GpuProbe {
    let devices: Vec<String> = crate::components::timeline::render::list_available_gpu_devices()
        .into_iter()
        // wgpu reports software/WARP/llvmpipe fallbacks as `Cpu`; exclude those.
        .filter(|d| d.device_type.as_deref() != Some("Cpu"))
        .map(|d| d.name)
        .collect();
    let has_gpu = !devices.is_empty();

    // Authoritative signal for `SphereAudioProcessor::gpu_available()`.
    SphereAudioProcessor::set_gpu_detected(has_gpu);

    let summary = if has_gpu {
        format!("GPU: {}", devices.join(", "))
    } else {
        "GPU: none detected — using CPU".to_string()
    };
    crate::boot::log(&format!("startup GPU probe: {summary}"));
    GpuProbe {
        devices,
        summary,
        has_gpu,
    }
}

/// Lightweight boot work shared by Welcome and direct-to-studio launches.
/// Runs while the splash window is visible.
pub async fn run_lightweight_boot(cx: &mut AsyncApp) -> StartupPlan {
    let executor = cx.background_executor().clone();

    log_startup_phase(StartupPhase::Starting);
    executor.timer(Duration::from_millis(1)).await;

    log_startup_phase(StartupPhase::LoadingConfig);
    let _schema = SettingsSchema::load_from_disk();
    executor.timer(Duration::from_millis(40)).await;

    log_startup_phase(StartupPhase::LoadingTheme);
    executor.timer(Duration::from_millis(40)).await;

    log_startup_phase(StartupPhase::PreparingUserData);
    let paths = FutureboardPaths::resolve();
    let _ = paths.ensure_user_dirs();
    let mut recent = RecentProjectsStore::load();
    recent.refresh_missing();
    executor.timer(Duration::from_millis(40)).await;

    log_startup_phase(StartupPhase::ResolvingStartupRoute);
    let plan = StartupPlan::resolve();
    executor.timer(Duration::from_millis(40)).await;

    // CoreAudio device enumeration can block inside Apple's HAL while another
    // application owns an audio session (common during remote desktop or DAW
    // use). It is not needed to choose the first Studio surface, and a blocked
    // background worker can otherwise starve the pre-studio engine install.
    // Keep the eager scan on platforms where the native enumeration is
    // reliable; macOS refreshes the cache when audio settings are opened.
    #[cfg(not(target_os = "macos"))]
    {
        crate::boot::log("[Startup] phase=ScanAudio");
        executor
            .spawn(async {
                crate::device_registry::scan_audio();
            })
            .detach();
    }

    crate::boot::log("[Startup] phase=ScanMidi");
    executor
        .spawn(async {
            crate::device_registry::scan_midi_resilient();
        })
        .detach();
    if crate::device_registry::cached_midi_devices().is_empty() {
        // USB class drivers and platform MIDI services can appear after the
        // splash scan has completed. Retry once after startup without delaying
        // Welcome/Studio; the regular hardware sync will consume the new cache.
        let retry_timer = executor.clone();
        executor
            .spawn(async move {
                retry_timer.timer(Duration::from_secs(2)).await;
                crate::device_registry::scan_midi_resilient();
            })
            .detach();
    }

    executor.timer(Duration::from_millis(80)).await;
    cx.update(|_app| {
        let warm = crate::layout::warm_up_renderer_status();
        crate::boot::log(&format!(
            "renderer warm-up: {} [{}]",
            warm.status_text(),
            warm.backend_label
        ));
    });

    log_startup_phase(StartupPhase::Done);
    plan
}

#[cfg(test)]
mod tests {
    use super::project_path_from_args;
    use std::ffi::OsString;

    #[test]
    fn only_an_existing_fbproj_argument_becomes_the_open_route() {
        let dir =
            std::env::temp_dir().join(format!("futureboard-startup-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let project = dir.join("Song.FBPROJ");
        std::fs::write(&project, b"not a real project").expect("write");
        let sample = dir.join("kick.wav");
        std::fs::write(&sample, b"riff").expect("write");

        let args = |items: &[&std::path::Path]| -> Vec<OsString> {
            items.iter().map(|p| p.as_os_str().to_owned()).collect()
        };

        // Flags and non-project files are skipped; case of the extension is
        // irrelevant (Explorer preserves whatever the user typed).
        assert_eq!(
            project_path_from_args(args(&[
                std::path::Path::new("--verbose"),
                &sample,
                &project
            ])),
            Some(project.clone())
        );
        // A project path that does not exist is not a route: the shell would
        // otherwise open a blank Studio and report a missing file.
        assert_eq!(
            project_path_from_args(args(&[&dir.join("missing.fbproj")])),
            None
        );
        assert_eq!(project_path_from_args(Vec::<OsString>::new()), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gpu_adapter_env_tests {
    use super::*;
    use crate::settings::GpuDevicePreference;

    #[test]
    fn auto_leaves_the_os_default_alone() {
        assert_eq!(gpu_adapter_env_value(&GpuDevicePreference::Auto), None);
    }

    #[test]
    fn a_saved_device_becomes_vendor_device_and_name() {
        let value = gpu_adapter_env_value(&GpuDevicePreference::DeviceId(
            "Dx12:10de:2520:NVIDIA GeForce RTX 3060 Laptop GPU".to_string(),
        ));
        assert_eq!(
            value.as_deref(),
            Some("vendor=0x10de;device=0x2520;name=NVIDIA GeForce RTX 3060 Laptop GPU")
        );
    }

    #[test]
    fn a_malformed_id_is_ignored_rather_than_guessed() {
        assert_eq!(
            gpu_adapter_env_value(&GpuDevicePreference::DeviceId("garbage".into())),
            None
        );
    }
}
