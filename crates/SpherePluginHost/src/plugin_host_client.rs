//! Main-app side of the separated plugin-host IPC: spawns
//! `FutureboardPluginHostX64.exe`, sends [`HostCommand`]s on its stdin, and
//! delivers [`HostEvent`]s (plus a synthetic disconnect signal) over a
//! `crossbeam-channel` so the GPUI UI thread can poll without ever blocking
//! (spec Part 9).
//!
//! Slice 1: this client is only constructed when
//! `FUTUREBOARD_PLUGIN_EDITOR_OWNERSHIP=host_process`. The default
//! (`main_owned`, the current in-process path) does not touch this module, so
//! existing behavior is unchanged. Wiring the GPUI editor window's content
//! child HWND through `open_editor` is Slice 2.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, TryRecvError};

use crate::ipc::{self, HostCommand, HostEvent, PROTOCOL_VERSION};
use crate::plugin_host_lifecycle;
use crate::plugin_host_logging;
use crate::plugin_host_spawn_config::PluginHostSpawnConfig;
use crate::process_manager::PluginHostProcessManager;

/// What the UI thread receives from [`PluginHostClient::try_recv_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// A typed event from the host process.
    Host(HostEvent),
    /// The host's stdout pipe closed — the process exited or crashed. The main
    /// app should mark any open editors on this host as offline (spec Part 7).
    Disconnected,
}

/// Errors spawning / locating the host binary.
#[derive(Debug)]
pub enum PluginHostClientError {
    BinaryMissing(String),
    Spawn(std::io::Error),
}

impl std::fmt::Display for PluginHostClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BinaryMissing(p) => write!(f, "plugin host binary not found: {p}"),
            Self::Spawn(e) => write!(f, "failed to spawn plugin host: {e}"),
        }
    }
}

impl std::error::Error for PluginHostClientError {}

const BINARY_STEM: &str = "FutureboardPluginHostX64";

/// Strip main-app-only renderer/compositor environment from the plugin-host
/// child process before spawning it.
///
/// The GPUI main app sets `GPUI_DISABLE_DIRECT_COMPOSITION=1` (and may set other
/// `GPUI_*` renderer flags) for its *own* swap-chain / DirectComposition needs.
/// Those flags are meaningless — and potentially harmful — inside the separate
/// `FutureboardPluginHostX64.exe`, which hosts arbitrary plugin GPU / WebView /
/// DirectComposition UI frameworks. Inheriting them can leave the plugin editor
/// blank. The host must run with a clean native environment by default (spec
/// Part 1/2); a PluginHost-specific opt-in
/// (`FUTUREBOARD_PLUGIN_HOST_DISABLE_DIRECT_COMPOSITION`) is the only supported
/// way to re-enable the workaround for the host, and it never reuses the GPUI
/// flag.
fn sanitize_child_env(command: &mut Command) {
    // Remove every `GPUI_*` variable: these are scoped to the main GPUI process
    // only. Snapshot the current process env so we drop whatever it actually
    // inherited, not just a hard-coded list.
    for (key, _) in std::env::vars() {
        if key.starts_with("GPUI_") {
            command.env_remove(&key);
            eprintln!("[plugin-bridge] child_env_remove {key}");
        }
    }
    // Belt-and-braces: the headline offender, even if the loop above missed it.
    command.env_remove("GPUI_DISABLE_DIRECT_COMPOSITION");

    // Tag the child's role so host-side diagnostics / future behavior can branch
    // on it without sniffing GPUI flags.
    for key in [
        "WGPU_BACKEND",
        "LIBGL_ALWAYS_SOFTWARE",
        "DXGI_PRESENT_ALLOW_TEARING",
    ] {
        command.env_remove(key);
    }
    command.env("FUTUREBOARD_PROCESS_ROLE", "plugin_host");
    // Editor window ownership (single source of truth for both processes):
    //   host-owned (default) -> "detached": the host owns a standalone top-level
    //     editor window. The plugin view is parented under a HOST-thread window,
    //     never under the GPUI main thread, so a slow/hanging editor cannot
    //     freeze the app. This is the fix for the cross-process input-queue
    //     coupling.
    //   legacy main-owned -> "child": the old WS_CHILD-into-main-content-HWND
    //     embedding (kept only behind FUTUREBOARD_PLUGIN_EDITOR_MAIN_OWNED_SHELL).
    let editor_mode = if editor_main_owned_shell_enabled() {
        "child"
    } else {
        "detached"
    };
    command.env("FUTUREBOARD_PLUGIN_EDITOR_MODE", editor_mode);
    eprintln!("[plugin-bridge] child_env_set FUTUREBOARD_PROCESS_ROLE=plugin_host");
    eprintln!("[plugin-bridge] child_env_set FUTUREBOARD_PLUGIN_EDITOR_MODE={editor_mode}");

    // Linux VST3 editors need an X11/XEmbed parent (GTK4 + GDK X11 backend).
    // The GPUI studio may run under pure Wayland (`env -u DISPLAY`), but the
    // plugin-host child must still reach an X11 display (often the outer nested
    // compositor's X server, or XWayland). Force GDK onto X11 and restore a
    // DISPLAY when the parent cleared it.
    #[cfg(target_os = "linux")]
    {
        configure_linux_plugin_host_display(command);
    }

    eprintln!("[plugin-host-env] sanitized=true");
}

/// Ensure the Linux plugin-host child can open GTK4/X11 VST3 editors.
#[cfg(target_os = "linux")]
fn configure_linux_plugin_host_display(command: &mut Command) {
    command.env("GDK_BACKEND", "x11");
    eprintln!("[plugin-bridge] child_env_set GDK_BACKEND=x11");

    // Prefer the parent's DISPLAY when present; otherwise pick the first live
    // X11 socket under /tmp/.X11-unix (e.g. X1 → :1) so a Wayland-only GPUI
    // process can still spawn an X11 editor host.
    let display = std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.trim().is_empty());
    let display = display.or_else(discover_x11_display);
    match display {
        Some(d) => {
            command.env("DISPLAY", &d);
            eprintln!("[plugin-bridge] child_env_set DISPLAY={d}");
        }
        None => {
            eprintln!(
                "[plugin-bridge] WARNING no DISPLAY for plugin host; VST3 X11 editors will fail"
            );
        }
    }

    // Keep GTK from preferring Wayland when both sockets are visible.
    command.env_remove("WAYLAND_DISPLAY");
    eprintln!("[plugin-bridge] child_env_remove WAYLAND_DISPLAY");
}

#[cfg(target_os = "linux")]
fn discover_x11_display() -> Option<String> {
    let dir = std::fs::read_dir("/tmp/.X11-unix").ok()?;
    let mut sockets: Vec<u32> = dir
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let num = name.strip_prefix('X')?.parse::<u32>().ok()?;
            Some(num)
        })
        .collect();
    sockets.sort_unstable();
    sockets.first().map(|n| format!(":{n}"))
}

fn binary_name(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        dir.join(format!("{BINARY_STEM}.exe"))
    }
    #[cfg(not(windows))]
    {
        dir.join(BINARY_STEM)
    }
}

/// Truthy env values accepted for boolean flags: `1`, `true`, `yes`, `on`
/// (case-insensitive).
fn env_is_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Whether the emergency legacy in-process VST3 runtime/editor path is enabled.
/// This is intentionally opt-in only; the external bridge is mandatory by
/// default.
pub fn legacy_in_process_enabled() -> bool {
    env_is_truthy("FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS")
        || std::env::var("FUTUREBOARD_VST3_EDITOR_BACKEND")
            .map(|v| v.trim().eq_ignore_ascii_case("rust_legacy"))
            .unwrap_or(false)
}

pub fn vst3_editor_backend_disabled() -> bool {
    std::env::var("FUTUREBOARD_VST3_EDITOR_BACKEND")
        .map(|v| v.trim().eq_ignore_ascii_case("disabled"))
        .unwrap_or(false)
}

/// Whether the external plugin-host bridge is active. This is the default and
/// must not depend on `FUTUREBOARD_PLUGIN_HOST_BRIDGE`; that flag is deprecated
/// and is ignored for backend selection.
pub fn plugin_host_bridge_enabled() -> bool {
    !legacy_in_process_enabled() && !vst3_editor_backend_disabled()
}

/// Legacy escape hatch: keep the old *main-owned* editor shell, where the main
/// app creates a Win32 content HWND and the host embeds the VST3 view into it
/// as a cross-process `WS_CHILD`. That parenting attaches the host UI thread's
/// input queue to the GPUI main thread, so a slow/hanging plugin editor (e.g. a
/// GPU/WebView editor mid-init) freezes the whole app ("Not responding").
///
/// Default is OFF: the bridge editor is **host-owned** — the host process owns
/// a detached top-level editor window and the main app owns no plugin window,
/// fully decoupling the GPUI main thread. Set
/// `FUTUREBOARD_PLUGIN_EDITOR_MAIN_OWNED_SHELL=1` only to restore the old
/// (coupled) behavior for comparison. Vendor-agnostic; no plugin branches.
pub fn editor_main_owned_shell_enabled() -> bool {
    env_is_truthy("FUTUREBOARD_PLUGIN_EDITOR_MAIN_OWNED_SHELL")
}

/// One-time boot diagnostics for plugin runtime selection. Call early in app
/// startup.
pub fn log_bridge_env() {
    let legacy = legacy_in_process_enabled();
    let requested_editor_backend =
        std::env::var("FUTUREBOARD_VST3_EDITOR_BACKEND").unwrap_or_else(|_| "cpp_shell".into());
    let disabled = vst3_editor_backend_disabled();
    eprintln!("[plugin-runtime] default_backend=external_bridge");
    eprintln!("[VST3Editor] backend={requested_editor_backend}");
    eprintln!("[plugin-runtime] legacy_override={legacy}");
    if let Ok(raw) = std::env::var("FUTUREBOARD_PLUGIN_HOST_BRIDGE") {
        eprintln!("[plugin-runtime] deprecated_env_ignored FUTUREBOARD_PLUGIN_HOST_BRIDGE={raw}");
    }
    if disabled {
        eprintln!(
            "[plugin-runtime] backend=disabled reason=FUTUREBOARD_VST3_EDITOR_BACKEND=disabled"
        );
    } else if legacy {
        eprintln!("[VST3Editor] backend=rust_legacy");
        eprintln!(
            "[plugin-runtime] backend=in_process reason=FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1"
        );
        eprintln!("[plugin-runtime] WARNING using legacy in-process plugin runtime");
        eprintln!("[plugin-runtime] legacy path may hang GPU/browser-backed plugin editors");
    } else {
        eprintln!("[VST3Editor] backend=cpp_shell");
        eprintln!("[plugin-runtime] backend=external_bridge reason=forced_default");
    }
}

/// Resolve the host executable and whether it exists on disk. Resolution order
/// (spec): `FUTUREBOARD_PLUGIN_HOST_EXE` → next to the running exe →
/// `target/{debug,release}`. `FUTUREBOARD_PLUGIN_HOST` is a legacy alias for the
/// explicit override. The returned path is logged by [`PluginHostClient::spawn_bridge`].
pub fn resolve_host_exe() -> (PathBuf, bool) {
    for var in ["FUTUREBOARD_PLUGIN_HOST_EXE", "FUTUREBOARD_PLUGIN_HOST"] {
        if let Ok(path) = std::env::var(var) {
            let path = PathBuf::from(path);
            let exists = path.is_file();
            return (path, exists);
        }
    }

    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{BINARY_STEM}")) {
        let path = PathBuf::from(path);
        let exists = path.is_file();
        return (path, exists);
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let candidate = binary_name(dir);
            if candidate.is_file() {
                return (candidate, true);
            }
            if dir.file_name().is_some_and(|name| name == "deps") {
                if let Some(profile_dir) = dir.parent() {
                    let candidate = binary_name(profile_dir);
                    if candidate.is_file() {
                        return (candidate, true);
                    }
                }
            }
        }
    }

    if let (Ok(target_dir), Ok(profile)) =
        (std::env::var("CARGO_TARGET_DIR"), std::env::var("PROFILE"))
    {
        let candidate = binary_name(&PathBuf::from(target_dir).join(profile));
        if candidate.is_file() {
            return (candidate, true);
        }
    }

    for profile in ["debug", "release"] {
        let candidate = binary_name(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/{profile}")),
        );
        if candidate.is_file() {
            return (candidate, true);
        }
    }

    (PathBuf::from(BINARY_STEM), false)
}

/// Resolve the host executable, erroring if it cannot be found. Used by tests.
pub fn locate_plugin_host_binary() -> Result<PathBuf, PluginHostClientError> {
    let (path, exists) = resolve_host_exe();
    if exists {
        Ok(path)
    } else {
        Err(PluginHostClientError::BinaryMissing(
            path.display().to_string(),
        ))
    }
}

/// A live connection to one `FutureboardPluginHostX64.exe` process.
///
/// Drop sends `Shutdown` (best-effort) and then kills the child, so a dropped
/// client never leaves an orphan host process.
pub struct PluginHostClient {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<ClientEvent>,
    reader: Option<JoinHandle<()>>,
    pub(crate) shutdown_started: bool,
}

impl PluginHostClient {
    /// Spawn the host process and start the background event reader.
    pub fn spawn() -> Result<Self, PluginHostClientError> {
        let binary = locate_plugin_host_binary()?;
        Self::spawn_from(&binary)
    }

    /// Resolve + spawn the bridge host with full `[plugin-bridge]` diagnostics.
    pub fn spawn_bridge() -> Result<Self, PluginHostClientError> {
        Self::spawn_bridge_with_config(&PluginHostSpawnConfig::default())
    }

    /// Spawn the bridge host with explicit session / main-window configuration.
    pub fn spawn_bridge_with_config(
        config: &PluginHostSpawnConfig,
    ) -> Result<Self, PluginHostClientError> {
        let current_exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        let (exe, exists) = resolve_host_exe();
        eprintln!("[plugin-bridge] current_exe={current_exe}");
        eprintln!("[plugin-bridge] resolved_host_exe={}", exe.display());
        eprintln!("[plugin-bridge] exists={exists}");
        if !exists {
            let err = PluginHostClientError::BinaryMissing(exe.display().to_string());
            eprintln!("[plugin-bridge] ERROR host exe not found; external bridge is mandatory");
            eprintln!("[plugin-bridge] spawn_failed error={err}");
            return Err(err);
        }
        eprintln!("[plugin-bridge] spawning {}", exe.display());
        match Self::spawn_from_config(&exe, config) {
            Ok(client) => {
                eprintln!("[plugin-bridge] spawned pid={}", client.pid());
                Ok(client)
            }
            Err(e) => {
                eprintln!("[plugin-bridge] spawn_failed error={e}");
                Err(e)
            }
        }
    }

    /// Spawn a specific host binary (used by tests).
    pub fn spawn_from(binary: &Path) -> Result<Self, PluginHostClientError> {
        Self::spawn_from_config(binary, &PluginHostSpawnConfig::default())
    }

    fn spawn_from_config(
        binary: &Path,
        config: &PluginHostSpawnConfig,
    ) -> Result<Self, PluginHostClientError> {
        eprintln!("[plugin-bridge] ipc=stdio");
        let parent_pid = std::process::id();
        let hidden = !plugin_host_logging::host_console_enabled();
        eprintln!(
            "[PluginHost] spawn hidden={hidden} path={}",
            binary.display()
        );
        // When hidden, the host's stderr would normally be discarded
        // (`Stdio::null()`) — which throws away every host-side editor
        // diagnostic (`[vst3-editor] …`, `[EDITOR …]`, the C++ fprintf lines).
        // If host-log forwarding is opted in, pipe it instead and forward it
        // inline to the parent's stderr below, so the host's editor lifecycle
        // shows up in the same out.log the user already captures — no console
        // window. `inherit` (non-hidden) is unchanged.
        let forward_host_stderr = hidden && plugin_host_logging::host_stderr_forward_enabled();
        let mut command = Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if !hidden {
                Stdio::inherit()
            } else if forward_host_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .arg("--parent-pid")
            .arg(parent_pid.to_string());
        #[cfg(windows)]
        if hidden {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        sanitize_child_env(&mut command);
        let mut child = command.spawn().map_err(PluginHostClientError::Spawn)?;
        PluginHostProcessManager::global().on_host_spawned(&child, config);

        let stdin = child
            .stdin
            .take()
            .expect("child configured with piped stdin");
        eprintln!("[plugin-bridge] stdin connected");
        let stdout = child
            .stdout
            .take()
            .expect("child configured with piped stdout");

        // Forward the hidden host's stderr inline into our own stderr (out.log),
        // line by line, prefixed `[host]`. Lets the host's editor lifecycle be
        // read straight from the main log when debugging a stuck/blank editor.
        if forward_host_stderr {
            if let Some(host_stderr) = child.stderr.take() {
                let _ = std::thread::Builder::new()
                    .name("plugin-host-stderr".into())
                    .spawn(move || {
                        use std::io::BufRead;
                        let reader = BufReader::new(host_stderr);
                        for line in reader.lines() {
                            match line {
                                Ok(line) => eprintln!("[host] {line}"),
                                Err(_) => break, // pipe closed / host gone
                            }
                        }
                    });
                eprintln!("[plugin-bridge] host stderr forwarding enabled (prefix=[host])");
            }
        }

        let (tx, rx) = crossbeam_channel::unbounded::<ClientEvent>();
        let reader = std::thread::Builder::new()
            .name("plugin-host-events".into())
            .spawn(move || {
                eprintln!("[plugin-bridge] stdout reader started");
                let mut reader = BufReader::new(stdout);
                loop {
                    match ipc::read_frame::<HostEvent, _>(&mut reader) {
                        Ok(Some(event)) => {
                            if tx.send(ClientEvent::Host(event)).is_err() {
                                break; // client dropped
                            }
                        }
                        // EOF or malformed frame → the host is gone/unusable.
                        Ok(None) | Err(_) => {
                            let _ = tx.send(ClientEvent::Disconnected);
                            break;
                        }
                    }
                }
            })
            .expect("spawn plugin-host event reader");

        let mut client = Self {
            child,
            stdin,
            events: rx,
            reader: Some(reader),
            shutdown_started: false,
        };
        client.send(&HostCommand::Hello {
            protocol_version: PROTOCOL_VERSION,
            main_hwnd: config.main_hwnd.map(|h| h as u64),
            session_id: Some(config.project_id.clone()),
        })?;
        if let Some(hwnd) = config.main_hwnd {
            eprintln!(
                "[PluginHost] ipc hello main_hwnd=0x{hwnd:x} session={}",
                config.project_id
            );
        }
        Ok(client)
    }

    /// OS process id of the spawned host (for diagnostics / Task Manager).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Send a `Ping`; the host replies with [`HostEvent::Pong`].
    pub fn ping(&mut self) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::Ping)
    }

    /// Send one command to the host. Maps to `EditorAttached` etc. on the event
    /// channel; this call itself only writes the frame.
    pub fn send(&mut self, cmd: &HostCommand) -> Result<(), PluginHostClientError> {
        ipc::write_frame(&mut self.stdin, cmd).map_err(PluginHostClientError::Spawn)
    }

    /// `format` is the registry's module format label (`"VST3"` / `"VST2"`).
    /// `None` leaves the host to detect it from `plugin_path`.
    pub fn load_plugin(
        &mut self,
        plugin_instance_id: impl Into<String>,
        plugin_path: impl Into<String>,
        class_id: impl Into<String>,
        sample_rate: u32,
        max_block_size: u32,
        format: Option<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::LoadPlugin {
            plugin_instance_id: plugin_instance_id.into(),
            plugin_path: plugin_path.into(),
            class_id: class_id.into(),
            sample_rate,
            max_block_size,
            format,
        })
    }

    pub fn load_builtin_plugin(
        &mut self,
        plugin_instance_id: impl Into<String>,
        plugin_id: impl Into<String>,
        sample_rate: u32,
        max_block_size: u32,
        state_json: Option<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::LoadBuiltinPlugin {
            plugin_instance_id: plugin_instance_id.into(),
            plugin_id: plugin_id.into(),
            sample_rate,
            max_block_size,
            state_json,
        })
    }

    /// `component_id` is the scanner's `au:<type>:<subtype>:<manufacturer>`
    /// identifier; Audio Units have no module path. `state_b64` carries the
    /// persisted ClassInfo blob, applied before the instance reaches the audio
    /// producer (same restore window as the built-ins).
    pub fn load_au_plugin(
        &mut self,
        plugin_instance_id: impl Into<String>,
        component_id: impl Into<String>,
        sample_rate: u32,
        max_block_size: u32,
        state_b64: Option<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::LoadAudioUnit {
            plugin_instance_id: plugin_instance_id.into(),
            component_id: component_id.into(),
            sample_rate,
            max_block_size,
            state_b64,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_editor(
        &mut self,
        plugin_instance_id: impl Into<String>,
        plugin_path: impl Into<String>,
        class_id: impl Into<String>,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::OpenEditorWithParentHwnd {
            track_id: None,
            track_index: None,
            track_name: None,
            plugin_slot_id: None,
            plugin_instance_id: plugin_instance_id.into(),
            plugin_path: plugin_path.into(),
            class_id: class_id.into(),
            plugin_uid: None,
            plugin_display_name: None,
            owner_hwnd: Some(parent_hwnd),
            parent_hwnd,
            width,
            height,
            dpi,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_editor_with_metadata(
        &mut self,
        track_id: impl Into<String>,
        track_index: Option<u32>,
        track_name: Option<String>,
        plugin_slot_id: impl Into<String>,
        plugin_instance_id: impl Into<String>,
        plugin_path: impl Into<String>,
        class_id: impl Into<String>,
        plugin_uid: Option<String>,
        plugin_display_name: impl Into<String>,
        owner_hwnd: u64,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::OpenEditorWithParentHwnd {
            track_id: Some(track_id.into()),
            track_index,
            track_name,
            plugin_slot_id: Some(plugin_slot_id.into()),
            plugin_instance_id: plugin_instance_id.into(),
            plugin_path: plugin_path.into(),
            class_id: class_id.into(),
            plugin_uid,
            plugin_display_name: Some(plugin_display_name.into()),
            owner_hwnd: Some(owner_hwnd),
            parent_hwnd,
            width,
            height,
            dpi,
        })
    }

    pub fn prepare_editor_view(
        &mut self,
        plugin_instance_id: impl Into<String>,
        plugin_path: impl Into<String>,
        class_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::PrepareEditorView {
            plugin_instance_id: plugin_instance_id.into(),
            plugin_path: plugin_path.into(),
            class_id: class_id.into(),
        })
    }

    pub fn confirm_editor_content_ready(
        &mut self,
        plugin_instance_id: impl Into<String>,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::ConfirmEditorContentReady {
            plugin_instance_id: plugin_instance_id.into(),
            parent_hwnd,
            width,
            height,
            dpi,
        })
    }

    pub fn resize_editor(
        &mut self,
        plugin_instance_id: impl Into<String>,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::ResizeEditor {
            plugin_instance_id: plugin_instance_id.into(),
            width,
            height,
            dpi,
        })
    }

    /// Push what the editor's chrome strip should show.
    ///
    /// Only the host-owned-window platforms draw one; elsewhere the host
    /// receives this and does nothing, which is what lets the studio send it
    /// unconditionally instead of branching on the platform at every call site.
    pub fn set_editor_chrome(&mut self, chrome: HostCommand) -> Result<(), PluginHostClientError> {
        debug_assert!(
            matches!(chrome, HostCommand::SetEditorChrome { .. }),
            "set_editor_chrome takes the command it names"
        );
        self.send(&chrome)
    }

    pub fn preview_note_on(
        &mut self,
        plugin_instance_id: impl Into<String>,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::PreviewNoteOn {
            plugin_instance_id: plugin_instance_id.into(),
            channel,
            pitch,
            velocity,
        })
    }

    pub fn preview_note_off(
        &mut self,
        plugin_instance_id: impl Into<String>,
        channel: u8,
        pitch: u8,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::PreviewNoteOff {
            plugin_instance_id: plugin_instance_id.into(),
            channel,
            pitch,
        })
    }

    pub fn preview_control_change(
        &mut self,
        plugin_instance_id: impl Into<String>,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::PreviewControlChange {
            plugin_instance_id: plugin_instance_id.into(),
            channel,
            controller,
            value,
        })
    }

    pub fn preview_all_notes_off(
        &mut self,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::PreviewAllNotesOff {
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    pub fn midi_panic(
        &mut self,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::MidiPanic {
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    /// Stage 1 (shared audio bridge): tell the host the engine-owned sample rate
    /// and block size to follow. Diagnostics-only at this stage.
    pub fn configure_audio_bridge(
        &mut self,
        sample_rate: u32,
        max_block_size: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::ConfigureAudioBridge {
            sample_rate,
            max_block_size,
        })
    }

    /// Stage 1 skeleton: ask the host to process one DSP block. The host replies
    /// with [`crate::ipc::HostEvent::AudioBridgeStatus`] (`dsp_output=pending`
    /// until Stage 3). No shared-memory transport exists yet.
    pub fn process_block_shared(
        &mut self,
        block_id: u64,
        frames: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::ProcessBlockShared { block_id, frames })
    }

    /// Prepare plugin DSP at the engine-owned sample rate / block size.
    pub fn prepare_processing(
        &mut self,
        plugin_instance_id: impl Into<String>,
        sample_rate: u32,
        max_block_size: u32,
        input_channels: u32,
        output_channels: u32,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::PrepareProcessing {
            plugin_instance_id: plugin_instance_id.into(),
            sample_rate,
            max_block_size,
            input_channels,
            output_channels,
        })
    }

    /// Stage 2: ask the host to map the engine-created named shared-memory audio
    /// region. The host replies [`crate::ipc::HostEvent::SharedAudioAttached`].
    pub fn attach_shared_audio(
        &mut self,
        name: String,
        bytes: u64,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::AttachSharedAudio {
            name,
            bytes,
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    /// Ask the host for the instance's current VST3 state. The host replies
    /// [`crate::ipc::HostEvent::PluginState`] (poll via [`Self::try_recv_event`]).
    pub fn get_plugin_state(
        &mut self,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::GetPluginState {
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    /// Restore a previously captured VST3 state (base64 blobs from a project
    /// file). The host replies [`crate::ipc::HostEvent::PluginStateSet`].
    pub fn set_plugin_state(
        &mut self,
        plugin_instance_id: impl Into<String>,
        component_b64: String,
        controller_b64: String,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::SetPluginState {
            plugin_instance_id: plugin_instance_id.into(),
            component_b64,
            controller_b64,
        })
    }

    /// Request the VST3 parameter list for a loaded instance. Reply:
    /// [`crate::ipc::HostEvent::PluginParameters`] (poll via [`Self::try_recv_event`]).
    pub fn get_plugin_parameters(
        &mut self,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::GetPluginParameters {
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    pub fn close_editor(
        &mut self,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::CloseEditor {
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    pub fn unload_plugin(
        &mut self,
        plugin_instance_id: impl Into<String>,
    ) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::UnloadPlugin {
            plugin_instance_id: plugin_instance_id.into(),
        })
    }

    /// Ask the host to shut down gracefully. Best-effort; `Drop` still enforces
    /// teardown.
    pub fn shutdown(&mut self) -> Result<(), PluginHostClientError> {
        self.send(&HostCommand::Shutdown)
    }

    /// Non-blocking poll for the next event. `None` when nothing is queued.
    pub fn try_recv_event(&self) -> Option<ClientEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            // The reader thread already pushed `Disconnected` before exiting,
            // so a closed channel just means nothing more is coming.
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// `Some(true)` if the host has exited, `Some(false)` if still running,
    /// `None` if the status could not be queried.
    pub fn has_exited(&mut self) -> Option<bool> {
        match self.child.try_wait() {
            Ok(Some(_)) => Some(true),
            Ok(None) => Some(false),
            Err(_) => None,
        }
    }

    /// Force-terminate the host process (after graceful shutdown times out).
    pub fn force_kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    /// Block until the host process has exited.
    pub fn wait_for_exit(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// Join the stdout reader thread after the host has exited.
    pub fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for PluginHostClient {
    fn drop(&mut self) {
        plugin_host_lifecycle::shutdown_host_client(self);
        let _ = self.wait_for_exit();
        self.join_reader();
    }
}
